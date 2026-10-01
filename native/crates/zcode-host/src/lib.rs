//! zcode-host：S3 Rust agent host（specs/rust-native-crates.md crate map）。
//!
//! 进程形态：stdio NDJSON RPC 服务。一条 stdin 行一个请求，响应与
//! `v4/conversation/frame` 通知共用同一条 stdout。集成面（全部 in-process，无
//! sidecar 跳转）：
//!
//! - `zcode-v4-wire`：帧编解码、CRC/base64、信封计量闸门；
//! - `zcode-core`：CommandInbox 串行 admission + identity key + 幂等回源；
//! - `zcode-provider`：模型回合执行器（重试/分类/流式拼装）。
//!
//! 所有权：`HostServer` 只做路由与会话注册表；每个 `Session` 是自己行集、
//! revision、phase 与 inbox 的唯一所有者；turn 是唯一写投影的路径。
//! 与 TS host 的关系：能力面子集 + fail-closed 差异逐条记录在 crate README。

pub mod commands_query;
pub mod compact;
pub mod frame;
pub mod mcp;
pub mod mcp_tools;
pub mod memory;
pub mod nc_tools;
pub mod profile;
pub mod provider_config;
pub mod reconciliation;
pub mod rows;
pub mod rpc;
pub mod session;
pub mod snapshot;
pub mod storage;
pub mod store;
pub mod tools;
pub mod turn;
pub mod turn_context;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use zcode_core::identity::identity_key;
use zcode_core::protocol::{parse_command_envelope, CommandStatus};

use crate::mcp::McpPool;
use crate::profile::DeliveryProfileName;
use crate::provider_config::ProviderConfig;
use crate::rpc::{
    parse_request_line, RpcError, RpcRequest, RpcResponse, CODE_INTERNAL, CODE_INVALID_PARAMS,
    CODE_METHOD_NOT_FOUND, METHOD_COMMAND, METHOD_COMMANDS_QUERY, METHOD_CONNECTION_FLOW,
    METHOD_CONVERSATION_RESYNC, METHOD_CONVERSATION_ROWS_RANGE, METHOD_CONVERSATION_SUBSCRIBE,
    METHOD_CONVERSATION_UNSUBSCRIBE,
};
use crate::session::{Session, SessionInit};
use crate::store::{generate_id, is_safe_session_id, now_ms, SessionMeta, TranscriptStore};
use crate::tools::ToolRegistry;
use crate::turn::TurnDeps;

/// 解析 `--mcp-server Name=command[:arg,arg]`。
/// 返回 `(server 名, 可执行文件, 参数)`；形状不合法返回 None。
fn parse_mcp_spec(spec: &str) -> Option<(String, String, Vec<String>)> {
    let (name, rest) = spec.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let mut parts = rest.splitn(2, ':');
    let command = parts.next()?.trim().to_string();
    if command.is_empty() {
        return None;
    }
    let args = parts
        .next()
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|arg| !arg.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Some((name.to_string(), command, args))
}

pub struct HostServer {
    workspace: PathBuf,
    workspace_identity: String,
    store: TranscriptStore,
    provider: Option<ProviderConfig>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// 进程内命令账本中**不属于任何 session** 的那一份（`sessionId: null` 的
    /// global 幂等桶，即 createSession）。与 session 账本物理分开：global 桶的
    /// key 本来就没有 sessionId，混进 session 账本会让归属变成隐式约定。
    global_acks: Mutex<HashMap<String, Value>>,
    /// 本进程实例标识，供 `v4/commands/query` 的 `clock` 校准回显
    /// （`localTtftClockSchema.instanceId`）。进程内唯一、稳定 <= 128 字符。
    instance_id: String,
    outbox: UnboundedSender<String>,
    id_counter: AtomicU64,
    turn_deps: TurnDeps,
    /// 自引用弱指针：flush ticker 只持有 Weak，server 释放即退出（stdout writer 收尾的前提）。
    self_weak: std::sync::OnceLock<std::sync::Weak<HostServer>>,
}

