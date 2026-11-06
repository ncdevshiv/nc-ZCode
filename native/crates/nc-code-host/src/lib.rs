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

#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::empty_line_after_doc_comments)]
#![allow(dead_code)]
#![allow(unused_variables)]

pub mod agent;
pub mod attachment;
pub mod background;
mod background_notifier;
pub mod branch;
mod commands;
pub mod commands_query;
pub mod compact;
pub mod content_hash;
pub mod context;
pub mod controller;
pub mod controller_row;
pub mod conversation_reads;
pub mod file_ledger;
pub mod frame;
pub mod goal;
pub mod goal_loop;
pub mod goal_verifier;
pub mod headless;
pub mod host_interactions;
mod host_queue;
pub mod i18n_runtime;
pub mod interaction;
pub mod mcp;
pub mod mcp_tools;
pub mod memory;
pub mod memory_agent;
pub mod memory_paths;
pub mod nc_tools;
pub mod node_repl_runtime;
pub mod notifications;
pub mod permission;
pub mod profile;
pub mod protocol_methods;
pub mod provider_config;
pub mod provider_handoff;
pub mod queue;
pub mod read_model;
pub mod reconciliation;
pub mod rewind;
pub mod rows;
pub mod rpc;
pub mod runtime_core;
pub mod session;
pub mod session_debug;
pub mod session_ttft;
pub mod sessions;
pub mod sessions_index;
pub mod share_attachment;
pub mod shell;
pub mod single_shot;
pub mod snapshot;
pub mod storage;
pub mod store;
pub mod subagent;
pub mod subagent_profiles;
pub mod telemetry_runtime;
pub mod tools;
pub mod tui_runtime;
pub mod turn;
pub mod turn_context;
pub mod workflow;
pub mod workflow_graph;
pub mod workflow_projection;
pub mod workflow_service;
pub mod workspace_hooks;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use nc_code_core::identity::identity_key;
use nc_code_core::protocol::parse_command_envelope;
use serde_json::{json, Value};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

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
    data_dir: PathBuf,
    store: TranscriptStore,
    subagent_profiles: Mutex<Option<Arc<Vec<crate::subagent_profiles::AgentProfile>>>>,
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
    /// In-process channel host mode: frames are delivered in the response's post-frames
    /// instead of waiting for the stdio writer. Set by `set_inline_responses`.
    inline_responses: std::sync::atomic::AtomicBool,
    /// Scheduled-task port injected by the composition root (services layer). The host
    /// does not depend on the automation service; absent = cron tools fail closed.
    automation_port: Mutex<Option<Arc<dyn tools::AutomationPort>>>,
    /// Browser-control port injected by the composition root. Absent = `Browser`
    /// tool fails closed with `browser_unavailable`.
    browser_port: Mutex<Option<Arc<dyn tools::browser::BrowserControlPort>>>,
    /// Computer-use port injected by the composition root. Absent = `Computer`
    /// tool fails closed with `computer_unavailable`.
    computer_port: Mutex<Option<Arc<dyn tools::computer::ComputerControlPort>>>,
    /// Local time-to-first-token recorder (per host instance).
    pub(crate) ttft: Mutex<crate::session_ttft::TtftRecorder>,
    /// In-flight `generateWorkspaceText` cancellations, keyed by operationId.
    pub(crate) generate_operations: Mutex<HashMap<String, Arc<std::sync::atomic::AtomicBool>>>,
    /// Attachment upload transactions / committed metadata (file attachments).
    pub(crate) upload_registry: Mutex<crate::attachment::UploadRegistry>,
    /// Session-level content-addressed attachment store.
    pub(crate) attachments: crate::attachment::AttachmentStore,
    /// Lazy workflow tool port (run service + compile kernel + snippet surface).
    workflow_port: Mutex<Option<Arc<crate::tools::workflow::service_port::HostWorkflowPort>>>,
    /// In-process subagent run registry (disk records fill the gap after restart).
    subagent_runs: Mutex<crate::subagent::SubagentRegistry>,
    /// Window-controller stdio projection (v4/controller/*, spec §1).
    pub(crate) controller: Mutex<crate::controller::ControllerState>,
    /// 自引用弱指针：flush ticker 只持有 Weak，server 释放即退出（stdout writer 收尾的前提）。
    self_weak: std::sync::OnceLock<std::sync::Weak<HostServer>>,
    /// Runtime handle captured at construction: background workers are plain OS
    /// threads (bash reader, subagent settle), so `Handle::try_current()` is absent
    /// when they deliver a completion notification.
    runtime: std::sync::OnceLock<tokio::runtime::Handle>,
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
        // AttachmentStore root = <data-dir>/sessions (same session directories as the transcript).
        let attachments_root = data_dir.join("sessions");
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

        let nc_kernel = nc_tools::build_kernel(&workspace)?;
        let turn_deps = TurnDeps {
            registry: Arc::new(
                ToolRegistry::with_defaults()
                    .with_tool(Arc::new(tools::web_search::WebSearchTool::new(Some(
                        Arc::clone(&nc_kernel),
                    ))))
                    .with_nc_tools(nc_tools::tools_from_kernel(nc_kernel))
                    .with_mcp_tools(mcp_tools::proxies_for(&mcp_pool)),
            ),
            context: turn_context::assemble_context_with_memory(&workspace, Some(&memory_root)),
            full_access,
            provider_config: provider.clone().map(Arc::new),
            host: std::sync::OnceLock::new(),
            max_tool_rounds: None,
        };
        let instance_id = next_instance_id();
        let server = Arc::new(Self {
            workspace,
            workspace_identity,
            store: TranscriptStore::new(data_dir.clone()),
            data_dir,
            subagent_profiles: Mutex::new(None),
            hook_store,
            provider,
            sessions: Mutex::new(HashMap::new()),
            global_acks: Mutex::new(HashMap::new()),
            instance_id: instance_id.clone(),
            outbox,
            id_counter: AtomicU64::new(1),
            turn_deps,
            inline_responses: std::sync::atomic::AtomicBool::new(false),
            automation_port: Mutex::new(None),
            browser_port: Mutex::new(None),
            computer_port: Mutex::new(None),
            ttft: Mutex::new(crate::session_ttft::TtftRecorder::new(instance_id.clone())),
            generate_operations: Mutex::new(HashMap::new()),
            upload_registry: Mutex::new(crate::attachment::UploadRegistry::new()),
            attachments: crate::attachment::AttachmentStore::new(attachments_root),
            workflow_port: Mutex::new(None),
            subagent_runs: Mutex::new(crate::subagent::SubagentRegistry::default()),
            controller: Mutex::new(crate::controller::ControllerState::new(
                format!("controller-tasks-{instance_id}"),
                format!("controller-workspaces-{instance_id}"),
            )),
            self_weak: std::sync::OnceLock::new(),
            runtime: std::sync::OnceLock::new(),
        });
        let _ = server.self_weak.set(Arc::downgrade(&server));
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let _ = server.runtime.set(handle);
        }
        let _ = server.turn_deps.host.set(Arc::downgrade(&server));
        Ok((server, inbox))
    }

    pub fn workspace(&self) -> &PathBuf {
        &self.workspace
    }

    pub fn has_provider(&self) -> bool {
        self.provider.is_some()
    }

    /// In-process host mode: deliver frames immediately with the response instead of
    /// relying on the stdio writer. The channel runtime sets this before creating any
    /// session; existing sessions are switched too so the flag is not order-sensitive.
    pub fn set_inline_responses(&self, enabled: bool) {
        self.inline_responses
            .store(enabled, std::sync::atomic::Ordering::SeqCst);
        if let Ok(guard) = self.sessions.lock() {
            for session in guard.values() {
                session.set_eager_frames(enabled);
            }
        }
    }

    /// Inject the scheduled-task port. The composition root owns the concrete
    /// automation service; the host only sees the port and fails closed without it.
    pub fn set_automation_port(&self, port: Arc<dyn tools::AutomationPort>) {
        if let Ok(mut guard) = self.automation_port.lock() {
            *guard = Some(port);
        }
    }

    pub(crate) fn automation_port(&self) -> Option<Arc<dyn tools::AutomationPort>> {
        self.automation_port
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    /// Inject the native browser-control port. The shell owns the CDP bridge; the
    /// host only sees the port and fails closed without it.
    pub fn set_browser_port(&self, port: Arc<dyn tools::browser::BrowserControlPort>) {
        if let Ok(mut guard) = self.browser_port.lock() {
            *guard = Some(port);
        }
    }

    pub(crate) fn browser_port(&self) -> Option<Arc<dyn tools::browser::BrowserControlPort>> {
        self.browser_port
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    /// Inject the native computer-use port. The shell owns screen capture and
    /// input injection; the host only sees the port and fails closed without it.
    pub fn set_computer_port(&self, port: Arc<dyn tools::computer::ComputerControlPort>) {
        if let Ok(mut guard) = self.computer_port.lock() {
            *guard = Some(port);
        }
    }

    pub(crate) fn computer_port(&self) -> Option<Arc<dyn tools::computer::ComputerControlPort>> {
        self.computer_port
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    pub(crate) fn ttft_receive(&self, envelope: &nc_code_core::protocol::CommandEnvelope) {
        let Ok(mut recorder) = self.ttft.lock() else {
            return;
        };
        recorder.receive(
            &envelope.command_id,
            envelope.session_id.as_deref(),
            envelope.ttft.as_ref(),
            false,
        );
    }

    fn ttft_emit(&self, fact: Option<Value>) {
        if let Some(fact) = fact {
            let _ = self
                .outbox
                .send(crate::session_ttft::notification_line(fact));
        }
    }

    pub(crate) fn ttft_mark_admitted(&self, command_id: &str) {
        let fact = self
            .ttft
            .lock()
            .ok()
            .and_then(|mut recorder| recorder.mark_admitted(command_id));
        self.ttft_emit(fact);
    }

    pub(crate) fn ttft_mark_execution(&self, command_id: &str, turn_id: &str) {
        let fact = self
            .ttft
            .lock()
            .ok()
            .and_then(|mut recorder| recorder.mark_execution(command_id, turn_id));
        self.ttft_emit(fact);
    }

    pub(crate) fn ttft_mark_request(
        &self,
        command_id: &str,
        request_id: &str,
        provider: &str,
        model: &str,
        traces: &[nc_code_provider::executor::AttemptTrace],
    ) {
        let fact = self.ttft.lock().ok().and_then(|mut recorder| {
            recorder.mark_request(
                command_id,
                &crate::session_ttft::TtftRequest {
                    request_id,
                    provider,
                    model,
                    traces,
                },
            )
        });
        self.ttft_emit(fact);
    }

    pub(crate) fn ttft_mark_first_output(&self, command_id: &str, kind: &str) {
        let fact = self
            .ttft
            .lock()
            .ok()
            .and_then(|mut recorder| recorder.mark_first_output(command_id, kind));
        self.ttft_emit(fact);
    }

    pub(crate) fn ttft_mark_terminal(&self, command_id: &str, terminal: &'static str) {
        let fact = self
            .ttft
            .lock()
            .ok()
            .and_then(|mut recorder| recorder.mark_terminal(command_id, terminal));
        self.ttft_emit(fact);
    }

    /// Load a session from the in-memory registry, or cold-resume it from disk.
    ///
    /// This is the only cold-read entry point used by the channel layer; it never
    /// fabricates a session for an unknown id.
    pub fn load_session(&self, session_id: &str) -> Option<Arc<Session>> {
        if let Some(existing) = self.session(session_id) {
            return Some(existing);
        }
        if !is_safe_session_id(session_id) {
            return None;
        }
        let meta = self.store.load_meta(session_id).ok()?;
        let transcript = self.store.load_transcript(session_id).unwrap_or_default();
        let next_row_id = transcript
            .rows
            .iter()
            .filter_map(|row| row.get("rowId").and_then(Value::as_u64))
            .max()
            .unwrap_or(0)
            + 1;
        let hooks = self.build_hook_admission(&meta.workspace_identity);
        let plan_previous_mode = meta.plan_previous_mode.clone();
        let session = Session::new(SessionInit {
            id: session_id.to_string(),
            workspace_root: self.workspace.clone(),
            workspace_identity: meta.workspace_identity.clone(),
            created_at: meta.created_at,
            task_type: meta.task_type.clone(),
            parent_session_id: meta.parent_session_id.clone(),
            log_epoch: meta.log_epoch,
            title: meta.title,
            title_source: meta.title_source,
            rows: transcript.rows,
            revision: meta.revision,
            transcript_acks: transcript.acks,
            next_row_id,
            history: transcript.history,
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
        if !plan_previous_mode.is_empty() {
            session.set_plan_previous_mode(&plan_previous_mode);
        }
        session.ensure_hook_review(now_ms());
        session.run_session_start_hooks("resume");
        self.register_session(Arc::clone(&session));
        Some(session)
    }

    /// Runtime projection for the task list / session channel. Read-only: does not
    /// trigger a cold resume (call `load_session` for that).
    pub fn session_view(&self, session_id: &str) -> Option<Value> {
        let session = self.session(session_id)?;
        Some(self.session_view_of(&session))
    }

    pub(crate) fn session_view_of(&self, session: &Arc<Session>) -> Value {
        let (title, title_source) = session.title();
        let phase = session.phase();
        json!({
            "sessionId": session.id,
            "workspacePath": self.workspace.to_string_lossy(),
            "workspaceIdentity": session.workspace_identity,
            "title": title,
            "titleSource": title_source,
            "phase": phase,
            "status": phase,
            "sessionEnded": matches!(
                phase.as_str(),
                "completedSuccess" | "completedInterrupted" | "error"
            ),
            "createdAt": session.created_at(),
            "updatedAt": session.updated_at(),
            "revision": session.revision(),
            "logEpoch": session.log_epoch(),
            "provider": session.provider(),
            "model": session.model(),
            "thought": session.thought(),
            "mode": session.collaboration_mode(),
            "followupMode": session.followup_mode(),
            "goalStatus": session.goal_status(),
            "lastError": session.last_error(),
            "pendingInteractions": session.interactions_projection(),
            "taskType": session.task_type(),
            "parentSessionId": session.parent_session_id(),
        })
    }

    pub fn session_task_type(&self, session_id: &str) -> Option<String> {
        self.session(session_id)?.task_type()
    }

    pub fn session_ids(&self) -> Vec<String> {
        self.sessions
            .lock()
            .map(|guard| guard.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn session_transcript_view(&self, session_id: &str) -> Option<Value> {
        let session = self.session(session_id)?;
        Some(json!({
            "sessionId": session.id,
            "revision": session.revision(),
            "logEpoch": session.log_epoch(),
            "rows": session.rows_snapshot(),
            "fileChanges": session.file_ledger().records(),
            "meta": self.session_view_of(&session),
        }))
    }

    pub fn session_usage(&self, session_id: &str) -> Option<Value> {
        Some(self.session(session_id)?.usage_report())
    }

    /// `<data-dir>/sessions/<id>` — the session's own directory (attachments, subagent output).
    pub fn session_dir(&self, session_id: &str) -> PathBuf {
        self.store.session_dir(session_id)
    }

    /// Agent profiles: built-ins + `<data-dir>/agents/*.md` + `<workspace>/.zcode/agents/*.md`.
    /// Cached after the first read; a profile edit is picked up on the next process start
    /// (the TS product also loads them at session start).
    pub fn subagent_profiles(&self) -> Arc<Vec<crate::subagent_profiles::AgentProfile>> {
        if let Ok(guard) = self.subagent_profiles.lock() {
            if let Some(profiles) = guard.as_ref() {
                return Arc::clone(profiles);
            }
        }
        let (profiles, diagnostics) = crate::subagent_profiles::load_agent_profiles(
            &self.data_dir,
            &self.workspace,
            self.provider.as_ref(),
        );
        for diagnostic in diagnostics {
            eprintln!(
                "[zcode-host] agent profile {}: {}",
                diagnostic.code, diagnostic.message
            );
        }
        let profiles = Arc::new(profiles);
        if let Ok(mut guard) = self.subagent_profiles.lock() {
            *guard = Some(Arc::clone(&profiles));
        }
        profiles
    }

    pub fn subagent_profile(&self, name: &str) -> Option<crate::subagent_profiles::AgentProfile> {
        self.subagent_profiles()
            .iter()
            .find(|profile| profile.name == name)
            .cloned()
    }

    pub fn subagent_profile_names(&self) -> Vec<String> {
        self.subagent_profiles()
            .iter()
            .map(|profile| profile.name.clone())
            .collect()
    }

    /// The workflow run service (None when the journal could not be opened).
    pub fn workflow_service(&self) -> Option<Arc<crate::workflow_service::WorkflowService>> {
        let _ = self.workflow_port();
        self.workflow_port
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().and_then(|port| port.service_handle()))
    }

    /// Cold rebuild of a session's `workflowRuns` projection from the run journal.
    /// Idempotent: an already-populated projection is left untouched.
    pub fn ensure_workflow_runs(&self, session: &Arc<Session>) {
        if session.workflow_runs_state().is_some() {
            return;
        }
        let Some(service) = self.workflow_service() else {
            return;
        };
        let runs = service.list_runs(Some(&session.id));
        let mut state: Option<Value> = None;
        for run in runs {
            let mut after = None;
            loop {
                let page = service.list_events(&run.run_id, after, Some(200));
                for stored in &page.events {
                    let Ok(event) = serde_json::to_value(&stored.event) else {
                        continue;
                    };
                    let event_type = event
                        .get("type")
                        .cloned()
                        .unwrap_or_else(|| json!("unknown"));
                    let envelope = json!({
                        "runId": run.run_id,
                        "eventType": event_type,
                        "sequence": stored.sequence,
                        "payload": event,
                    });
                    state =
                        crate::workflow_projection::reduce_workflow_runs(state.as_ref(), &envelope)
                            .or(state);
                }
                match page.next_sequence {
                    Some(next) if page.has_more => after = Some(next),
                    _ => break,
                }
            }
        }
        if state.is_some() {
            session.set_workflow_runs(state);
        }
    }

    /// `v4/conversation/backgroundBashOutput`: one background Bash task's merged
    /// stdout/stderr file. Unknown/missing ids are invalid requests, never empty output.
    pub(crate) fn background_bash_output(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                RpcError::new(CODE_INVALID_PARAMS, "params.sessionId: required string")
            })?;
        let work_id = params
            .get("workId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| RpcError::new(CODE_INVALID_PARAMS, "params.workId: required string"))?;
        let Some(session) = self.session(session_id) else {
            return Ok(json!({"kind": "unavailable", "workId": work_id}));
        };
        // The registry is the single owner of the detail shape: it freezes the
        // state before reading the file and includes `status` (the test and the
        // desktop both render from it). Do not rebuild a second, status-less shape here.
        Ok(session.background.output_snapshot(work_id))
    }

    /// `v4/conversation/plans`: the terminal ExitPlanMode rows of one session, with
    /// their plan input. This is the read model the composer's plan pane consumes.
    pub(crate) fn conversation_plans(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                RpcError::new(CODE_INVALID_PARAMS, "params.sessionId: required string")
            })?;
        let session = self.session(session_id).ok_or_else(|| {
            crate::rpc::RpcError::new(
                CODE_INVALID_PARAMS,
                format!("unknown session: {session_id}"),
            )
        })?;
        let rows = session.rows_snapshot();
        let mut plans = Vec::new();
        for row in &rows {
            if row.get("kind").and_then(Value::as_str) != Some("toolCall")
                || row.get("toolName").and_then(Value::as_str) != Some("ExitPlanMode")
                || row.get("status").and_then(Value::as_str) != Some("success")
            {
                continue;
            }
            let input = row
                .get("inputText")
                .and_then(Value::as_str)
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
                .unwrap_or(Value::Null);
            plans.push(json!({
                "toolCallId": row.get("toolCallId"),
                "toolName": "ExitPlanMode",
                "status": "success",
                "input": input,
                "rowId": row.get("rowId"),
            }));
        }
        Ok(json!({
            "plans": plans,
            "atSeq": rows.last().and_then(|row| row.get("createdAtSeq")).cloned().unwrap_or(json!(0)),
            "atLogEpoch": session.log_epoch(),
        }))
    }

    /// The session's current `workflowRuns` projection. The live reducer is fed by the
    /// v4 frame stream; a session that has not received workflow events has no
    /// projection (the Get tool omits `phases` rather than inventing an empty roster).
    pub fn workflow_runs_state(&self, _session_id: &str) -> Option<Value> {
        None
    }

    /// Dispatch a v4 workflow read to the workflow service. When unavailable, return
    /// schema-compatible empty/failure responses matching `workflow_service::v4` semantics.
    fn workflow_v4(
        &self,
        params: &Value,
        handler: fn(&crate::workflow_service::WorkflowService, &Value) -> Result<Value, RpcError>,
        empty: fn() -> Result<Value, RpcError>,
    ) -> Result<Value, RpcError> {
        if let Some(service) = self.workflow_service() {
            handler(service.as_ref(), params)
        } else {
            empty()
        }
    }

    /// Lazy workflow tool port. The run service needs an `Arc<HostServer>`, so it is
    /// attached here (after construction) rather than passed to `new`.
    pub fn workflow_port(&self) -> Option<Arc<dyn crate::tools::workflow::WorkflowToolPort>> {
        if let Ok(guard) = self.workflow_port.lock() {
            if let Some(port) = guard.as_ref() {
                return Some(Arc::clone(port) as Arc<dyn crate::tools::workflow::WorkflowToolPort>);
            }
        }
        let host = self.self_weak.get()?.upgrade()?;
        let port = Arc::new(crate::tools::workflow::service_port::HostWorkflowPort::new(
            self.data_dir.clone(),
            self.workspace.clone(),
        ));
        port.attach(Arc::downgrade(&host));
        match crate::workflow_service::WorkflowService::new(
            Arc::clone(&host),
            &self.data_dir,
            self.workspace.clone(),
        ) {
            Ok(service) => port.set_service(service),
            Err(error) => eprintln!(
                "[zcode-host] workflow journal unavailable; workflow tools fail closed: {error}"
            ),
        }
        if let Ok(mut guard) = self.workflow_port.lock() {
            *guard = Some(Arc::clone(&port));
        }
        Some(port as Arc<dyn crate::tools::workflow::WorkflowToolPort>)
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
        if self
            .inline_responses
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            session.set_eager_frames(true);
        }
        if let Ok(mut guard) = self.sessions.lock() {
            guard.insert(session.id.clone(), Arc::clone(&session));
        }
        // Window-controller live delta (spec §1): a new session is a real
        // tasks-index event; snapshot at subscribe/resync reflects it too.
        let view = self.session_view_of(&session);
        self.controller_notify_session(&view);
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
            crate::rpc::METHOD_CONTROLLER_SUBSCRIBE => self.controller_subscribe(&request.params),
            crate::rpc::METHOD_CONTROLLER_RESYNC => self.controller_resync(&request.params),
            crate::rpc::METHOD_CONTROLLER_UNSUBSCRIBE => {
                self.controller_unsubscribe(&request.params)
            }
            crate::rpc::METHOD_CONVERSATION_ATTACHMENT_READ => self
                .conversation_attachment_read(&request.params)
                .map(RequestOutcome::empty),
            crate::rpc::METHOD_CONVERSATION_ATTACHMENT_STAT => self
                .conversation_attachment_stat(&request.params)
                .map(RequestOutcome::empty),
            "v4/conversation/fileChanges" => self
                .conversation_file_changes(&request.params)
                .map(RequestOutcome::empty),
            "v4/conversation/fileRewindPreview" => self
                .conversation_file_rewind_preview(&request.params)
                .map(RequestOutcome::empty),
            "v4/conversation/usage" => self
                .conversation_usage(&request.params)
                .map(RequestOutcome::empty),
            "v4/attachment/begin" => self
                .attachment_begin(&request.params)
                .map(RequestOutcome::empty),
            "v4/attachment/chunk" => self
                .attachment_chunk(&request.params)
                .map(RequestOutcome::empty),
            "v4/attachment/commit" => self
                .attachment_commit(&request.params)
                .map(RequestOutcome::empty),
            "v4/attachment/abort" => self
                .attachment_abort(&request.params)
                .map(RequestOutcome::empty),
            "v4/attachment/read" => self
                .attachment_read(&request.params)
                .map(RequestOutcome::empty),
            "v4/attachment/previewSource" => self
                .attachment_preview_source(&request.params)
                .map(RequestOutcome::empty),
            "v4/conversation/plans" => self
                .conversation_plans(&request.params)
                .map(RequestOutcome::empty),
            "v4/conversation/workflowRuns" => self
                .workflow_v4(
                    &request.params,
                    crate::workflow_service::WorkflowService::v4_workflow_runs,
                    || Ok(json!({ "runs": [] })),
                )
                .map(RequestOutcome::empty),
            "v4/conversation/workflowRunEvents" => self
                .workflow_v4(
                    &request.params,
                    crate::workflow_service::WorkflowService::v4_workflow_run_events,
                    || Ok(json!({ "events": [], "hasMore": false })),
                )
                .map(RequestOutcome::empty),
            "v4/conversation/workflowRunArtifacts" => self
                .workflow_v4(
                    &request.params,
                    crate::workflow_service::WorkflowService::v4_workflow_run_artifacts,
                    || Ok(json!({ "artifacts": [] })),
                )
                .map(RequestOutcome::empty),
            "v4/conversation/workflowRunArtifactData" => self
                .workflow_v4(
                    &request.params,
                    crate::workflow_service::WorkflowService::v4_workflow_run_artifact_data,
                    || Ok(json!({ "items": [], "hasMore": false })),
                )
                .map(RequestOutcome::empty),
            "v4/conversation/workflowRunArtifactRead" => self
                .workflow_v4(
                    &request.params,
                    crate::workflow_service::WorkflowService::v4_workflow_run_artifact_read,
                    || {
                        Err(RpcError::new(
                            crate::rpc::CODE_INVALID_REQUEST,
                            "workflow service unavailable in this host",
                        ))
                    },
                )
                .map(RequestOutcome::empty),
            "v4/conversation/workflowRunWorkspace" => self
                .workflow_v4(
                    &request.params,
                    crate::workflow_service::WorkflowService::v4_workflow_run_workspace,
                    || Ok(json!({ "nodes": [] })),
                )
                .map(RequestOutcome::empty),
            "v4/conversation/workflowRunNodeResult" => self
                .workflow_v4(
                    &request.params,
                    crate::workflow_service::WorkflowService::v4_workflow_run_node_result,
                    || {
                        Err(RpcError::new(
                            crate::rpc::CODE_INVALID_REQUEST,
                            "workflow service unavailable in this host",
                        ))
                    },
                )
                .map(RequestOutcome::empty),
            "v4/usage/stats" => self.usage_stats(&request.params).map(RequestOutcome::empty),
            "v4/conversation/backgroundBashOutput" => self
                .background_bash_output(&request.params)
                .map(RequestOutcome::empty),
            "session/debug" => self
                .protocol_session_debug(&request.params)
                .map(RequestOutcome::empty),
            "session/subagents" => self
                .protocol_session_subagents(&request.params)
                .map(RequestOutcome::empty),
            "session/compact" => self
                .protocol_session_compact(&request.params)
                .await
                .map(RequestOutcome::empty),
            "session/goal" => self
                .protocol_session_goal(&request.params)
                .await
                .map(RequestOutcome::empty),
            "workspace/generateText" => self
                .protocol_generate_text(&request.params)
                .await
                .map(RequestOutcome::empty),
            "workspace/cancelGenerateText" => self
                .protocol_cancel_generate_text(&request.params)
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
                    if session.has_frame_sink()
                        && !self
                            .inline_responses
                            .load(std::sync::atomic::Ordering::SeqCst)
                    {
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
                    if !self
                        .inline_responses
                        .load(std::sync::atomic::Ordering::SeqCst)
                        && session.enqueue_line(line.clone())
                    {
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
        let base = params.get("base").filter(|value| value.is_object());
        let base_epoch = base
            .and_then(|value| value.get("logEpoch"))
            .and_then(Value::as_str);
        let base_seq = base
            .and_then(|value| value.get("seq"))
            .and_then(Value::as_u64);
        let force_snapshot = params
            .get("forceSnapshot")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let resume = if force_snapshot {
            None
        } else {
            base_epoch
                .filter(|epoch| *epoch == session.log_epoch())
                .and(base_seq)
                .and_then(|seq| session.resume_since(seq).map(|deltas| (seq, deltas)))
        };
        let (mode, post_frames) = match resume {
            Some((seq, deltas)) => {
                session.add_subscriber_at(subscription_id.clone(), profile, seq);
                (
                    "resume",
                    session
                        .replay_to_subscriber(
                            &subscription_id,
                            deltas,
                            crate::frame::DELIVERY_KIND_INITIAL,
                        )
                        .into_iter()
                        .collect::<Vec<_>>(),
                )
            }
            None => {
                session.add_subscriber_at(subscription_id.clone(), profile, 0);
                (
                    "snapshot",
                    session.project_snapshot(&subscription_id, "initial"),
                )
            }
        };
        let ack = json!({
            "ack": {
                "subscriptionId": subscription_id,
                "mode": mode,
                "logEpoch": session.log_epoch(),
            }
        });
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
        if !session.has_subscriber(&subscription_id) {
            return Err(
                RpcError::new(CODE_INVALID_PARAMS, "fault.subscription.notOwned")
                    .with_data(json!({"reasonCode": "fault.subscription.notOwned"})),
            );
        }
        let base = params.get("base").filter(|value| value.is_object());
        let base_epoch = base
            .and_then(|value| value.get("logEpoch"))
            .and_then(Value::as_str);
        let base_seq = base
            .and_then(|value| value.get("seq"))
            .and_then(Value::as_u64);
        let force_snapshot = params
            .get("forceSnapshot")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let resume = if force_snapshot {
            None
        } else {
            base_epoch
                .filter(|epoch| *epoch == session.log_epoch())
                .and(base_seq)
                .and_then(|seq| session.resume_since(seq).map(|deltas| (seq, deltas)))
        };
        let (mode, post_frames) = match resume {
            Some((seq, deltas)) => {
                session.add_subscriber_at(
                    subscription_id.clone(),
                    DeliveryProfileName::Continuous,
                    seq,
                );
                (
                    "resume",
                    session
                        .replay_to_subscriber(
                            &subscription_id,
                            deltas,
                            crate::frame::DELIVERY_KIND_RECOVERY,
                        )
                        .into_iter()
                        .collect::<Vec<_>>(),
                )
            }
            None => {
                session.add_subscriber_at(
                    subscription_id.clone(),
                    DeliveryProfileName::Continuous,
                    0,
                );
                (
                    "snapshot",
                    session.project_snapshot(&subscription_id, "recovery"),
                )
            }
        };
        Ok(RequestOutcome {
            response: json!({
                "ack": {
                    "subscriptionId": subscription_id,
                    "mode": mode,
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
        self.ttft_receive(&envelope);

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
        envelope: &nc_code_core::protocol::CommandEnvelope,
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
        envelope: &nc_code_core::protocol::CommandEnvelope,
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
        envelope: &nc_code_core::protocol::CommandEnvelope,
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
        envelope: &nc_code_core::protocol::CommandEnvelope,
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
        envelope: &nc_code_core::protocol::CommandEnvelope,
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
            nc_code_core::inbox::Outcome::Ack(ack) => {
                let value = serde_json::to_value(&ack).unwrap_or(Value::Null);
                self.persist_command_ack(&session_id, &envelope.command_id, &value);
                Ok(RequestOutcome::empty(value))
            }
            nc_code_core::inbox::Outcome::Execute(admitted) => {
                self.ttft_mark_admitted(&envelope.command_id);
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
        session.touch();
        let meta = SessionMeta {
            session_id: session.id.clone(),
            title,
            title_source,
            log_epoch: session.log_epoch(),
            revision: session.revision(),
            workspace_identity: session.workspace_identity.clone(),
            created_at: session.created_at(),
            updated_at: session.updated_at(),
            task_type: session.task_type(),
            parent_session_id: session.parent_session_id(),
            followup_mode: session.followup_mode(),
            collaboration_mode: session.collaboration_mode(),
            provider: session.provider(),
            model: session.model(),
            thought: session.thought(),
            plan_previous_mode: session.plan_previous_mode().unwrap_or_default(),
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
                created_at: meta.created_at,
                task_type: meta.task_type.clone(),
                parent_session_id: meta.parent_session_id.clone(),
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
            if !meta.plan_previous_mode.is_empty() {
                session.set_plan_previous_mode(&meta.plan_previous_mode);
            }
            session.ensure_hook_review(now_ms());
            session.run_session_start_hooks("resume");
            self.register_session(session);
            resumed += 1;
        }
        resumed
    }
}

/// 一次请求的处理结果：响应体 + 必须排在 response 之后的帧行。
pub(crate) struct RequestOutcome {
    pub(crate) response: Value,
    pub(crate) post_frames: Vec<String>,
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
