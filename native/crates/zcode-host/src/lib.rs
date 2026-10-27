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

pub mod branch;
mod commands;
pub mod commands_query;
pub mod compact;
pub mod content_hash;
pub mod conversation_reads;
pub mod file_ledger;
pub mod frame;
pub mod goal;
pub mod goal_loop;
pub mod goal_verifier;
mod host_interactions;
mod host_queue;
pub mod interaction;
pub mod mcp;
pub mod mcp_tools;
pub mod memory;
pub mod nc_tools;
pub mod profile;
pub mod provider_config;
pub mod queue;
pub mod reconciliation;
pub mod rewind;
pub mod rows;
pub mod rpc;
pub mod session;
pub mod sessions;
pub mod single_shot;
pub mod snapshot;
pub mod storage;
pub mod store;
pub mod tools;
pub mod turn;
pub mod turn_context;
pub mod workspace_hooks;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use zcode_core::identity::identity_key;
use zcode_core::protocol::parse_command_envelope;

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
    /// workspace hook 信任台账（`<data-dir>/security/workspace-hook-trust-v1.json`）。
    /// host 级共享：同一工作区的多个会话看到同一份台账。
    hook_store: Arc<crate::workspace_hooks::TrustStore>,
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
        let hook_store = Arc::new(crate::workspace_hooks::TrustStore::new(
            data_dir
                .join("security")
                .join(crate::workspace_hooks::TRUST_STORE_FILE),
        ));

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
            hook_store,
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
        // 帧的唯一有序出口：turn 期间的帧由 Session 直接写入 outbox（eager），
        // 与 ticker 共用同一 FIFO，避免「延后发送的帧」越过后续帧。
        session.set_frame_sink(self.outbox.clone());
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
                for line in session.expire_hook_review(now_ms()) {
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
            "v4/conversation/fileChanges" => self
                .conversation_file_changes(&request.params)
                .map(RequestOutcome::empty),
            "v4/conversation/fileRewindPreview" => self
                .conversation_file_rewind_preview(&request.params)
                .map(RequestOutcome::empty),
            "v4/conversation/usage" => self
                .conversation_usage(&request.params)
                .map(RequestOutcome::empty),
            METHOD_COMMAND => self.command(&request.params).await,
            METHOD_COMMANDS_QUERY => self
                .commands_query(&request.params)
                .map(RequestOutcome::empty),
            other => {
                // 已知但未接线的方法必须说清楚**缺什么**（清单测试保证表不为空且精确）；
                // 未知方法保持通用 not-found，不把内部清单当公开词表。
                let missing = rpc::V4_METHODS_UNIMPLEMENTED
                    .iter()
                    .find(|(name, _)| *name == other)
                    .map(|(_, reason)| *reason);
                let message = match missing {
                    Some(reason) => {
                        format!("{other} is not wired in this host increment: missing {reason}")
                    }
                    None => format!("method not implemented in this host increment: {other}"),
                };
                Err(RpcError::new(CODE_METHOD_NOT_FOUND, message))
            }
        };
        let session = request
            .params
            .get("sessionId")
            .and_then(Value::as_str)
            .and_then(|session_id| self.session(session_id));
        match result {
            Ok(outcome) => {
                let response_line = RpcResponse::ok(id, outcome.response).to_line();
                if let Some(session) = session {
                    if session.has_frame_sink() {
                        // 会话请求：response 与帧写入同一 FIFO（response 先入），
                        // 与 turn 的 eager 帧共序，杜绝「帧先于响应/帧序颠倒」。
                        session.enqueue_line(response_line.clone());
                        for frame in outcome.post_frames {
                            session.enqueue_line(frame);
                        }
                        return (None, Vec::new());
                    }
                }
                (Some(response_line), outcome.post_frames)
            }
            Err(error) => {
                let line = RpcResponse::err(id, error).to_line();
                if let Some(session) = session {
                    if session.enqueue_line(line.clone()) {
                        return (None, Vec::new());
                    }
                }
                (Some(line), Vec::new())
            }
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

        // 路由事实只有 `commands::route` 一处；本函数把位点翻译成调用，不承载命令语义。
        // 这样能力面（snapshot::availability）与执行面读同一张表，不可能对同一个命令
        // 一个显示 allowed、另一个 fail-closed。
        match commands::route(&envelope.command_type) {
            commands::Route::CreateSession => self.create_session(&envelope),
            commands::Route::Stop => self.stop(&envelope),
            commands::Route::RenameSession => self.rename_session(&envelope),
            commands::Route::DeleteSession => self.delete_session(&envelope),
            commands::Route::ResolveInteraction => {
                let session = self.queue_session(&envelope)?;
                Ok(self.resolve_interaction_command(&session, &envelope))
            }
            commands::Route::SnoozeInteraction => {
                let session = self.queue_session(&envelope)?;
                Ok(self.snooze_interaction_command(&session, &envelope))
            }
            commands::Route::QueueMutation(mutation) => {
                let session = self.queue_session(&envelope)?;
                Ok(match mutation {
                    commands::QueueMutation::Edit => self.queue_edit_command(&session, &envelope),
                    commands::QueueMutation::Reorder => {
                        self.queue_reorder_command(&session, &envelope)
                    }
                    commands::QueueMutation::Delete => {
                        self.queue_delete_command(&session, &envelope)
                    }
                    commands::QueueMutation::SendNow => {
                        self.queue_send_now_command(&session, &envelope)
                    }
                })
            }
            // busy / 已有排队 → 入队（FIFO，不插队）；否则走正常 admission。
            commands::Route::SendText => {
                let session = self.queue_session(&envelope)?;
                if HostServer::should_queue_input(&session) {
                    let text = envelope
                        .payload
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let outcome = self.enqueue_text_input(&session, &envelope, text);
                    if !session.is_running() && session.queue_auto_drain() {
                        let server = Arc::clone(self);
                        let session = Arc::clone(&session);
                        tokio::spawn(async move {
                            server.drain_queue(session).await;
                        });
                    }
                    return Ok(outcome);
                }
                self.admitted_command(&envelope).await
            }
            // sendGoalCommand：提交期校验（空目标 / 4000 字上限 / plan 互斥）不通过
            // 就直接拒绝——不能先入队再在执行时拒绝，那会让 UI 先看到"已接受"。
            commands::Route::SendGoalCommand => {
                let session = self.queue_session(&envelope)?;
                if let Err(final_) = commands::goal::validate_goal_submission(&envelope.payload) {
                    let ack = json!({
                        "commandId": envelope.command_id,
                        "status": "rejected",
                        "revisionAtDecision": session.revision(),
                        "reasonCode": final_.reason_code,
                        "message": final_.message,
                    });
                    self.persist_command_ack(&session.id, &envelope.command_id, &ack);
                    return Ok(RequestOutcome::empty(ack));
                }
                if HostServer::should_queue_input(&session) {
                    let objective = envelope
                        .payload
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    let text = envelope
                        .payload
                        .get("displayText")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("/goal {objective}"));
                    let outcome = self.enqueue_text_input(&session, &envelope, text);
                    if !session.is_running() && session.queue_auto_drain() {
                        let server = Arc::clone(self);
                        let session = Arc::clone(&session);
                        tokio::spawn(async move {
                            server.drain_queue(session).await;
                        });
                    }
                    return Ok(outcome);
                }
                self.admitted_command(&envelope).await
            }
            commands::Route::PauseGoal => {
                let session = self.queue_session(&envelope)?;
                Ok(self.pause_goal_command(&session, &envelope))
            }
            commands::Route::ResumeGoal => {
                let session = self.queue_session(&envelope)?;
                if let Some(outcome) = self.resume_goal_submit_ack(&session, &envelope) {
                    return Ok(outcome);
                }
                self.admitted_command(&envelope).await
            }
            // 未接线的命令也进 inbox：ack 必须进命令账本，重放时才能回同一个
            // fail-closed 结果，而不是每次答 unknown。
            commands::Route::Admitted | commands::Route::Unsupported => {
                self.admitted_command(&envelope).await
            }
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
        // 创建序列（id/epoch、meta 落盘、注册、frame sink、ticker）只有
        // `open_session` 一份实现；这里只提供"空会话"这份 seed。
        let session = self.open_session(crate::sessions::SessionSeed::blank())?;
        let session_id = session.id.clone();
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
        let revision_at_decision = session.revision();
        let expected = envelope
            .payload
            .get("expectedForegroundExecutionId")
            .and_then(Value::as_str);
        // 迟到 stop 护栏（TS guard.stopTargetChanged）：带 expected 时必须命中当前
        // 前台执行的 commandId；否则只 noop，绝不 abort 用户没看到的新执行。
        if let Some(expected) = expected {
            if !running || session.active_command().as_deref() != Some(expected) {
                let ack = json!({
                    "commandId": envelope.command_id,
                    "status": "noop",
                    "revisionAtDecision": revision_at_decision,
                    "reasonCode": "guard.stopTargetChanged",
                });
                self.persist_command_ack(session_id, &envelope.command_id, &ack);
                return Ok(RequestOutcome::empty(ack));
            }
        }
        if running {
            session.request_cancel();
        }
        let ack = json!({
            "commandId": envelope.command_id,
            "status": if running { "accepted" } else { "noop" },
            "revisionAtDecision": revision_at_decision,
            "reasonCode": if running { Value::Null } else { json!("guard.nothingToStop") },
        });
        self.persist_command_ack(session_id, &envelope.command_id, &ack);
        // 受理停止 → 立即投影 control.stopState="stopping"（turn 收口前订阅者可见）。
        let post_frames = if running {
            session.bump_revision();
            session.project_control_update()
        } else {
            Vec::new()
        };
        Ok(RequestOutcome {
            response: ack,
            post_frames,
        })
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
        let revision_at_decision = session.revision();
        session.set_title(title.to_string(), "custom");
        session.bump_revision();
        self.persist_meta(&session);
        let ack = json!({
            "commandId": envelope.command_id,
            "status": "accepted",
            "revisionAtDecision": revision_at_decision,
        });
        self.persist_command_ack(session_id, &envelope.command_id, &ack);
        // meta 变更投影给 live 订阅者（TS：SessionTitleUpdated → 投影收口）。
        let post_frames = session.project_meta_update();
        Ok(RequestOutcome {
            response: ack,
            post_frames,
        })
    }

    fn delete_session(
        &self,
        envelope: &zcode_core::protocol::CommandEnvelope,
    ) -> Result<RequestOutcome, RpcError> {
        let session_id = envelope.session_id.as_deref().ok_or_else(|| {
            RpcError::new(CODE_INVALID_PARAMS, "deleteSession requires sessionId")
        })?;
        // 语义 = closeSession，**不是真删**（对齐 TS session-mgmt 的裁决：message 库
        // 无删除 API，旧协议同样只 close；历史必须残留，重启后仍可恢复）。
        // 旧实现 remove_dir_all 会连 transcript 一起抹掉——不可接受的破坏性偏差。
        let removed = self
            .sessions
            .lock()
            .ok()
            .map(|mut guard| guard.remove(session_id).is_some())
            .unwrap_or(false);
        if !removed {
            return Err(session_not_found(session_id));
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
                // 路由到此为止：命令语义全部交给 `commands::execute`（唯一的 admitted
                // 执行入口），本函数只负责先响应 admission ack、再派生执行任务。
                let server = Arc::clone(self);
                let session_for_command = Arc::clone(&session);
                tokio::spawn(async move {
                    commands::execute(&server, session_for_command, *admitted).await;
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
            if let Err(error) = session.persist_ack(command_id, ack) {
                eprintln!(
                    "[zcode-host] persist_ack failed for session {session_id} command {command_id}: {error}"
                );
            }
        }
        // session 查不到时**不**回落到 global 账本：global 桶只由 persist_global_ack 写。
        // 两个写入口等于两个 owner，重提 createSession 的语义会随分支顺序漂移。
    }

    /// 构建一个会话的 hook 准入状态（快照在这里发现；读失败只记诊断，
    /// 会话仍然可用——hook 执行面尚未接线，fail-closed 的表现就是"什么都不跑"）。
    fn build_hook_admission(
        &self,
        workspace_identity: &str,
    ) -> Arc<crate::workspace_hooks::WorkspaceHookAdmission> {
        let admission = Arc::new(crate::workspace_hooks::WorkspaceHookAdmission::new(
            self.workspace.clone(),
            workspace_identity.to_string(),
            workspace_identity.to_string(),
            Arc::clone(&self.hook_store),
        ));
        if let Err(error) = admission.refresh(&crate::workspace_hooks::rfc3339_utc(now_ms())) {
            eprintln!(
                "[zcode-host] workspace hook discovery failed for {}: {} ({error:?})",
                workspace_identity,
                error.reason_code()
            );
        }
        admission
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
            followup_mode: session.followup_mode(),
            collaboration_mode: session.collaboration_mode(),
            provider: session.provider(),
            model: session.model(),
            thought: session.thought(),
            goal: session.goal(),
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
            // 降级恢复必须**可见**（review MAJOR-2）：容错回放会跳过坏行，所以
            // 这份 transcript 可能缺一段。静默 resume 会让用户以为会话记忆完整，
            // 而模型实际看到的是残缺上下文——那比直接报错更糟。
            if !transcript.skipped_lines.is_empty() {
                eprintln!(
                    "[zcode-host] degraded resume for session {session_id}: skipped {} unreadable line(s); history={} rows={}",
                    transcript.skipped_lines.len(),
                    transcript.history.len(),
                    transcript.rows.len()
                );
                for reason in transcript.skipped_lines.iter().take(3) {
                    eprintln!("[zcode-host]   skipped {reason}");
                }
                if transcript.skipped_lines.len() > 3 {
                    eprintln!(
                        "[zcode-host]   … {} more",
                        transcript.skipped_lines.len() - 3
                    );
                }
            }
            let next_row_id = transcript
                .rows
                .iter()
                .filter_map(|row| row.get("rowId").and_then(Value::as_u64))
                .max()
                .unwrap_or(0)
                + 1;
            let hooks = self.build_hook_admission(&meta.workspace_identity);
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
                history: transcript.history,
                // 分支截断的历史锚点随 transcript 回放重建：重启后仍能安全 edit/retry。
                turn_history_starts: transcript.turn_history_starts,
                file_changes: transcript.file_changes,
                followup_mode: meta.followup_mode,
                collaboration_mode: meta.collaboration_mode,
                provider: meta.provider,
                model: meta.model,
                thought: meta.thought,
                goal: meta.goal,
                hooks,
                store: self.store.clone(),
            });
            session.ensure_hook_review(now_ms());
            session.run_session_start_hooks("resume");
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