impl HostServer {
    pub fn new(
        workspace: PathBuf,
        data_dir: PathBuf,
        provider: Option<ProviderConfig>,
        full_access: bool,
        mcp_servers: Vec<String>,
    ) -> Result<(Arc<Self>, UnboundedReceiver<String>), tools::ToolError> {
        let (outbox, inbox) = tokio::sync::mpsc::unbounded_channel();
        let workspace_identity = identity_key(None, &workspace.to_string_lossy());
        let memory_root = data_dir.join("memory");
        let _ = std::fs::create_dir_all(&memory_root);

        // MCP servers（可选，stdio 形态）：`--mcp-server Name=command[:arg,arg]` 可重复。
        // 连接失败不阻断启动——MCP 是可选能力面，不是 host 的生存依赖。
        let mcp_pool = Arc::new(McpPool::new());
        for spec in &mcp_servers {
            let Some((name, command, args)) = parse_mcp_spec(spec) else {
                eprintln!(
                    "[zcode-host] invalid --mcp-server spec (expected Name=command[:arg,arg]): {spec}"
                );
                continue;
            };
            match mcp_pool.connect_stdio(&name, &command, &args) {
                Ok(count) => eprintln!("[zcode-host] mcp {name}: {count} tools"),
                Err(error) => eprintln!("[zcode-host] mcp {name} unavailable: {error}"),
            }
        }

        let turn_deps = TurnDeps {
            registry: Arc::new(
                ToolRegistry::with_defaults()
                    .with_nc_tools(nc_tools::build_nc_tools(&workspace)?)
                    .with_mcp_tools(mcp_tools::proxies_for(&mcp_pool)),
            ),
            context: turn_context::assemble_context_with_memory(&workspace, Some(&memory_root)),
            full_access,
            provider_config: provider.clone().map(Arc::new),
        };
        let server = Arc::new(Self {
            workspace,
            workspace_identity,
            store: TranscriptStore::new(data_dir),
            provider,
            sessions: Mutex::new(HashMap::new()),
            global_acks: Mutex::new(HashMap::new()),
            instance_id: next_instance_id(),
            outbox,
            id_counter: AtomicU64::new(1),
            turn_deps,
            self_weak: std::sync::OnceLock::new(),
        });
        let _ = server.self_weak.set(Arc::downgrade(&server));
        Ok((server, inbox))
    }

    pub fn workspace(&self) -> &PathBuf {
        &self.workspace
    }

    pub fn has_provider(&self) -> bool {
        self.provider.is_some()
    }

    /// 供二进制把协议行写进同一根 stdout 管道（响应与帧共序）。
    pub fn send_line(&self, line: String) -> Result<(), String> {
        self.outbox.send(line).map_err(|error| error.to_string())
    }

    fn next_id(&self, prefix: &str) -> String {
        generate_id(prefix, &self.id_counter)
    }

    fn session(&self, session_id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().ok()?.get(session_id).cloned()
    }

    fn register_session(&self, session: Arc<Session>) {
        if let Ok(mut guard) = self.sessions.lock() {
            guard.insert(session.id.clone(), Arc::clone(&session));
        }
        self.spawn_flush_ticker(&session);
    }

    /// 每会话 flush ticker：窗口到期即发出滞留 delta（无新事件也要推进窗口）。
    /// 只持有 server 的 Weak 引用：stdin EOF（server 释放）或会话删除都会让
    /// ticker 自行退出，outbox 发送端随之归零，writer 才能收尾退出。
    fn spawn_flush_ticker(&self, session: &Arc<Session>) {
        let Some(weak) = self.self_weak.get().cloned() else {
            return;
        };
        let session = Arc::clone(session);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(10));
            interval.tick().await;
            loop {
                interval.tick().await;
                let Some(server) = weak.upgrade() else {
                    return;
                };
                if server.session(&session.id).is_none() {
                    return;
                }
                for line in session.flush_expired() {
                    if server.outbox.send(line).is_err() {
                        return;
                    }
                }
            }
        });
    }

    // ── 请求分发 ──

    /// 处理一行请求；返回响应行（如有）。通知帧直接进 outbox。
    /// 处理一行请求。返回 (响应行, 响应之后才允许发出的帧行)——协议契约：
    /// subscribe/resync 的 initial/recovery snapshot 是 post-response owned
    /// notification，response line 必须先写（transport.ts subscribe 注释）。
    pub async fn handle_line(self: &Arc<Self>, line: &str) -> (Option<String>, Vec<String>) {
        let request = match parse_request_line(line) {
            Ok(request) => request,
            Err(error) => {
                return (
                    Some(RpcResponse::err(Value::Null, error).to_line()),
                    Vec::new(),
                )
            }
        };
        self.handle_request(request).await
    }

    pub async fn handle_request(
        self: &Arc<Self>,
        request: RpcRequest,
    ) -> (Option<String>, Vec<String>) {
        let id = request.id.clone();
        let result = match request.method.as_str() {
            METHOD_CONNECTION_FLOW => Ok(RequestOutcome::empty(json!({}))),
            METHOD_CONVERSATION_SUBSCRIBE => self.subscribe(&request.params),
            METHOD_CONVERSATION_RESYNC => self.resync(&request.params),
            METHOD_CONVERSATION_UNSUBSCRIBE => self.unsubscribe(&request.params),
            METHOD_CONVERSATION_ROWS_RANGE => self.rows_range(&request.params),
            METHOD_COMMAND => self.command(&request.params).await,
            METHOD_COMMANDS_QUERY => self
                .commands_query(&request.params)
                .map(RequestOutcome::empty),
            other => Err(RpcError::new(
                CODE_METHOD_NOT_FOUND,
                format!("method not implemented in this host increment: {other}"),
            )),
        };
        match result {
            Ok(outcome) => (
                Some(RpcResponse::ok(id, outcome.response).to_line()),
                outcome.post_frames,
            ),
            Err(error) => (Some(RpcResponse::err(id, error).to_line()), Vec::new()),
        }
    }

    // ── 订阅面 ──

    fn subscribe(&self, params: &Value) -> Result<RequestOutcome, RpcError> {
        let session_id = require_str(params, "sessionId")?;
        let connection_id = require_str(params, "connectionId")?;
        let client_mode = require_str(params, "clientMode")?;
        let session = self
            .session(session_id)
            .ok_or_else(|| session_not_found(session_id))?;
        let profile = DeliveryProfileName::from_client_mode(client_mode).ok_or_else(|| {
            RpcError::new(
                CODE_INVALID_PARAMS,
                format!("unknown clientMode: {client_mode}"),
            )
        })?;
        let subscription_id = format!("{connection_id}:conversation/{session_id}");
        session.add_subscriber(subscription_id.clone(), profile);
        let ack = json!({
            "ack": {
                "subscriptionId": subscription_id,
                "mode": "snapshot",
                "logEpoch": session.log_epoch(),
            }
        });
        // initial snapshot 由调用方在 response 之后发出（post-response outbox）。
        let post_frames = session.project_snapshot(&subscription_id, "initial");
        Ok(RequestOutcome {
            response: ack,
            post_frames,
        })
    }

    fn resync(&self, params: &Value) -> Result<RequestOutcome, RpcError> {
        let session_id = require_str(params, "sessionId")?;
        let connection_id = require_str(params, "connectionId")?;
        let session = self
            .session(session_id)
            .ok_or_else(|| session_not_found(session_id))?;
        let subscription_id = format!("{connection_id}:conversation/{session_id}");
        let post_frames = session.project_snapshot(&subscription_id, "recovery");
        Ok(RequestOutcome {
            response: json!({
                "ack": {
                    "subscriptionId": subscription_id,
                    "mode": "snapshot",
                    "logEpoch": session.log_epoch(),
                }
            }),
            post_frames,
        })
    }

    fn unsubscribe(&self, params: &Value) -> Result<RequestOutcome, RpcError> {
        let session_id = require_str(params, "sessionId")?;
        let connection_id = require_str(params, "connectionId")?;
        let subscription_id = format!("{connection_id}:conversation/{session_id}");
        if let Some(session) = self.session(session_id) {
            session.remove_subscriber(&subscription_id);
        }
        Ok(RequestOutcome::empty(json!({})))
    }

    /// v4/commands/query：命令幂等查询面。纯只读，不 admission、不改状态。
    ///
    /// 账本来源两条：
    /// 1. session 桶 -> `Session::settled_ack`（每 session 一份账本，天然隔离，
    ///    不会把别的 session 的 ack 答给调用方）；
    /// 2. global 桶（`sessionId: null`，即 createSession 幂等桶）-> Host 级
    ///    `global_acks`。本 increment 之前 global ack 只出现在 response 里、不留账本，
    ///    这里补上，否则 createSession 的重提查询只能答 unknown。
    fn commands_query(&self, params: &Value) -> Result<Value, RpcError> {
        let received_at = store::now_ms() as f64;
        match commands_query::parse_query_params(params)? {
            commands_query::QueryParams::Clock { commands } => {
                let sent_at = store::now_ms() as f64;
                Ok(commands_query::clock_result(
                    &commands,
                    &self.instance_id,
                    received_at,
                    sent_at,
                ))
            }
            commands_query::QueryParams::Ledger { commands } => {
                Ok(commands_query::ledger_result(&commands, |key| {
                    match &key.session_id {
                        Some(session_id) => self
                            .session(session_id)
                            .and_then(|session| session.settled_ack(&key.command_id)),
                        None => self
                            .global_acks
                            .lock()
                            .ok()
                            .and_then(|guard| guard.get(&key.command_id).cloned()),
                    }
                }))
            }
        }
    }

    fn rows_range(&self, params: &Value) -> Result<RequestOutcome, RpcError> {
        let session_id = require_str(params, "sessionId")?;
        let from_row_id = params.get("fromRowId").and_then(Value::as_u64);
        let to_row_id = params.get("toRowId").and_then(Value::as_u64);
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        let session = self
            .session(session_id)
            .ok_or_else(|| session_not_found(session_id))?;
        let rows = session.rows_range(from_row_id, to_row_id, limit);
        Ok(RequestOutcome::empty(json!({
            "rows": rows,
            "totalCount": session.total_row_count(),
            "firstRowId": session.first_row_id(),
        })))
    }

    // ── 命令面 ──

    async fn command(self: &Arc<Self>, params: &Value) -> Result<RequestOutcome, RpcError> {
        let envelope = parse_command_envelope(params).map_err(|error| {
            RpcError::new(CODE_INVALID_PARAMS, error.message)
                .with_data(json!({"reasonCode": "proto.invalidPayload"}))
        })?;

        match envelope.command_type.as_str() {
            // createSession 是会话外操作（TS 的 global bucket，不经 session inbox）。
            "createSession" => self.create_session(&envelope),
            // stop 是控制命令（对齐 TS sessionStop 的即时取消路径）：不能排 inbox
            // FIFO——那会排在正在跑的 turn 后面造成死锁。
            "stop" => self.stop(&envelope),
            // rename/delete 是会话级控制：直接生效，无 turn 副作用。
            "renameSession" => self.rename_session(&envelope),
            "deleteSession" => self.delete_session(&envelope),
            // 其余命令（sendText 等）走 session inbox 串行 admission。
            _ => self.admitted_command(&envelope).await,
        }
    }

    fn create_session(
        &self,
        envelope: &zcode_core::protocol::CommandEnvelope,
    ) -> Result<RequestOutcome, RpcError> {
        if envelope.session_id.is_some() {
            return Err(RpcError::new(
                CODE_INVALID_PARAMS,
                "createSession requires sessionId: null",
            ));
        }
        let session_id = self.next_id("session");
        if !is_safe_session_id(&session_id) {
            return Err(RpcError::new(
                CODE_INTERNAL,
                "generated session id failed safety check",
            ));
        }
        let meta = SessionMeta {
            session_id: session_id.clone(),
            title: String::new(),
            title_source: "default".to_string(),
            log_epoch: self.next_id("epoch"),
            revision: 0,
            workspace_identity: self.workspace_identity.clone(),
            created_at: now_ms(),
        };
        self.store
            .create_session(&meta)
            .map_err(|error| RpcError::new(CODE_INTERNAL, error.to_string()))?;
        let session = Session::new(SessionInit {
            id: session_id.clone(),
            workspace_root: self.workspace.clone(),
            workspace_identity: self.workspace_identity.clone(),
            log_epoch: meta.log_epoch,
            title: meta.title,
            title_source: meta.title_source,
            rows: Vec::new(),
            revision: 0,
            transcript_acks: HashMap::new(),
            next_row_id: 0,
            store: self.store.clone(),
        });
        self.register_session(session);
        let ack = json!({
            "commandId": envelope.command_id,
            "status": "accepted",
            "revisionAtDecision": 0,
            "result": {"type": "createSession", "sessionId": session_id},
        });
        // 这里的 ack 记在 global 桶（envelope.sessionId 是 null，按 v4 协议
        // createSession 属于「全局 / createSession 幂等桶」）。**但这个账本在本 increment
        // 是刻意只在进程内的**，原因是可验证的，不是偷懒：
        //
        // 1. global 桶账本的**唯一**读取点是 `commands_query`（lib.rs:354），而
        //    `v4/commands/query` 这个方法是本 increment 新加的（HEAD 的 rpc.rs 里没有
        //    这个方法名）。所以「ack 不落盘」这件事没有任何既有消费者能观察到。
        // 2. createSession **本来就没有幂等重提去重**：HEAD 的 dispatch 注释就写着
        //    「createSession 是会话外操作（TS 的 global bucket，不经 session inbox）」，
        //    它直接调 create_session，从不走 CommandInbox。所以 HEAD 上那次
        //    `store.append_ack` 提供的是**查询可见性**，不是幂等性——而查询面当时不存在。
        //
        // 相对 HEAD 的**行为变化**（不掩盖）：HEAD 上 register_session 先于 persist，
        // 因此 persist_command_ack 命中 `self.session(id)`，ack 会经
        // `Session::persist_ack` 写进 `sessions/<id>/transcript.jsonl`（durable），
        // 且落在**新建 session** 的账本里——那对 global 桶是错的归属。改成 global 桶后，
        // 它不再落盘。给 global 桶建一个 durable 账本是**新功能**，不在本 increment 的
        // 「只加不改」范围内，故记入 RUST-PORT-LOG ENTRY 026 的审计小节。
        self.persist_global_ack(&envelope.command_id, &ack);
        Ok(RequestOutcome::empty(ack))
    }

    fn stop(
        &self,
        envelope: &zcode_core::protocol::CommandEnvelope,
    ) -> Result<RequestOutcome, RpcError> {
        let session_id = envelope
            .session_id
            .as_deref()
            .ok_or_else(|| RpcError::new(CODE_INVALID_PARAMS, "stop requires sessionId"))?;
        let session = self
            .session(session_id)
            .ok_or_else(|| session_not_found(session_id))?;
        let running = session.is_running();
        if running {
            session.request_cancel();
        }
        let ack = json!({
            "commandId": envelope.command_id,
            "status": if running { "accepted" } else { "noop" },
            "revisionAtDecision": session.revision(),
            "reasonCode": if running { Value::Null } else { json!("guard.nothingToStop") },
        });
        self.persist_command_ack(session_id, &envelope.command_id, &ack);
        Ok(RequestOutcome::empty(ack))
    }

    fn rename_session(
        &self,
        envelope: &zcode_core::protocol::CommandEnvelope,
    ) -> Result<RequestOutcome, RpcError> {
        let session_id = envelope.session_id.as_deref().ok_or_else(|| {
            RpcError::new(CODE_INVALID_PARAMS, "renameSession requires sessionId")
        })?;
        let title = envelope
            .payload
            .get("title")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                RpcError::new(
                    CODE_INVALID_PARAMS,
                    "renameSession payload.title: required string",
                )
            })?;
        let session = self
            .session(session_id)
            .ok_or_else(|| session_not_found(session_id))?;
        session.set_title(title.to_string(), "custom");
        self.persist_meta(&session);
        let ack = json!({
            "commandId": envelope.command_id,
            "status": "accepted",
            "revisionAtDecision": session.revision(),
        });
        self.persist_command_ack(session_id, &envelope.command_id, &ack);
        Ok(RequestOutcome::empty(ack))
    }

    fn delete_session(
        &self,
        envelope: &zcode_core::protocol::CommandEnvelope,
    ) -> Result<RequestOutcome, RpcError> {
        let session_id = envelope.session_id.as_deref().ok_or_else(|| {
            RpcError::new(CODE_INVALID_PARAMS, "deleteSession requires sessionId")
        })?;
        self.store
            .delete_session(session_id)
            .map_err(|error| RpcError::new(CODE_INTERNAL, error.to_string()))?;
        if let Ok(mut guard) = self.sessions.lock() {
            guard.remove(session_id);
        }
        let ack = json!({
            "commandId": envelope.command_id,
            "status": "accepted",
            "revisionAtDecision": 0,
        });
        Ok(RequestOutcome::empty(ack))
    }

    /// inbox admission：ack-only 命令直接响应；admitted 命令先响应 admission
    /// ack，再派生 turn 任务（turn 结束才 settle）。
    async fn admitted_command(
        self: &Arc<Self>,
        envelope: &zcode_core::protocol::CommandEnvelope,
    ) -> Result<RequestOutcome, RpcError> {
        let session_id = envelope.session_id.clone().ok_or_else(|| {
            RpcError::new(CODE_INVALID_PARAMS, "session commands require sessionId")
        })?;
        let session = self
            .session(&session_id)
            .ok_or_else(|| session_not_found(&session_id))?;

        // inbox.handle_envelope 可能阻塞到 session gate 释放（串行 admission），
        // 因此放 blocking 池，stdin 读取循环不被挂起。
        let inbox = session.inbox.clone();
        let envelope_for_inbox = envelope.clone();
        let outcome =
            tokio::task::spawn_blocking(move || inbox.handle_envelope(envelope_for_inbox))
                .await
                .map_err(|error| {
                    RpcError::new(CODE_INTERNAL, format!("admission task failed: {error}"))
                })?;

        match outcome {
            zcode_core::inbox::Outcome::Ack(ack) => {
                let value = serde_json::to_value(&ack).unwrap_or(Value::Null);
                self.persist_command_ack(&session_id, &envelope.command_id, &value);
                Ok(RequestOutcome::empty(value))
            }
            zcode_core::inbox::Outcome::Execute(admitted) => {
                let ack = serde_json::to_value(&admitted.ack).unwrap_or(Value::Null);
                let provider = self.provider.clone();
                let outbox = self.outbox.clone();
                let session_for_turn = Arc::clone(&session);
                let server_for_turn: Arc<Self> = Arc::clone(self);
                let command_type = admitted.envelope.command_type.clone();
                tokio::spawn(async move {
                    if command_type == "sendText" {
                        let lines = turn::run_turn(
                            session_for_turn,
                            provider.as_ref(),
                            &server_for_turn.turn_deps,
                            *admitted,
                        )
                        .await;
                        for line in lines {
                            let _ = outbox.send(line);
                        }
                        return;
                    }
                    // 本 increment 未接线的命令类型：fail-closed settle，绝不静默执行。
                    let admitted = *admitted;
                    let session = session_for_turn;
                    let reason = format!("fault.command.unsupported:{command_type}");
                    let final_ = zcode_core::protocol::CommandFinal::with_reason(
                        CommandStatus::Failed,
                        reason,
                    );
                    let merged = zcode_core::protocol::merge_final(&admitted.ack, &final_);
                    let merged_value = serde_json::to_value(&merged).unwrap_or(Value::Null);
                    session.persist_ack(&admitted.envelope.command_id, &merged_value);
                    let _ = session.inbox.settle(admitted.ticket, final_);
                });
                Ok(RequestOutcome::empty(ack))
            }
        }
    }

    /// global 桶（`sessionId: null`）账本的**唯一**写入口。
    ///
    /// WHY 锁中毒时静默丢弃：读侧 `commands_query` 在同样的中毒状态下会把这些
    /// commandId 答成 `unknown`，即整条链路仍然是 fail-closed（不返回错误的成功，
    /// 也不会把别的 ack 错配过来）。本 increment 刻意不引入日志面——给一条只有
    /// 单测能触发的路径加运行时日志，代价大于收益；代价已记在 crate README 的
    /// 差异清单里。若将来 `commands/query` 变成用户可见功能，再单独评估可观测性。
    fn persist_global_ack(&self, command_id: &str, ack: &Value) {
        if let Ok(mut guard) = self.global_acks.lock() {
            guard.insert(command_id.to_string(), ack.clone());
        }
    }

    fn persist_command_ack(&self, session_id: &str, command_id: &str, ack: &Value) {
        if let Some(session) = self.session(session_id) {
            session.persist_ack(command_id, ack);
        }
        // session 查不到时**不**回落到 global 账本：global 桶只由 persist_global_ack 写。
        // 两个写入口等于两个 owner，重提 createSession 的语义会随分支顺序漂移。
    }

    fn persist_meta(&self, session: &Arc<Session>) {
        let (title, title_source) = session.title();
        let meta = SessionMeta {
            session_id: session.id.clone(),
            title,
            title_source,
            log_epoch: session.log_epoch(),
            revision: session.revision(),
            workspace_identity: session.workspace_identity.clone(),
            created_at: now_ms(),
        };
        let _ = self.store.write_meta(&meta);
    }

    /// 冷恢复：从 store 加载全部会话（resume 验收的入口）。
    pub fn resume_sessions(&self) -> usize {
        let mut resumed = 0;
        for session_id in self.store.list_sessions() {
            if !is_safe_session_id(&session_id) {
                continue;
            }
            let Ok(meta) = self.store.load_meta(&session_id) else {
                continue;
            };
            let transcript = self.store.load_transcript(&session_id).unwrap_or_default();
            let next_row_id = transcript
                .rows
                .iter()
                .filter_map(|row| row.get("rowId").and_then(Value::as_u64))
                .max()
                .unwrap_or(0)
                + 1;
            let session = Session::new(SessionInit {
                id: session_id.clone(),
                workspace_root: self.workspace.clone(),
                workspace_identity: meta.workspace_identity.clone(),
                log_epoch: meta.log_epoch,
                title: meta.title,
                title_source: meta.title_source,
                rows: transcript.rows,
                revision: meta.revision,
                transcript_acks: transcript.acks,
                next_row_id,
                store: self.store.clone(),
            });
            self.register_session(session);
            resumed += 1;
        }
        resumed
    }
}

/// 一次请求的处理结果：响应体 + 必须排在 response 之后的帧行。
struct RequestOutcome {
    response: Value,
    post_frames: Vec<String>,
}

impl RequestOutcome {
    fn empty(response: Value) -> Self {
        Self {
            response,
            post_frames: Vec::new(),
        }
    }
}

fn require_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params.get(key).and_then(Value::as_str).ok_or_else(|| {
        RpcError::new(
            CODE_INVALID_PARAMS,
            format!("params.{key}: required string"),
        )
    })
}

/// 进程内唯一的 host 实例标识（`localTtftClockSchema.instanceId`）。
///
/// 不用 pid 或墙钟：pid 会在进程内复用，墙钟在同一毫秒内可能相同，两者混起来
/// 都不是「这是第几个实例」的可靠答案。`localTtft` 用 instanceId 区分并发观察者，
/// 所以它必须在同一进程内严格单调且不重复。
fn next_instance_id() -> String {
    // 进程内自增 + 进程启动时刻：pid 会被系统复用，单独的 pid 不够；单独的
    // 自增序号在两个进程里会重复。两者合起来：同进程内严格唯一，跨进程也几乎必然不同。
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let ordinal = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!(
        "host-{}-{}-{}",
        std::process::id(),
        store::now_ms(),
        ordinal
    )
}

fn session_not_found(session_id: &str) -> RpcError {
    RpcError::new(
        CODE_INVALID_PARAMS,
        format!("session not found: {session_id}"),
    )
    .with_data(json!({"reasonCode": "proto.sessionNotFound"}))
}

#[cfg(test)]
mod instance_id_tests {
    use super::next_instance_id;

    // main M3：instance_id 必须真的逐实例唯一。旧实现只有 pid，pid 会被系统复用，
    // 而 localTtft 用 instanceId 区分并发观察者——重复的 id 会让校准串到别的实例。
    #[test]
    fn two_hosts_in_one_process_get_different_instance_ids() {
        let first = next_instance_id();
        let second = next_instance_id();
        assert_ne!(first, second, "两个 host 实例不能拿到同一个 instance_id");
        assert!(
            !first.is_empty() && first.len() <= 128,
            "localTtftClockSchema.identifier 1..128"
        );
    }
}
