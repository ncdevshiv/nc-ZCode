//! Session：一个会话的权威投影态 + 串行 admission + 订阅者帧管线。
//!
//! 所有权（specs/rust-native-crates.md S3 与本 increment 的验收）：session 是行、
//! revision、logEpoch、phase 与命令 inbox 的唯一所有者；turn 只通过本模块的
//! 方法变更状态并发射帧；stdio 连接只携带订阅者身份，不持有会话事实。
//!
//! 事件顺序保证：所有投影变更先追加进 session 行集合并落盘，再进入各订阅者的
//! flush 管线；同一订阅者内帧序严格递增；回放缓冲与在线帧走同一条编码路径。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use zcode_core::inbox::{CommandInbox, SharedCommandInbox};
use zcode_core::protocol::{CommandAck, CommandInboxHost, CommandKey};

use crate::frame::{self, TopicFrameInput, DELIVERY_KIND_ONLINE};
use crate::goal::GoalRecord;
use crate::profile::{
    coalesce_deltas, filter_deltas_for_profile, DeliveryProfile, DeliveryProfileName, ReplayBuffer,
};
use crate::rows::{self, RowBase};
use crate::snapshot::{self, SnapshotInput};
use crate::store::{now_ms, TranscriptStore};
use crate::workspace_hooks::{ReviewTarget, WorkspaceHookAdmission};
use zcode_provider::request::Message;

/// 订阅者缓冲区上限：镜像 `PROTOCOL_V4_LIMITS.subscriberBufferMaxOps/Bytes`。
const SUBSCRIBER_BUFFER_MAX_OPS: usize = 500;
const SUBSCRIBER_BUFFER_MAX_BYTES: usize = 1024 * 1024;

/// inbox 回调面：session 的 revision/epoch/transcript 回源。
pub struct SessionHost {
    pub session_id: String,
    pub revision: AtomicI64,
    pub log_epoch: String,
    transcript_acks: Mutex<HashMap<String, Value>>,
}

impl CommandInboxHost for SessionHost {
    fn get_revision(&self, _session_id: &str) -> Option<i64> {
        Some(self.revision.load(Ordering::SeqCst))
    }

    fn get_log_epoch(&self, _session_id: &str) -> Option<String> {
        Some(self.log_epoch.clone())
    }

    /// transcript 回源：transcript → timeline → child → discarded 优先级中的第一级。
    /// 终态 ack 在 session 落盘（幂等重提的权威事实）。
    fn lookup_transcript_command(&self, key: &CommandKey) -> Option<CommandAck> {
        let guard = self.transcript_acks.lock().ok()?;
        let raw = guard.get(&key.command_id)?;
        serde_json::from_value(raw.clone()).ok()
    }

    fn now_ms(&self) -> i64 {
        now_ms()
    }
}

/// 一个 turn 的 header 事实（终态 upsert 必须原样复用的字段）。
///
/// WHY 不复用行集里那行、也不用 now()：终态 upsert 若把 `startedAt` 写成结束时间，
/// `turnHeader` 的"这轮何时开始"与随之推算的工时一起失真；而 append_row 落盘失败时
/// 行根本不在内存里，只有这份记账能让终态帧回到正确的 rowId 上。
#[derive(Debug, Clone)]
struct TurnHeaderFact {
    row_id: u64,
    started_at: i64,
    origin: String,
    history_round_count: usize,
}

struct Subscriber {
    id: String,
    profile: DeliveryProfile,
    last_to_seq: u64,
    pending: Vec<Value>,
    last_flush_at_ms: i128,
    replay: ReplayBuffer,
}

pub struct Session {
    pub id: String,
    pub workspace_root: PathBuf,
    pub workspace_identity: String,
    title: Mutex<(String, String)>,
    pub rows: Mutex<Vec<Value>>,
    next_row_id: AtomicU64,
    /// 投影记账：每个 op 递增；帧的 (fromSeq, toSeq] 区间以它为准。
    op_seq: AtomicU64,
    logical_frame_ordinal: AtomicU64,
    pub phase: Mutex<String>,
    pub last_error: Mutex<Option<Value>>,
    /// turn 取消探针：工具层与 provider executor 共用同一个开关。
    pub cancel: Arc<AtomicBool>,
    /// 当前前台执行的 sourceCommandId（stop 的 expectedForegroundExecutionId 护栏）。
    active_command: Mutex<Option<String>>,
    /// 帧的唯一有序出口（host outbox）。turn 期间 eager 投递，保证帧序 = 投影序：
    /// 否则「inline flush 的帧被延后到 turn 结束才发」会排在 ticker 后续帧之后
    /// （upserted 先于 appended 到达 → 行状态错乱）。
    frame_sink: Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>,
    eager_frames: AtomicBool,
    /// 产品输入队列（busy 时排队；单一所有者见 `queue` 模块）。
    input_queue: Mutex<crate::queue::InputQueue>,
    queue_pause_reason: Mutex<Option<String>>,
    /// 反向请求登记表（permission/userInput；单一所有者见 `interaction` 模块）。
    interactions: Mutex<crate::interaction::InteractionRegistry>,
    /// 用户在 permission 交互里选择 `fullAccess` 后授予的会话级授权。
    full_access_granted: AtomicBool,
    /// 等待 permission 应答的 oneshot（interactionId → sender）。
    interaction_waiters: Mutex<HashMap<String, tokio::sync::oneshot::Sender<Value>>>,
    /// 正在收尾的 turn 的 header 事实（source_command_id → fact）。append_row 落盘失败时
    /// 行不会进入 `rows` 内存，但投影仍需按原 rowId upsert 终态，因此单独记账，
    /// 避免 `project_turn_finished` 退化成 rowId=0 的孤儿 upsert。
    turn_header_facts: Mutex<HashMap<String, TurnHeaderFact>>,
    /// 每轮起始时模型历史的条数（turnId → count），按落轮顺序排列。
    /// 分支截断（editUserQuery / retryTurn）用它把历史切回目标轮的起点。
    turn_history_starts: Mutex<Vec<(String, usize)>>,
    /// 会话级队列自动 drain 授权位（AtomicBool，与方法名区分）。
    queue_auto_drain_bit: AtomicBool,
    followup_mode: Mutex<String>,
    provider: Mutex<String>,
    model: Mutex<String>,
    thought: Mutex<String>,
    collaboration_mode: Mutex<String>,
    /// 会话目标（TS `SessionGoal` / target）。唯一所有者；持久化落在
    /// `meta.json` 的 `goal` 键（`HostServer::persist_meta`），重启后随 meta 恢复。
    goal: Mutex<Option<GoalRecord>>,
    subscribers: Mutex<Vec<Subscriber>>,
    /// revision / logEpoch / transcript 回源的唯一所有者是 inbox 的 host
    /// （`CommandInboxHost`）；本结构不持有第二份 revision/epoch。
    pub inbox: SharedCommandInbox<SessionHost>,
    /// read-before-edit 状态：会话级（跨 turn 保持），工具层读写它。
    pub read_state: Arc<Mutex<HashSet<PathBuf>>>,
    /// 会话级 todo 列表（工具与 runtime 共用同一实例；随 meta 持久化后可恢复）。
    pub todos: crate::tools::todo::TodoStore,
    /// workspace hook 准入状态（快照/台账/策略/评审 flow 的唯一所有者）。
    hooks: Arc<WorkspaceHookAdmission>,
    /// 当前评审交互的不可变 payload（pending 时存在；作为 `pendingInteractions`
    /// 的一条投影给客户端）。`None` = 没有在途评审。
    hook_review_payload: Mutex<Option<Value>>,
    /// 下一次评审 flow 的 generation（同 flow 重开必须前进）。
    hook_review_generation: AtomicU32,
    /// 模型请求/失败计数（`v4/conversation/usage` 的事实源；重试轮按一次请求计）。
    model_request_count: AtomicU64,
    model_error_count: AtomicU64,
    /// SessionStart hook 的 additionalContext（只注入本会话第一条用户消息）。
    hook_session_context: Mutex<Option<String>>,
    /// 文件变更台账（`applyFileRewind` 的事实来源；工具回合写入，rewind 读取）。
    file_ledger: Arc<crate::file_ledger::FileLedger>,
    /// 模型消息历史的**唯一所有者**（specs/rust-native-crates.md S3 session
    /// memory 规则 1）。turn 只经 append_history 追加，不持有第二份副本。
    history: Mutex<Vec<Message>>,
    store: TranscriptStore,
    usage: Mutex<Value>,
}

/// 构造参数集中到一个结构（调用点两个：create/resume）。
pub struct SessionInit {
    pub id: String,
    pub workspace_root: PathBuf,
    pub workspace_identity: String,
    pub log_epoch: String,
    pub title: String,
    pub title_source: String,
    pub rows: Vec<Value>,
    pub revision: i64,
    pub transcript_acks: HashMap<String, Value>,
    pub next_row_id: u64,
    /// 冷恢复出来的模型历史（createSession 时为空）。
    pub history: Vec<Message>,
    /// 冷恢复出来的文件变更台账（rewind 的事实来源；新建会话为空）。
    pub file_changes: Vec<crate::file_ledger::FileChangeRecord>,
    /// 由 transcript 回放的「轮 → 起始历史条数」表（分支截断用；新建会话为空）。
    pub turn_history_starts: Vec<(String, usize)>,
    /// 会话级配置（冷恢复从 meta 装载；新建为默认值）。
    pub followup_mode: String,
    pub collaboration_mode: String,
    pub provider: String,
    pub model: String,
    pub thought: String,
    /// 冷恢复出来的目标（新建会话为 None）。
    pub goal: Option<GoalRecord>,
    /// workspace hook 准入状态（由 `open_session` 按工作区构建）。
    pub hooks: Arc<WorkspaceHookAdmission>,
    pub store: TranscriptStore,
}

impl Session {
    pub fn new(init: SessionInit) -> Arc<Self> {
        let max_row_id = init
            .rows
            .iter()
            .filter_map(|row| row.get("rowId").and_then(Value::as_u64))
            .max()
            .unwrap_or(0);
        // 台账与它的落盘通道在构造前装配好（见 file_ledger::new_with_transcript_sink）。
        let file_ledger = crate::file_ledger::new_with_transcript_sink(
            &init.id,
            &init.store,
            init.file_changes.clone(),
        );
        let host = SessionHost {
            session_id: init.id.clone(),
            revision: AtomicI64::new(init.revision),
            log_epoch: init.log_epoch,
            transcript_acks: Mutex::new(init.transcript_acks),
        };
        Arc::new(Self {
            id: init.id,
            workspace_root: init.workspace_root,
            workspace_identity: init.workspace_identity,
            title: Mutex::new((init.title, init.title_source)),
            rows: Mutex::new(init.rows),
            next_row_id: AtomicU64::new(init.next_row_id.max(max_row_id)),
            op_seq: AtomicU64::new(max_row_id),
            logical_frame_ordinal: AtomicU64::new(1),
            phase: Mutex::new("draft".to_string()),
            last_error: Mutex::new(None),
            cancel: Arc::new(AtomicBool::new(false)),
            active_command: Mutex::new(None),
            frame_sink: Mutex::new(None),
            eager_frames: AtomicBool::new(false),
            input_queue: Mutex::new(crate::queue::InputQueue::new()),
            queue_pause_reason: Mutex::new(None),
            interactions: Mutex::new(crate::interaction::InteractionRegistry::new()),
            full_access_granted: AtomicBool::new(false),
            interaction_waiters: Mutex::new(HashMap::new()),
            queue_auto_drain_bit: AtomicBool::new(true),
            followup_mode: Mutex::new(init.followup_mode),
            provider: Mutex::new(init.provider),
            model: Mutex::new(init.model),
            thought: Mutex::new(init.thought),
            collaboration_mode: Mutex::new(init.collaboration_mode),
            goal: Mutex::new(init.goal),
            turn_header_facts: Mutex::new(HashMap::new()),
            turn_history_starts: Mutex::new(init.turn_history_starts),
            file_ledger,
            subscribers: Mutex::new(Vec::new()),
            inbox: SharedCommandInbox::new(CommandInbox::new(host)),
            read_state: Arc::new(Mutex::new(HashSet::new())),
            todos: crate::tools::todo::new_todo_store(),
            hooks: init.hooks,
            hook_review_payload: Mutex::new(None),
            hook_review_generation: AtomicU32::new(0),
            model_request_count: AtomicU64::new(0),
            model_error_count: AtomicU64::new(0),
            hook_session_context: Mutex::new(None),
            history: Mutex::new(init.history),
            store: init.store,
            usage: Mutex::new(snapshot::empty_usage()),
        })
    }

    pub fn log_epoch(&self) -> String {
        self.inbox.host().log_epoch.clone()
    }

    pub fn revision(&self) -> i64 {
        self.inbox.host().revision.load(Ordering::SeqCst)
    }

    pub fn set_revision(&self, revision: i64) {
        self.inbox.host().revision.store(revision, Ordering::SeqCst);
    }

    /// 会话级配置变更的 revision 推进（CAS 语义：配置变更后旧 baseRevision 失效）。
    pub fn bump_revision(&self) -> i64 {
        let host = self.inbox.host();
        let next = host.revision.load(Ordering::SeqCst).saturating_add(1);
        host.revision.store(next, Ordering::SeqCst);
        next
    }

    pub fn phase(&self) -> String {
        self.phase
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    /// `sessionPhaseSchema` 的闭集（packages/shared/zcode-protocol-v4/snapshot.ts）。
    /// 写入非法相位会让 snapshot.control 被客户端 schema 拒绝，因此 fail-closed。
    pub(crate) const VALID_PHASES: &'static [&'static str] = &[
        "draft",
        "prewarming",
        "running",
        "completedSuccess",
        "completedInterrupted",
        "error",
    ];

    pub fn set_phase(&self, phase: &str) {
        if !Self::VALID_PHASES.contains(&phase) {
            eprintln!("[zcode-host] refusing invalid session phase: {phase}");
            return;
        }
        if let Ok(mut guard) = self.phase.lock() {
            *guard = phase.to_string();
        }
    }

    pub fn title(&self) -> (String, String) {
        self.title
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    pub fn set_title(&self, title: String, source: &str) {
        if let Ok(mut guard) = self.title.lock() {
            *guard = (title, source.to_string());
        }
    }

    pub fn goal(&self) -> Option<GoalRecord> {
        self.goal
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    pub fn set_goal(&self, goal: Option<GoalRecord>) {
        if let Ok(mut guard) = self.goal.lock() {
            *guard = goal;
        }
    }

    /// 目标状态的读-改-写：整个会话只有这一把锁，闭包内不得再调用会取同一把锁的
    /// 方法（如 `goal()`）。
    pub fn update_goal<T>(&self, update: impl FnOnce(&mut GoalRecord) -> T) -> Option<T> {
        match self.goal.lock() {
            Ok(mut guard) => guard.as_mut().map(update),
            Err(poisoned) => poisoned.into_inner().as_mut().map(update),
        }
    }

    /// 投影态 status（`goalStateSchema.status`），没有目标时为 None。
    pub fn goal_status(&self) -> Option<String> {
        self.goal().map(|goal| goal.projection_status())
    }

    pub fn goal_projection(&self) -> Value {
        self.goal()
            .map(|goal| goal.to_projection())
            .unwrap_or(Value::Null)
    }

    /// 裁判在途（`computeInputRouting` 的 goalVerifying / `control.activeWorks`）。
    pub fn goal_verifying(&self) -> bool {
        self.goal()
            .is_some_and(|goal| goal.verification_in_flight.is_some())
    }

    /// `control.activeWorks` 的本 host 子集：目前只有 goalVerifier 一种工作。
    pub fn active_works(&self) -> Vec<Value> {
        let Some(goal) = self.goal() else {
            return Vec::new();
        };
        match goal.verification_in_flight {
            Some(started_at) => vec![json!({"kind": "goalVerifier", "startedAt": started_at})],
            None => Vec::new(),
        }
    }

    /// 目标变更的权威投影：goal + availability + inputRouting + revision。
    /// 与 turn 收口同规则——revision 只能随一条 `state.updated` 一起前移。
    pub fn project_goal_update(&self) -> Vec<String> {
        self.bump_revision();
        let patch = json!({
            "revision": self.revision(),
            "goal": self.goal_projection(),
            "availability": snapshot::availability(self.goal_status().as_deref()),
            "inputRouting": self.input_routing_value(),
        });
        self.project_deltas(vec![rows::state_updated(patch)], false)
    }

    /// 等待 gate 的 admitted 命令数（goal 循环的让位判据：用户命令优先于自动续跑）。
    pub fn inbox_pending_commands(&self) -> usize {
        self.inbox.pending_command_ids(Some(&self.id)).len()
    }

    /// `goalVerify` 时间线 marker（`timelineMarkerRowSchema`，lane = turnTailBoundary）。
    ///
    /// TS 用 `targetId_goalIteration` 作为生命周期键：started 建行、终态 upsert，
    /// 同一轮重试（新 verificationId）不会长出第二行。返回 (rowId, 帧)。
    pub fn project_goal_verify_marker(
        &self,
        target_id: &str,
        iteration: u32,
        outcome: &str,
        detail: Option<&str>,
        turn_id_hint: Option<&str>,
    ) -> (u64, Vec<String>) {
        let entity_id = format!("{target_id}_goal_{iteration}");
        let mut marker = json!({
            "type": "goalVerify",
            "iteration": iteration,
            "outcome": outcome,
        });
        if let Some(detail) = detail.filter(|detail| !detail.is_empty()) {
            marker["detail"] = json!(detail);
        }
        let existing = self.rows_snapshot().into_iter().find(|row| {
            row.get("kind").and_then(Value::as_str) == Some("timelineMarker")
                && row.get("entityId").and_then(Value::as_str) == Some(entity_id.as_str())
        });
        if let Some(mut row) = existing {
            let row_id = row.get("rowId").and_then(Value::as_u64).unwrap_or(0);
            if let Some(object) = row.as_object_mut() {
                object.insert("marker".to_string(), marker);
            }
            self.upsert_row(row.clone());
            return (
                row_id,
                self.project_deltas(vec![rows::row_upserted(row)], false),
            );
        }
        let turn_id = turn_id_hint
            .map(str::to_string)
            .or_else(|| self.latest_turn_id())
            .unwrap_or_else(|| format!("goal-{target_id}"));
        let base = self.row_base(&turn_id, &entity_id);
        let mut row = rows::row_base(&base);
        if let Some(object) = row.as_object_mut() {
            object.insert("kind".to_string(), json!("timelineMarker"));
            // 轮尾边界：UI 把它留在轮结尾，不折进 turn 分组。
            object.insert("lane".to_string(), json!("turnTailBoundary"));
            object.insert("marker".to_string(), marker);
        }
        let row_id = base.row_id;
        self.append_row(row.clone());
        (
            row_id,
            self.project_deltas(vec![rows::row_appended(row)], false),
        )
    }

    pub fn hooks(&self) -> &Arc<WorkspaceHookAdmission> {
        &self.hooks
    }

    /// 当前评审交互投影（pending 时存在）。
    pub fn hook_review_interaction(&self) -> Option<Value> {
        self.hook_review_payload
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or(None)
    }

    /// 打开/刷新评审 flow：有待审项且没有可复用的 pending flow 时，用前进的
    /// generation 开一条新的；无待审项时清空交互。返回是否新建了 flow。
    pub fn ensure_hook_review(&self, now_ms: i64) -> bool {
        self.refresh_hook_review(now_ms, false)
    }

    /// `force_supersede`：条目状态变了但 bundle 没变（撤销/部分授权）时也必须换一代，
    /// 否则旧交互上的应答还能继续授权刚被撤销的声明。
    pub fn refresh_hook_review(&self, now_ms: i64, force_supersede: bool) -> bool {
        let admission = &self.hooks;
        let pending = admission
            .evaluation()
            .map(|evaluation| evaluation.pending_count())
            .unwrap_or(0);
        if pending == 0 {
            let _ = admission.close_review_if_pending();
            if let Ok(mut guard) = self.hook_review_payload.lock() {
                *guard = None;
            }
            return false;
        }
        let stale = admission.flow_bundle_stale();
        let pending_flow = admission.flow_is_pending();
        if pending_flow && !stale && !force_supersede {
            // 复用：payload 还在就不动；丢了（理论上不会）就按同一代重建。
            if self.hook_review_interaction().is_some() {
                return false;
            }
        }
        let generation = self
            .hook_review_generation
            .fetch_add(1, Ordering::SeqCst)
            .saturating_add(1);
        let review_flow_id = format!("hook-review-{}", self.id);
        let interaction_id = format!("workspace-hook-interaction:{}:{generation}", self.id);
        let result = if pending_flow {
            let Some(target) = self.hook_review_target() else {
                return false;
            };
            admission.supersede_review(
                &target,
                &self.id,
                &review_flow_id,
                generation,
                &interaction_id,
                now_ms,
            )
        } else {
            admission
                .request_review(
                    &self.id,
                    &review_flow_id,
                    generation,
                    &interaction_id,
                    now_ms,
                )
                .map(|outcome| outcome.payload.unwrap_or(Value::Null))
        };
        match result {
            Ok(payload) if !payload.is_null() => {
                if let Ok(mut guard) = self.hook_review_payload.lock() {
                    *guard = Some(payload);
                }
                true
            }
            Ok(_) => false,
            Err(error) => {
                eprintln!(
                    "[zcode-host] workspace hook review could not start: {} ({error:?})",
                    error.reason_code()
                );
                false
            }
        }
    }

    fn hook_review_target(&self) -> Option<ReviewTarget> {
        let payload = self.hook_review_interaction()?;
        crate::workspace_hooks::review_target_from_payload(&payload)
    }

    /// 评审 flow 收口后同步投影：flow 不再 pending 时移除交互。
    pub fn sync_hook_review(&self) {
        if !self.hooks.flow_is_pending() {
            if let Ok(mut guard) = self.hook_review_payload.lock() {
                *guard = None;
            }
        }
    }

    /// `workspaceHookAdmission` + `pendingInteractions` 的权威投影（revision 同帧前移）。
    pub fn project_hook_state_update(&self) -> Vec<String> {
        self.bump_revision();
        let patch = json!({
            "revision": self.revision(),
            "workspaceHookAdmission": self
                .hooks
                .admission_state()
                .unwrap_or(Value::Null),
            "pendingInteractions": self.interactions_projection(),
        });
        self.project_deltas(vec![rows::state_updated(patch)], false)
    }

    /// 评审 deadline 到点：收口 flow（不授权），返回需要广播的帧。
    pub fn expire_hook_review(&self, now_ms: i64) -> Vec<String> {
        if !self.hooks.flow_is_pending() {
            return Vec::new();
        }
        if self.hooks.expire(now_ms).is_none() {
            return Vec::new();
        }
        self.sync_hook_review();
        self.project_hook_state_update()
    }

    /// 运行一组 workspace hook（PreToolUse/PostToolUse/UserPromptSubmit...），
    /// 并把调用事实投影成一条 `hookInvocation` 行。
    ///
    /// 没有快照/没有匹配声明时**不发行**：空的 hookInvocation 行只会污染时间线。
    pub fn run_hook_event(
        &self,
        event: crate::workspace_hooks::HookEventName,
        match_value: Option<&str>,
        input: crate::workspace_hooks::runtime::HookInput,
        turn_id: &str,
        lane: &str,
        anchor_tool_call_id: Option<&str>,
    ) -> crate::workspace_hooks::runtime::HookEventOutcome {
        let Some(snapshot) = self.hooks.snapshot() else {
            return Default::default();
        };
        let Some(evaluation) = self.hooks.evaluation() else {
            return Default::default();
        };
        let started_at = now_ms();
        let cancel: Arc<dyn Fn() -> bool + Send + Sync> = {
            let cancel = Arc::clone(&self.cancel);
            Arc::new(move || cancel.load(Ordering::SeqCst))
        };
        let outcome = crate::workspace_hooks::runtime::run_event(
            event,
            match_value,
            &input,
            &evaluation,
            &snapshot,
            &self.workspace_root,
            cancel,
        );
        if outcome.invocations.is_empty() {
            return outcome;
        }
        let ended_at = now_ms();
        let executions: Vec<Value> = outcome
            .invocations
            .iter()
            .enumerate()
            .map(|(index, invocation)| {
                let did_execute = !matches!(
                    invocation.status,
                    crate::workspace_hooks::runtime::InvocationStatus::Skipped { .. }
                );
                let (state, outcome_label) = match &invocation.status {
                    crate::workspace_hooks::runtime::InvocationStatus::Ran => {
                        ("completed", Some("success"))
                    }
                    crate::workspace_hooks::runtime::InvocationStatus::Blocked => {
                        ("completed", Some("blocked"))
                    }
                    crate::workspace_hooks::runtime::InvocationStatus::Background => {
                        ("completed", None)
                    }
                    crate::workspace_hooks::runtime::InvocationStatus::Skipped { .. } => {
                        ("completed", None)
                    }
                    crate::workspace_hooks::runtime::InvocationStatus::Error => (
                        "failed",
                        Some(
                            if invocation.reason_code.as_deref() == Some("workspace_hook_timeout") {
                                "timed_out"
                            } else {
                                "failed"
                            },
                        ),
                    ),
                };
                let mut execution = json!({
                    "hookRunId": invocation.review_item_id,
                    "hookIndex": index,
                    "didExecute": did_execute,
                    "state": state,
                    "startedAt": started_at,
                    "endedAt": ended_at,
                    "durationMs": invocation.duration_ms,
                    "displayName": invocation.command_display,
                    "sourceKind": "project",
                });
                if let Some(label) = outcome_label {
                    execution["outcome"] = json!(label);
                }
                if let Some(code) = &invocation.reason_code {
                    execution["blockReason"] = json!(code);
                }
                if let Some(tool_name) = &input.tool_name {
                    execution["toolName"] = json!(tool_name);
                }
                execution
            })
            .collect();
        let failed = outcome.invocations.iter().any(|invocation| {
            matches!(
                invocation.status,
                crate::workspace_hooks::runtime::InvocationStatus::Error
            )
        });
        let base = self.row_base(
            turn_id,
            &format!("hook:{turn_id}:{event:?}:{}", self.next_op_seq()),
        );
        let row = rows::hook_invocation_row(
            &base,
            rows::HookInvocationFields {
                hook_invocation_id: base.entity_id.as_str(),
                event: event.as_str(),
                state: if failed { "failed" } else { "completed" },
                started_at,
                ended_at,
                lane,
                anchor_tool_call_id,
                executions,
            },
        );
        self.append_row(row.clone());
        self.project_deltas(vec![rows::row_appended(row)], false);
        outcome
    }

    /// async 侧的 hook 入口：hook 是同步外部进程（最长 60s），必须放进阻塞池，
    /// 否则会占住 tokio worker，别的会话的帧/命令一起卡住。
    pub async fn run_hook_event_blocking(
        self: &Arc<Self>,
        event: crate::workspace_hooks::HookEventName,
        match_value: Option<String>,
        input: crate::workspace_hooks::runtime::HookInput,
        turn_id: String,
        lane: &'static str,
        anchor_tool_call_id: Option<String>,
    ) -> crate::workspace_hooks::runtime::HookEventOutcome {
        let session = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            session.run_hook_event(
                event,
                match_value.as_deref(),
                input,
                &turn_id,
                lane,
                anchor_tool_call_id.as_deref(),
            )
        })
        .await
        .unwrap_or_default()
    }

    /// 最后一条 assistantText 行的正文（Stop hook 的 `last_assistant_message`）。
    pub fn last_assistant_text(&self) -> Option<String> {
        self.rows_snapshot()
            .into_iter()
            .rev()
            .find(|row| row.get("kind").and_then(Value::as_str) == Some("assistantText"))
            .and_then(|row| row.get("text").and_then(Value::as_str).map(str::to_string))
    }

    /// SessionStart hook：会话打开/恢复时运行一次，additionalContext 留给第一条用户消息。
    pub fn run_session_start_hooks(&self, source: &str) {
        let input = crate::workspace_hooks::runtime::HookInput {
            session_id: self.id.clone(),
            cwd: self.workspace_root.to_string_lossy().to_string(),
            mode: self.collaboration_mode(),
            source: Some(source.to_string()),
            ..Default::default()
        };
        let outcome = self.run_hook_event(
            crate::workspace_hooks::HookEventName::SessionStart,
            None,
            input,
            "session-start",
            "assistantWork",
            None,
        );
        if outcome.result.additional_contexts.is_empty() {
            return;
        }
        self.set_session_start_context(Some(outcome.result.additional_contexts.join("\n\n")));
    }

    /// SessionStart hooks 的 additionalContext：取走（只注入一次）。
    pub fn take_session_start_context(&self) -> Option<String> {
        self.hook_session_context
            .lock()
            .ok()
            .and_then(|mut guard| guard.take())
    }

    pub fn set_session_start_context(&self, context: Option<String>) {
        if let Ok(mut guard) = self.hook_session_context.lock() {
            *guard = context;
        }
    }

    /// 最近一轮的 turnId（goalVerify marker 的归属轮）。
    pub fn latest_turn_id(&self) -> Option<String> {
        self.turn_history_starts
            .lock()
            .ok()
            .and_then(|guard| guard.last().map(|(turn_id, _)| turn_id.clone()))
    }

    /// `computeInputRouting` 的当前值（所有投影点共用同一份入参）。
    pub fn input_routing_value(&self) -> Value {
        snapshot::input_routing(
            &self.phase(),
            &self.followup_mode(),
            self.queue_len(),
            self.queue_auto_drain(),
            self.goal_verifying(),
        )
    }

    /// 会话级队列自动 drain 授权位（TS `setQueueAutoDrain`）。AtomicBool 无锁，
    /// 失败不可能——store 直接写入即可。
    pub fn set_queue_auto_drain(&self, value: bool) {
        self.queue_auto_drain_bit.store(value, Ordering::SeqCst);
    }

    pub fn queue_auto_drain(&self) -> bool {
        self.queue_auto_drain_bit.load(Ordering::SeqCst)
    }

    pub fn set_followup_mode(&self, value: &str) {
        if let Ok(mut guard) = self.followup_mode.lock() {
            *guard = value.to_string();
        }
    }

    pub fn followup_mode(&self) -> String {
        self.followup_mode
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| "queue".to_string())
    }

    pub fn set_model_config(&self, provider: &str, model: &str, thought: &str) {
        if let Ok(mut g) = self.provider.lock() {
            *g = provider.to_string();
        }
        if let Ok(mut g) = self.model.lock() {
            *g = model.to_string();
        }
        if let Ok(mut g) = self.thought.lock() {
            *g = thought.to_string();
        }
    }
    pub fn provider(&self) -> String {
        self.provider.lock().map(|g| g.clone()).unwrap_or_default()
    }
    pub fn model(&self) -> String {
        self.model.lock().map(|g| g.clone()).unwrap_or_default()
    }
    pub fn thought(&self) -> String {
        self.thought.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn set_collaboration_mode(&self, mode: &str) {
        if let Ok(mut g) = self.collaboration_mode.lock() {
            *g = mode.to_string();
        }
    }
    pub fn collaboration_mode(&self) -> String {
        self.collaboration_mode
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| "build".to_string())
    }

    /// `config` 全量投影（供 state.updated patch 用；字段与 snapshot.config 同形）。
    pub fn config_projection(&self) -> Value {
        json!({
            "provider": self.provider(),
            "model": self.model(),
            "thought": self.thought(),
            "thoughtLevels": [],
            "followupMode": self.followup_mode(),
            "mode": self.collaboration_mode(),
        })
    }

    /// setFollowupMode / switchModelConfig / switchCollaborationMode 的 state.updated
    /// 投影：config + availability + inputRouting + revision，与 TS 的
    /// onFollowupModeChanged/onSessionModeChanged 形状一致。
    pub fn project_config_update(&self) -> Vec<String> {
        let patch = json!({
            "config": self.config_projection(),
            "availability": snapshot::availability(self.goal_status().as_deref()),
            "inputRouting": self.input_routing_value(),
            "revision": self.revision(),
        });
        self.project_deltas(vec![rows::state_updated(patch)], false)
    }

    // ── 产品输入队列（排队事实由 `queue` 模块独占）──

    pub fn queue_len(&self) -> usize {
        self.input_queue
            .lock()
            .map(|guard| guard.len())
            .unwrap_or(0)
    }

    pub fn queue_has_command(&self, command_id: &str) -> bool {
        self.input_queue
            .lock()
            .map(|guard| guard.contains_command(command_id))
            .unwrap_or(false)
    }

    pub fn enqueue_input(
        &self,
        envelope: zcode_core::protocol::CommandEnvelope,
        text: String,
        client_id: String,
        admitted_at: i64,
        tool_disallowlist: Option<Vec<String>>,
    ) {
        if let Ok(mut guard) = self.input_queue.lock() {
            guard.enqueue(envelope, text, client_id, admitted_at, tool_disallowlist);
        }
    }

    pub fn queue_edit(
        &self,
        item_id: &str,
        new_text: &str,
    ) -> Result<(), crate::queue::QueueError> {
        match self.input_queue.lock() {
            Ok(mut guard) => guard.edit(item_id, new_text),
            Err(poisoned) => poisoned.into_inner().edit(item_id, new_text),
        }
    }

    pub fn queue_reorder(&self, item_id: &str, before_item_id: Option<&str>) -> bool {
        match self.input_queue.lock() {
            Ok(mut guard) => guard.reorder(item_id, before_item_id),
            Err(poisoned) => poisoned.into_inner().reorder(item_id, before_item_id),
        }
    }

    pub fn queue_remove(&self, item_id: &str) -> Option<crate::queue::QueueItem> {
        match self.input_queue.lock() {
            Ok(mut guard) => guard.remove(item_id),
            Err(poisoned) => poisoned.into_inner().remove(item_id),
        }
    }

    pub fn pop_queue_front(&self) -> Option<crate::queue::QueueItem> {
        match self.input_queue.lock() {
            Ok(mut guard) => guard.pop_front(),
            Err(poisoned) => poisoned.into_inner().pop_front(),
        }
    }

    pub fn queue_item(&self, item_id: &str) -> Option<crate::queue::QueueItem> {
        self.input_queue
            .lock()
            .ok()
            .and_then(|guard| guard.get(item_id).cloned())
    }

    pub fn set_queue_pause_reason(&self, reason: Option<&str>) {
        if let Ok(mut guard) = self.queue_pause_reason.lock() {
            *guard = reason.map(str::to_string);
        }
    }

    /// 同步命令用：forced + deferred（返回帧由调用方排在 response 之后发送）。
    pub fn project_queue_update_deferred(&self) -> Vec<String> {
        let patch = json!({
            "queue": self.queue_state(),
            "revision": self.revision(),
        });
        self.project_deltas_deferred(vec![rows::state_updated(patch)], true)
    }

    pub fn project_input_routing_update_deferred(&self) -> Vec<String> {
        let patch = json!({
            "inputRouting": self.input_routing_value(),
            "revision": self.revision(),
        });
        self.project_deltas_deferred(vec![rows::state_updated(patch)], true)
    }

    /// `queueStateSchema` 投影（items + autoDrain + pauseReason?）。
    pub fn queue_state(&self) -> Value {
        let pause_reason = self
            .queue_pause_reason
            .lock()
            .ok()
            .and_then(|guard| guard.clone());
        match self.input_queue.lock() {
            Ok(guard) => guard.projection(self.queue_auto_drain(), pause_reason.as_deref()),
            Err(poisoned) => poisoned
                .into_inner()
                .projection(self.queue_auto_drain(), pause_reason.as_deref()),
        }
    }

    /// setAutoDrain / 队列变更的 state.updated 投影：queue 容器整体替换。
    pub fn project_queue_update(&self) -> Vec<String> {
        let patch = json!({
            "queue": self.queue_state(),
            "revision": self.revision(),
        });
        self.project_deltas(vec![rows::state_updated(patch)], false)
    }

    /// renameSession 的 state.updated 投影：meta {title,titleSource} 全量替换。
    /// 旧实现只落盘 meta.json，live 订阅者看不到标题变更。
    pub fn project_meta_update(&self) -> Vec<String> {
        let (title, title_source) = self.title();
        let patch = json!({
            "meta": {"title": title, "titleSource": title_source},
            "revision": self.revision(),
        });
        self.project_deltas(vec![rows::state_updated(patch)], false)
    }

    /// 队列长度/autoDrain 变化时重算 inputRouting（A 区路由授权位）。
    pub fn project_input_routing_update(&self) -> Vec<String> {
        let patch = json!({
            "inputRouting": self.input_routing_value(),
            "revision": self.revision(),
        });
        self.project_deltas(vec![rows::state_updated(patch)], false)
    }

    /// stop 的 state.updated 投影：control.stopState 从 stoppable → stopping，
    /// 让订阅者立刻看到「停止已受理、turn 尚未收口」。
    pub fn project_control_update(&self) -> Vec<String> {
        let works = self.active_works();
        let patch = json!({
            "control": snapshot::control(
                &self.phase(),
                self.is_running(),
                self.cancel.load(Ordering::SeqCst),
                self.last_error().as_ref(),
                &works,
            ),
            "revision": self.revision(),
        });
        self.project_deltas(vec![rows::state_updated(patch)], false)
    }

    /// setAssistantFeedback：目标行必须是 assistantText。`feedback=None` 清除反馈
    /// （payload 的 null）。返回投影行；解析失败返回 Err（调用方 fail-closed settle）。
    pub fn apply_assistant_feedback(
        &self,
        row_id: u64,
        feedback: Option<&str>,
    ) -> Result<Vec<String>, String> {
        let mut row = self
            .rows_snapshot()
            .into_iter()
            .find(|row| row.get("rowId").and_then(Value::as_u64) == Some(row_id))
            .ok_or_else(|| {
                format!("setAssistantFeedback targetRowId {row_id} 无法解析到 transcript messageId")
            })?;
        if row.get("kind").and_then(Value::as_str) != Some("assistantText") {
            return Err(format!(
                "setAssistantFeedback targetRowId {row_id} 无法解析到 transcript messageId"
            ));
        }
        match feedback {
            Some(value) => {
                if let Some(obj) = row.as_object_mut() {
                    obj.insert("feedback".to_string(), json!(value));
                }
            }
            None => {
                if let Some(obj) = row.as_object_mut() {
                    obj.remove("feedback");
                }
            }
        }
        self.upsert_row(row.clone());
        Ok(self.project_deltas(vec![rows::row_upserted(row)], true))
    }

    pub fn usage(&self) -> Value {
        self.usage
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    /// 累加一轮模型调用的 token 用量到会话累计值（多轮 tool loop / 多 turn 累加）。
    /// 旧实现每轮**覆盖** cumulative，导致最终值只剩最后一轮。
    pub fn accumulate_usage(&self, round: &Value) {
        if let Ok(mut guard) = self.usage.lock() {
            let previous = guard
                .get("cumulative")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let mut merged = json!({
                "inputTokens": 0,
                "outputTokens": 0,
                "reasoningTokens": 0,
                "cacheReadTokens": 0,
                "cacheWriteTokens": 0,
            });
            for key in [
                "inputTokens",
                "outputTokens",
                "reasoningTokens",
                "cacheReadTokens",
                "cacheWriteTokens",
            ] {
                let prev = previous.get(key).and_then(Value::as_u64).unwrap_or(0);
                let add = round.get(key).and_then(Value::as_u64).unwrap_or(0);
                merged[key] = json!(prev + add);
            }
            *guard = json!({"contextWindow": Value::Null, "cumulative": merged});
        }
    }

    /// 一次模型 HTTP 请求（executor 内部 attempt 重试算同一次，与 TS 的
    /// `modelRequestCount` 语义一致：按 request 而不是按 attempt 计）。
    pub fn record_model_request(&self) {
        self.model_request_count.fetch_add(1, Ordering::SeqCst);
    }

    pub fn record_model_error(&self) {
        self.model_error_count.fetch_add(1, Ordering::SeqCst);
    }

    /// `v4/conversation/usage` 的结果（`v4ConversationUsageResultSchema`）。
    pub fn usage_report(&self) -> Value {
        let cumulative = self
            .usage()
            .get("cumulative")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let field = |name: &str| cumulative.get(name).and_then(Value::as_u64).unwrap_or(0);
        let input = field("inputTokens");
        let output = field("outputTokens");
        json!({
            "sessionId": self.id,
            "totalTokens": input.saturating_add(output),
            "inputTokens": input,
            "outputTokens": output,
            "reasoningTokens": field("reasoningTokens"),
            "cacheCreationTokens": field("cacheWriteTokens"),
            "cacheReadTokens": field("cacheReadTokens"),
            "modelRequestCount": self.model_request_count.load(Ordering::SeqCst),
            "modelErrorCount": self.model_error_count.load(Ordering::SeqCst),
            // 本 host 不按 querySource 记基线；空对象是"没有基线"，不是丢失数据。
            "inputBaselineBySource": {},
        })
    }

    pub fn last_error(&self) -> Option<Value> {
        self.last_error.lock().ok().and_then(|guard| guard.clone())
    }

    pub fn set_last_error(&self, error: Option<Value>) {
        if let Ok(mut guard) = self.last_error.lock() {
            *guard = error;
        }
    }

    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn take_cancel(&self) -> bool {
        self.cancel.swap(false, Ordering::SeqCst)
    }

    /// host outbox 的写端（进程内唯一，注册会话时注入一次）。
    pub fn set_frame_sink(&self, sink: tokio::sync::mpsc::UnboundedSender<String>) {
        if let Ok(mut guard) = self.frame_sink.lock() {
            *guard = Some(sink);
        }
    }

    /// turn 期间开启 eager 帧投递（settle 时关闭）。
    pub fn set_eager_frames(&self, eager: bool) {
        self.eager_frames.store(eager, Ordering::SeqCst);
    }

    pub(crate) fn interactions_lock(
        &self,
    ) -> std::sync::LockResult<std::sync::MutexGuard<'_, crate::interaction::InteractionRegistry>>
    {
        self.interactions.lock()
    }

    pub(crate) fn register_waiter(
        &self,
        interaction_id: &str,
        sender: tokio::sync::oneshot::Sender<Value>,
    ) {
        if let Ok(mut guard) = self.interaction_waiters.lock() {
            guard.insert(interaction_id.to_string(), sender);
        }
    }

    pub(crate) fn take_waiter(
        &self,
        interaction_id: &str,
    ) -> Option<tokio::sync::oneshot::Sender<Value>> {
        self.interaction_waiters
            .lock()
            .ok()
            .and_then(|mut guard| guard.remove(interaction_id))
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    pub(crate) fn full_access_flag(&self) -> bool {
        self.full_access_granted.load(Ordering::SeqCst)
    }

    pub(crate) fn set_full_access_flag(&self, value: bool) {
        self.full_access_granted.store(value, Ordering::SeqCst);
    }

    pub fn has_subscribers(&self) -> bool {
        self.subscribers
            .lock()
            .map(|guard| !guard.is_empty())
            .unwrap_or(false)
    }

    /// 把已经编码好的帧行写入会话唯一 FIFO（工具任务等非 turn 主路径使用）。
    pub fn emit_frames(&self, lines: Vec<String>) {
        for line in lines {
            self.enqueue_line(line);
        }
    }

    pub fn has_frame_sink(&self) -> bool {
        self.frame_sink
            .lock()
            .map(|guard| guard.is_some())
            .unwrap_or(false)
    }

    /// 把协议行写入会话唯一 FIFO（响应与帧共序）。返回是否投递成功。
    pub fn enqueue_line(&self, line: String) -> bool {
        let sink = self.frame_sink.lock().ok().and_then(|guard| guard.clone());
        match sink {
            Some(sink) => sink.send(line).is_ok(),
            None => false,
        }
    }

    /// 若处于 eager 模式且有 outbox，立即投递并返回 true（调用方不再重复返回该行）。
    /// 投递发生在持有 subscribers 锁的路径内，因此与 ticker 的帧序一致。
    fn deliver_eager(&self, line: &str) -> bool {
        if !self.eager_frames.load(Ordering::SeqCst) {
            return false;
        }
        let sink = self.frame_sink.lock().ok().and_then(|guard| guard.clone());
        match sink {
            Some(sink) => sink.send(line.to_string()).is_ok(),
            None => false,
        }
    }

    /// 标记/清除当前前台执行的 sourceCommandId。
    pub fn set_active_command(&self, command_id: Option<String>) {
        if let Ok(mut guard) = self.active_command.lock() {
            *guard = command_id;
        }
    }

    pub fn active_command(&self) -> Option<String> {
        self.active_command
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or(None)
    }

    fn allocate_row_id(&self) -> u64 {
        self.next_row_id.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn next_op_seq(&self) -> u64 {
        self.op_seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn op_seq(&self) -> u64 {
        self.op_seq.load(Ordering::SeqCst)
    }

    fn row_base(&self, turn_id: &str, entity_id: &str) -> RowBase {
        let row_id = self.allocate_row_id();
        let created_at = now_ms();
        RowBase {
            row_id,
            turn_id: turn_id.to_string(),
            entity_id: entity_id.to_string(),
            created_at,
            created_at_seq: self.next_op_seq(),
        }
    }

    /// 追加一行并返回其 rowId；落盘优先于投影。
    pub fn append_row(&self, row: Value) -> u64 {
        let row_id = row.get("rowId").and_then(Value::as_u64).unwrap_or(0);
        // 落盘优先于投影：内存是磁盘的投影，磁盘写失败时**不**更新内存，
        // 否则重启后内存里多出一行、磁盘里没有， projection 与 durable 事实分叉。
        if let Err(error) = self.store.append_row(&self.id, &row) {
            eprintln!("[zcode-host] append_row failed for {}: {error}", self.id);
            return row_id;
        }
        if let Ok(mut guard) = self.rows.lock() {
            guard.push(row);
        }
        row_id
    }

    /// 就地更新一行（结构变化，整行替换）；transcript 记 Upsert，重放不重复。
    pub fn upsert_row(&self, row: Value) -> u64 {
        let row_id = row.get("rowId").and_then(Value::as_u64).unwrap_or(0);
        if let Err(error) = self.store.append_upsert(&self.id, &row) {
            eprintln!("[zcode-host] append_upsert failed for {}: {error}", self.id);
            return row_id;
        }
        if let Ok(mut guard) = self.rows.lock() {
            if let Some(slot) = guard
                .iter_mut()
                .find(|candidate| candidate.get("rowId").and_then(Value::as_u64) == Some(row_id))
            {
                *slot = row;
            } else {
                guard.push(row);
            }
        }
        row_id
    }

    pub fn rows_snapshot(&self) -> Vec<Value> {
        self.rows
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    /// 窗口投影：尾部 60 行（`PROTOCOL_V4_LIMITS.snapshotTailWindowRows` 镜像）。
    pub fn rows_window(&self) -> Vec<Value> {
        const TAIL_WINDOW_ROWS: usize = 60;
        let rows = self.rows_snapshot();
        if rows.len() > TAIL_WINDOW_ROWS {
            rows[rows.len() - TAIL_WINDOW_ROWS..].to_vec()
        } else {
            rows
        }
    }

    pub fn total_row_count(&self) -> usize {
        self.rows.lock().map(|guard| guard.len()).unwrap_or(0)
    }

    pub fn first_row_id(&self) -> Option<u64> {
        self.rows_snapshot()
            .first()
            .and_then(|row| row.get("rowId").and_then(Value::as_u64))
    }

    /// 已落定 ack 的只读回源（v4/commands/query 用）。
    ///
    /// 账本在 `inbox.host()` 上，而 `SessionHost` 是**每 session 一份**
    /// （带自己的 session_id 与 transcript_acks），所以这里天然按 session 隔离，
    /// 不会把别的 session 的 ack 答给调用方。
    pub fn settled_ack(&self, command_id: &str) -> Option<Value> {
        self.inbox
            .host()
            .transcript_acks
            .lock()
            .ok()?
            .get(command_id)
            .cloned()
    }

    /// 落盘命令 ack（幂等查询面的事实来源）。**返回 Result**：落盘失败时调用方必须能
    /// 观测到（否则 commands/query 永远读不到这个 ack，幂等重提会重新执行——
    /// 静默吞掉就是静默重复执行）。内存账本只在落盘成功后更新，保持「内存是磁盘投影」。
    pub fn persist_ack(
        &self,
        command_id: &str,
        ack: &Value,
    ) -> Result<(), crate::store::StoreError> {
        // 内存账本**总是**更新（commands/query 的读源在进程内，即使磁盘写失败本轮也
        // 必须能查到终态），磁盘写失败只记日志、不阻断查询面——与 history 的「内存是磁盘
        // 投影」纪律不同：ack 是查询面事实，不是模型上下文，丢失只影响重启后的可查性。
        let disk_result = self.store.append_ack(&self.id, command_id, ack);
        if let Ok(mut guard) = self.inbox.host().transcript_acks.lock() {
            guard.insert(command_id.to_string(), ack.clone());
        }
        disk_result
    }

    // ── 模型历史（session memory 规则 1 的唯一写入口）──

    /// 历史快照（供 turn 组装请求体）。
    ///
    /// **锁中毒返回 `Err`，不是空历史**（review round2 MINOR-C）。原注释写的是
    /// 「fail-closed」，但返回 `Vec::new()` 恰恰是 fail-**open**：`turn.rs` 会拿这个
    /// 空历史组装请求体，模型在没有上下文的情况下回答——正是 MAJOR-1 那类「模型看到
    /// 的上下文与 durable 状态不一致」。`Mutex` 中毒意味着持锁期间已经发生过 panic，
    /// 诚实的回应是让本轮失败，而不是编造一段对话。（对照 `project_deltas` 用
    /// `poisoned.into_inner()`：那是投影路径、可容忍；这里是**输入**路径，不可。）
    pub fn history_snapshot(&self) -> Result<Vec<Message>, crate::store::StoreError> {
        self.history
            .lock()
            .map(|guard| guard.clone())
            .map_err(|_| crate::store::StoreError::HistoryPoisoned)
    }

    /// 追加一条历史消息：**先落盘再进内存**，失败可观测。
    ///
    /// 顺序与行投影一致（先 durable 后可见），理由是重启后回放的权威来源是磁盘：
    /// 内存里多一条而磁盘没有的消息，会在重启后凭空消失，等于把「模型以为发生过
    /// 的事」变成不可复现。落盘失败时**不**进内存——内存状态必须始终是磁盘状态的
    /// 投影，否则 fail-closed 方向就反了。
    ///
    /// **返回 `Result` 而不是 `()` 是刻意的**（review MAJOR-1）：调用方在落盘失败
    /// 时必须能拒绝继续。`turn.rs` 记录本轮用户输入后就用 `history_snapshot()`
    /// 组装请求体；若写失败被静默吞掉，模型会去回答一个**从未收到**的问题，而 turn
    /// 仍以 `completedSuccess` 落定，UI 上还显示着用户的问题与一个对不上的回答。
    /// 返回值让「输入没被记住」变成调用方可断言的 typed 错误。
    pub fn append_history(&self, message: Message) -> Result<(), crate::store::StoreError> {
        self.store.append_history(&self.id, &message)?;
        // 落盘成功后**必须**进内存；锁中毒意味着持锁期间已经 panic，本轮不能再继续，
        // 否则内存与磁盘永久分叉且无信号（fail-open）。把中毒升级成 HistoryPoisoned。
        let mut guard = self
            .history
            .lock()
            .map_err(|_| crate::store::StoreError::HistoryPoisoned)?;
        guard.push(message);
        Ok(())
    }

    /// 压缩后用新历史整体替换（同样先落盘再改内存），失败可观测。
    ///
    /// 是否值得替换由调用方判断（microcompact 看是否有消息被瘦身，LLM 摘要看长度
    /// 是否真的变短）；本方法只负责「先落盘、后改内存」这一个不变量，不替压缩器
    /// 做决策——策略属于压缩层，持久化属于 session，两者不混。
    ///
    /// **与 `append_history` 同样返回 `Result`**（review round2 NEW MAJOR-A）：这里
    /// 静默吞掉写失败比 `append_history` 更糟——压缩事件一丢，磁盘保留的是**压缩前**
    /// 的历史，而本轮模型看到的是压缩后的，两者永久分叉且无任何信号。调用方必须能
    /// 把这个失败并进本轮的 `history_persist_failed` 记账。
    /// 分支截断（`editUserQuery` / `retryTurn` 的 durable 侧）：行集切到
    /// `from_row_id` 之前（含该行），模型历史切回 `history_keep` 条。
    /// **先磁盘后内存**：任一步失败都不改动内存投影。
    ///
    /// WHY 第二条写失败要补偿：只落了一条 `rowsTruncated` 就返回错误，磁盘会停在
    /// 「行已截、历史未截」的半程状态，重放出来的行集比历史少一轮——模型上下文与
    /// UI 行集分叉。补偿是把刚截掉的行用 `Row` 条目原样补回（此刻内存还没动过），
    /// 重放结果回到截断前，调用方的 fail-closed 拒绝才有意义。
    pub fn truncate_branch(
        &self,
        from_row_id: u64,
        history_keep: usize,
    ) -> Result<(), crate::store::StoreError> {
        let is_dropped = |row: &Value| {
            row.get("rowId")
                .and_then(Value::as_u64)
                .is_some_and(|id| id >= from_row_id)
        };
        let dropped_rows: Vec<Value> = self
            .rows_snapshot()
            .into_iter()
            .filter(is_dropped)
            .collect();
        let retained_history: Vec<Message> = {
            let guard = self
                .history
                .lock()
                .map_err(|_| crate::store::StoreError::HistoryPoisoned)?;
            guard
                .iter()
                .take(history_keep.min(guard.len()))
                .cloned()
                .collect()
        };
        self.store.append_rows_truncated(&self.id, from_row_id)?;
        if let Err(error) = self
            .store
            .append_history_compaction(&self.id, &retained_history)
        {
            for row in &dropped_rows {
                let _ = self.store.append_row(&self.id, row);
            }
            return Err(error);
        }
        if let Ok(mut guard) = self.rows.lock() {
            guard.retain(|row| !is_dropped(row));
        }
        if let Ok(mut guard) = self.history.lock() {
            let keep = history_keep.min(guard.len());
            guard.truncate(keep);
        }
        // 被截掉的轮：起始点 >= history_keep，或 turnHeader 行本身已被截掉。
        // 只按起始点筛会把目标轮自己留下（它的起点正好 == history_keep），
        // 下一次 edit/retry 就会拿着一个已经不存在的轮做截断。
        if let Ok(mut guard) = self.turn_history_starts.lock() {
            guard.retain(|(turn_id, start)| {
                *start < history_keep && !Self::is_dropped_header(turn_id, &dropped_rows)
            });
        }
        // 被截掉的轮不再可回滚：记录留在台账里，rewind 就会去恢复一个
        // 已经不存在于对话中的轮次。
        self.file_ledger
            .drop_turns(&dropped_turn_ids(&dropped_rows));
        // 被截掉的轮不再可回滚：它的文件记录留在台账里，rewind 就会去恢复一个
        // 已经不存在于对话中的轮次。
        self.file_ledger
            .drop_turns(&dropped_turn_ids(&dropped_rows));
        Ok(())
    }

    /// 轮锚点是否属于被截掉的行：按 turnHeader 的 turnId 判定。
    fn is_dropped_header(turn_id: &str, dropped_rows: &[Value]) -> bool {
        dropped_rows.iter().any(|row| {
            row.get("kind").and_then(Value::as_str) == Some("turnHeader")
                && row.get("turnId").and_then(Value::as_str) == Some(turn_id)
        })
    }

    /// 分叉用的只读切片：父会话的前 `row_count` 行与前 `history_end` 条模型历史。
    ///
    /// 复制时**剥掉 `actions.canRewindFiles`**：fork 只复制对话，不接管工作区，
    /// child 的台账是空的；留着这个键等于 advertise 一个 child 执行不了的动作
    /// （它会去删父会话在共享 workspace 里留下的文件）。`fileChanges` 作为展示
    /// 数据保留。
    pub fn conversation_copy(
        &self,
        row_count: usize,
        history_end: usize,
    ) -> (Vec<Value>, Vec<Message>) {
        let rows: Vec<Value> = self
            .rows_snapshot()
            .into_iter()
            .take(row_count)
            .map(|mut row| {
                if let Some(actions) = row
                    .as_object_mut()
                    .and_then(|object| object.get_mut("actions"))
                    .and_then(Value::as_object_mut)
                {
                    actions.remove("canRewindFiles");
                    if actions.is_empty() {
                        row.as_object_mut().expect("object").remove("actions");
                    }
                }
                row
            })
            .collect();
        let history = self
            .history_snapshot()
            .unwrap_or_default()
            .into_iter()
            .take(history_end)
            .collect();
        (rows, history)
    }

    /// 把某一轮 turnHeader 的 `fileChanges.state` 改成 `"reverted"`（rewind 成功后）。
    ///
    /// WHY 由 host 改写而不是让客户端自己标：state 是"还能不能再回滚"的权威事实，
    /// 两次 rewind 会把同一批文件反复覆盖；TS 用同一套标记做 `canRewindFiles` 门禁。
    pub fn mark_turn_files_reverted(&self, turn_id: &str) -> Vec<String> {
        let mut updated: Option<Value> = None;
        if let Ok(mut guard) = self.rows.lock() {
            for row in guard.iter_mut() {
                if !(row_kind_is(row, "turnHeader")
                    && row.get("turnId").and_then(Value::as_str) == Some(turn_id))
                {
                    continue;
                }
                let Some(object) = row.as_object_mut() else {
                    break;
                };
                let state_ok = object
                    .get("fileChanges")
                    .and_then(Value::as_object)
                    .and_then(|changes| changes.get("state"))
                    .and_then(Value::as_str)
                    == Some("active");
                if !state_ok {
                    break;
                }
                if let Some(changes) = object.get_mut("fileChanges").and_then(Value::as_object_mut)
                {
                    changes.insert("state".to_string(), json!("reverted"));
                }
                if let Some(actions) = object.get_mut("actions").and_then(Value::as_object_mut) {
                    actions.remove("canRewindFiles");
                }
                updated = Some(row.clone());
                break;
            }
        }
        let Some(row) = updated else {
            return Vec::new();
        };
        self.upsert_row(row.clone());
        self.project_deltas(vec![rows::row_upserted(row)], false)
    }

    /// 分支截断的投影帧：一个 `row.removed{fromRowId}`（删除该行及其后所有）。
    pub fn project_rows_removed(&self, from_row_id: u64) -> Vec<String> {
        self.project_deltas(vec![rows::row_removed(from_row_id)], false)
    }

    pub fn replace_history(&self, compacted: &[Message]) -> Result<(), crate::store::StoreError> {
        self.store.append_history_compaction(&self.id, compacted)?;
        let mut guard = self
            .history
            .lock()
            .map_err(|_| crate::store::StoreError::HistoryPoisoned)?;
        *guard = compacted.to_vec();
        Ok(())
    }

    // ── 订阅者与帧发射 ──

    pub fn add_subscriber(&self, id: String, profile: DeliveryProfileName) {
        // 同一 subscriptionId 重新订阅 = 替换旧连接（幂等），否则会保留两个同名
        // subscriber，同一 op 被投影两次（重复帧）。
        self.remove_subscriber(&id);
        let subscriber = Subscriber {
            id,
            profile: DeliveryProfile::named(profile),
            last_to_seq: self.op_seq(),
            pending: Vec::new(),
            last_flush_at_ms: now_ms() as i128,
            replay: ReplayBuffer::new(SUBSCRIBER_BUFFER_MAX_OPS, SUBSCRIBER_BUFFER_MAX_BYTES),
        };
        if let Ok(mut guard) = self.subscribers.lock() {
            guard.push(subscriber);
        }
    }

    pub fn remove_subscriber(&self, id: &str) {
        if let Ok(mut guard) = self.subscribers.lock() {
            guard.retain(|subscriber| subscriber.id != id);
        }
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscribers
            .lock()
            .map(|guard| guard.len())
            .unwrap_or(0)
    }

    /// 投影一批 delta：入各订阅者待发队列；到期或 force 时打帧。
    /// 返回需要写入 stdout 的完整 NDJSON 行（每订阅者一至多帧）。
    pub fn project_deltas(&self, deltas: Vec<Value>, force: bool) -> Vec<String> {
        self.project_deltas_inner(deltas, force, true)
    }

    /// 同步命令处理器用：即使 turn 正在 eager 投递，本调用产出的帧也返回给调用方，
    /// 由它排在 response 之后发送（保证「响应先于帧」）。
    pub fn project_deltas_deferred(&self, deltas: Vec<Value>, force: bool) -> Vec<String> {
        self.project_deltas_inner(deltas, force, false)
    }

    fn project_deltas_inner(
        &self,
        deltas: Vec<Value>,
        force: bool,
        allow_eager: bool,
    ) -> Vec<String> {
        let now = now_ms() as i128;
        let mut out = Vec::new();
        let mut guard = match self.subscribers.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        for subscriber in guard.iter_mut() {
            let filtered = filter_deltas_for_profile(&deltas, subscriber.profile);
            if !filtered.is_empty() {
                subscriber.pending.extend(filtered);
            }
            let window_elapsed =
                now - subscriber.last_flush_at_ms >= subscriber.profile.flush_window_ms as i128;
            if !force && !window_elapsed {
                continue;
            }
            if let Some(line) = self.flush_subscriber(subscriber, now, allow_eager) {
                out.push(line);
            }
        }
        out
    }

    /// 定时 flush：flushWindowMs 到期后把滞留 delta 发出。turn 结束后的尾帧与
    /// 静默期的窗口推进都靠它，不依赖新事件到达（TS 侧同为由定时器驱动）。
    pub fn flush_expired(&self) -> Vec<String> {
        let now = now_ms() as i128;
        let mut out = Vec::new();
        let mut guard = match self.subscribers.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        for subscriber in guard.iter_mut() {
            if subscriber.pending.is_empty() {
                continue;
            }
            if now - subscriber.last_flush_at_ms < subscriber.profile.flush_window_ms as i128 {
                continue;
            }
            if let Some(line) = self.flush_subscriber(subscriber, now, true) {
                out.push(line);
            }
        }
        out
    }

    fn flush_subscriber(
        &self,
        subscriber: &mut Subscriber,
        now: i128,
        allow_eager: bool,
    ) -> Option<String> {
        if subscriber.pending.is_empty() {
            subscriber.last_flush_at_ms = now;
            return None;
        }
        let to_seq = self.op_seq();
        let line = self.encode_frame(
            subscriber,
            json!({"kind": "deltas", "deltas": coalesce_deltas(&subscriber.pending)}),
            DELIVERY_KIND_ONLINE,
            to_seq,
        );
        subscriber.pending.clear();
        subscriber.last_to_seq = to_seq;
        subscriber.last_flush_at_ms = now;
        match line {
            Some(line) => {
                subscriber.replay.push(line.clone());
                // turn 期间立即经唯一 outbox 投递，调用方不再把它带回去延后发送。
                if allow_eager && self.deliver_eager(&line) {
                    None
                } else {
                    Some(line)
                }
            }
            None => None,
        }
    }

    /// 全量 snapshot 帧（initial / recovery）。
    pub fn project_snapshot(&self, subscriber_id: &str, delivery_kind: &str) -> Vec<String> {
        let (title, title_source) = self.title();
        let usage = self.usage();
        let rows = self.rows_window();
        let snapshot = snapshot::conversation_snapshot(&SnapshotInput {
            session_id: &self.id,
            log_epoch: &self.log_epoch(),
            seq: self.op_seq(),
            revision: self.revision(),
            phase: &self.phase(),
            can_stop: self.is_running(),
            last_error: self.last_error().as_ref(),
            title: &title,
            title_source: &title_source,
            rows: &rows,
            total_row_count: self.total_row_count(),
            first_row_id: self.first_row_id(),
            usage: &usage,
            queue: self.queue_state(),
            pending_interactions: self.interactions_projection(),
            stop_requested: self.cancel.load(Ordering::SeqCst),
            followup_mode: self.followup_mode(),
            provider: self.provider(),
            model: self.model(),
            thought: self.thought(),
            collaboration_mode: self.collaboration_mode(),
            goal: self.goal_projection(),
            active_works: self.active_works(),
            workspace_hook_admission: self.hooks.admission_state().unwrap_or(Value::Null),
        });
        let payload = json!({"kind": "snapshot", "snapshot": snapshot});
        let mut guard = match self.subscribers.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut out = Vec::new();
        if let Some(subscriber) = guard
            .iter_mut()
            .find(|subscriber| subscriber.id == subscriber_id)
        {
            let to_seq = self.op_seq();
            if let Some(line) = self.encode_frame(subscriber, payload, delivery_kind, to_seq) {
                subscriber.replay.push(line.clone());
                out.push(line);
            }
        }
        out
    }

    fn encode_frame(
        &self,
        subscriber: &Subscriber,
        payload: Value,
        delivery_kind: &str,
        to_seq: u64,
    ) -> Option<String> {
        let topic = format!("conversation/{}", self.id);
        let ordinal = self.logical_frame_ordinal.fetch_add(1, Ordering::SeqCst);
        let input = TopicFrameInput {
            topic: &topic,
            subscription_id: &subscriber.id,
            delivery_kind,
            from_seq: subscriber.last_to_seq,
            to_seq,
            sent_at: now_ms(),
            payload: &payload,
            logical_frame_id: format!("{}-{ordinal}", self.id),
            logical_frame_ordinal: ordinal,
        };
        match frame::encode_frame_line(&input) {
            Ok(line) => Some(line),
            Err(error) => {
                // 信封超限是 typed error：投影侧记录并可观测，绝不静默丢帧后假装成功。
                if let Ok(mut guard) = self.last_error.lock() {
                    *guard = Some(snapshot::session_error_info(
                        "fault.frame.envelopeTooLarge",
                        &error.to_string(),
                        true,
                        "runtime",
                        now_ms(),
                    ));
                }
                None
            }
        }
    }

    pub fn is_running(&self) -> bool {
        self.phase() == "running"
    }

    /// rowsRange：按 [fromRowId, toRowId] 读行（只读查询，无状态）。
    pub fn rows_range(
        &self,
        from_row_id: Option<u64>,
        to_row_id: Option<u64>,
        limit: Option<usize>,
    ) -> Vec<Value> {
        const DEFAULT_LIMIT: usize = 60;
        let rows = self.rows_snapshot();
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(200);
        rows.into_iter()
            .filter(|row| {
                let row_id = row.get("rowId").and_then(Value::as_u64).unwrap_or(0);
                from_row_id.is_none_or(|from| row_id >= from)
                    && to_row_id.is_none_or(|to| row_id <= to)
            })
            .take(limit)
            .collect()
    }

    // ── turn 投影助手（turn.rs 调用）──

    /// 落一轮的 turnHeader（origin 由命令类型决定：普通输入 `userInput`，
    /// edit/retry 重放 `editRerun`）。
    pub fn project_turn_started(
        &self,
        turn_id: &str,
        source_command_id: &str,
        origin: &str,
    ) -> Vec<String> {
        let base = self.row_base(turn_id, &format!("turn:{turn_id}"));
        let history_round_count = self.turn_count();
        if let Ok(mut guard) = self.turn_header_facts.lock() {
            guard.insert(
                source_command_id.to_string(),
                TurnHeaderFact {
                    row_id: base.row_id,
                    started_at: base.created_at,
                    origin: origin.to_string(),
                    history_round_count,
                },
            );
        }
        // 分支截断的历史锚点：本轮开始**之前**已经落了几条消息。
        if let Ok(mut guard) = self.turn_history_starts.lock() {
            guard.push((turn_id.to_string(), self.history_len()));
        }
        let header = rows::turn_header_row(
            &base,
            rows::TurnHeaderFields {
                origin,
                state: "running",
                started_at: base.created_at,
                source_command_id,
                history_round_count,
                // 运行中的轮还没有完整的文件变更，不下发 fileChanges / canRewindFiles。
                file_changes: None,
                can_rewind_files: false,
            },
        );
        self.append_row(header.clone());
        self.project_deltas(vec![rows::row_appended(header)], false)
    }

    /// 已落轮数（`turnHeader` 记账数；不含正在落的这一轮，调用点在追加之前）。
    pub fn turn_count(&self) -> usize {
        self.turn_history_starts
            .lock()
            .map(|guard| guard.len())
            .unwrap_or(0)
    }

    /// 模型历史当前条数（分支锚点的直接来源）。锁中毒时按 0 计，与既有只读口径一致；
    /// 真正的写路径一律走 `history_snapshot` 的 fail-closed 分支。
    /// 文件变更台账（工具回合与 rewind 命令共用同一个实例）。
    pub fn file_ledger(&self) -> &Arc<crate::file_ledger::FileLedger> {
        &self.file_ledger
    }

    pub fn history_len(&self) -> usize {
        self.history.lock().map(|guard| guard.len()).unwrap_or(0)
    }

    /// 「轮 → 起始历史条数」的只读快照（分支定位用）。
    pub fn turn_history_snapshot(&self) -> Vec<(String, usize)> {
        self.turn_history_starts
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    pub fn project_turn_finished(
        &self,
        turn_id: &str,
        source_command_id: &str,
        state: &str,
    ) -> Vec<String> {
        let fact = {
            let mut guard = self.turn_header_facts.lock().ok();
            let found = guard
                .as_ref()
                .and_then(|g| g.get(source_command_id).cloned());
            if found.is_some() {
                if let Some(g) = guard.as_mut() {
                    g.remove(source_command_id);
                }
            }
            found
        }
        .or_else(|| {
            // 回源：进程在 turn 中间重启过，终态帧仍要落回原来那行。
            self.rows_snapshot()
                .iter()
                .find(|row| {
                    row.get("kind").and_then(Value::as_str) == Some("turnHeader")
                        && row.get("sourceCommandId").and_then(Value::as_str)
                            == Some(source_command_id)
                })
                .map(|row| TurnHeaderFact {
                    row_id: row.get("rowId").and_then(Value::as_u64).unwrap_or(0),
                    started_at: row.get("startedAt").and_then(Value::as_i64).unwrap_or(0),
                    origin: row
                        .get("origin")
                        .and_then(Value::as_str)
                        .unwrap_or(rows::TURN_ORIGIN_USER_INPUT)
                        .to_string(),
                    history_round_count: row
                        .get("historyRoundCount")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as usize,
                })
        })
        .unwrap_or_else(|| TurnHeaderFact {
            row_id: 0,
            started_at: now_ms(),
            origin: rows::TURN_ORIGIN_USER_INPUT.to_string(),
            history_round_count: 0,
        });
        let base = RowBase {
            row_id: fact.row_id,
            turn_id: turn_id.to_string(),
            entity_id: format!("turn:{turn_id}"),
            created_at: now_ms(),
            created_at_seq: self.next_op_seq(),
        };
        // 本轮的文件变更汇总：台账里有就下发（客户端据此显示"本轮改了 N 个文件"
        // 并允许回滚），没有就不带这个键——写一个假的零值等于给 UI 一个按下去
        // 什么都不会发生的按钮。
        let summary = self.file_ledger.summary_for_turn(turn_id);
        let header = rows::turn_header_row(
            &base,
            rows::TurnHeaderFields {
                origin: &fact.origin,
                state,
                started_at: fact.started_at,
                source_command_id,
                history_round_count: fact.history_round_count,
                file_changes: summary.as_ref(),
                can_rewind_files: summary.is_some(),
            },
        );
        self.upsert_row(header.clone());
        self.project_deltas(vec![rows::row_upserted(header)], false)
    }

    pub fn project_user_input(
        &self,
        turn_id: &str,
        text: &str,
        source_command_id: &str,
        client_id: &str,
    ) -> Vec<String> {
        let base = self.row_base(turn_id, &format!("ui:{source_command_id}"));
        let row = rows::user_input_row(&base, text, source_command_id, client_id);
        self.append_row(row.clone());
        self.project_deltas(vec![rows::row_appended(row)], false)
    }

    /// assistant 文本行：创建（streaming）→ delta 追加 → 终态 upsert。
    pub fn project_assistant_row(
        &self,
        turn_id: &str,
        response_id: &str,
        model: Option<&str>,
    ) -> u64 {
        let base = self.row_base(turn_id, response_id);
        let row = rows::assistant_text_row(&base, response_id, "", "streaming", model);
        let row_id = base.row_id;
        self.append_row(row.clone());
        self.project_deltas(vec![rows::row_appended(row)], false);
        row_id
    }

    pub fn project_assistant_delta(&self, row_id: u64, append: &str) -> Vec<String> {
        self.project_deltas(vec![rows::row_delta(row_id, "text", append)], false)
    }

    /// reasoning 行：open（streaming）→ 若干 row.delta(path=text) → 终态 upsert。
    /// TS 的 openReasoningRow/closeReasoningRow 同形（assistantResponseId 归组）。
    pub fn project_reasoning_start(&self, turn_id: &str, response_id: &str) -> (u64, Vec<String>) {
        let base = self.row_base(turn_id, &format!("reasoning:{response_id}"));
        let row_id = base.row_id;
        let row = rows::reasoning_row(&base, response_id, "", "streaming", None);
        self.append_row(row.clone());
        (
            row_id,
            self.project_deltas(vec![rows::row_appended(row)], false),
        )
    }

    pub fn project_reasoning_delta(&self, row_id: u64, append: &str) -> Vec<String> {
        self.project_deltas(vec![rows::row_delta(row_id, "text", append)], false)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn project_reasoning_final(
        &self,
        row_id: u64,
        turn_id: &str,
        response_id: &str,
        text: &str,
        state: &str,
        duration_ms: Option<i64>,
    ) -> Vec<String> {
        let base = RowBase {
            row_id,
            turn_id: turn_id.to_string(),
            entity_id: format!("reasoning:{response_id}"),
            created_at: now_ms(),
            created_at_seq: self.next_op_seq(),
        };
        let row = rows::reasoning_row(&base, response_id, text, state, duration_ms);
        self.upsert_row(row.clone());
        self.project_deltas(vec![rows::row_upserted(row)], false)
    }

    /// toolCall 行：running → 终态（success/error/cancelled）两次投影；
    /// `inputText` 是流式路径，continuous 档随窗口 flush，终态 upsert 携全量。
    pub fn project_tool_call(
        &self,
        turn_id: &str,
        tool_call_id: &str,
        tool_name: &str,
        status: &str,
        input_text: &str,
        error: Option<(&str, &str)>,
    ) -> Vec<String> {
        let existing_row_id = self
            .rows_snapshot()
            .iter()
            .find(|row| {
                row.get("kind").and_then(Value::as_str) == Some("toolCall")
                    && row.get("toolCallId").and_then(Value::as_str) == Some(tool_call_id)
            })
            .and_then(|row| row.get("rowId").and_then(Value::as_u64));
        let base = match existing_row_id {
            Some(row_id) => RowBase {
                row_id,
                turn_id: turn_id.to_string(),
                entity_id: tool_call_id.to_string(),
                created_at: now_ms(),
                created_at_seq: self.next_op_seq(),
            },
            None => self.row_base(turn_id, tool_call_id),
        };
        let row = rows::tool_call_row(&base, tool_call_id, tool_name, status, input_text, error);
        if existing_row_id.is_some() {
            self.upsert_row(row.clone());
            self.project_deltas(vec![rows::row_upserted(row)], false)
        } else {
            self.append_row(row.clone());
            self.project_deltas(vec![rows::row_appended(row)], false)
        }
    }

    pub fn project_assistant_final(
        &self,
        row_id: u64,
        response_id: &str,
        text: &str,
        state: &str,
        model: Option<&str>,
    ) -> Vec<String> {
        let turn_id = self
            .rows_snapshot()
            .iter()
            .find(|row| row.get("rowId").and_then(Value::as_u64) == Some(row_id))
            .and_then(|row| {
                row.get("turnId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default();
        let base = RowBase {
            row_id,
            entity_id: response_id.to_string(),
            turn_id,
            created_at: now_ms(),
            created_at_seq: self.next_op_seq(),
        };
        let row = rows::assistant_text_row(&base, response_id, text, state, model);
        self.upsert_row(row.clone());
        self.project_deltas(vec![rows::row_upserted(row)], true)
    }
}

/// 行的 kind 判定（避免各处重复字面量比较）。
fn row_kind_is(row: &Value, kind: &str) -> bool {
    row.get("kind").and_then(Value::as_str) == Some(kind)
}

/// 被截掉行里涉及的 turnId 集合。
fn dropped_turn_ids(rows: &[Value]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for row in rows {
        if let Some(turn_id) = row.get("turnId").and_then(Value::as_str) {
            if !ids.iter().any(|existing| existing == turn_id) {
                ids.push(turn_id.to_string());
            }
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SessionMeta;
    use crate::store::TranscriptStore;
    use zcode_provider::request::{ContentPart, MessageRole};

    /// 测试 store 根目录带 pid 后缀：`--test-threads` 并行跑同一二进制时，固定名字
    /// 会互相 `remove_dir_all`（这正是旧版把崩溃残留静默清掉的隐患）。
    fn test_store(id: &str) -> TranscriptStore {
        TranscriptStore::new(
            std::env::temp_dir().join(format!("zcode-host-test-{id}-{}", std::process::id())),
        )
    }

    fn test_hook_admission(id: &str) -> Arc<crate::workspace_hooks::WorkspaceHookAdmission> {
        // 测试不读工作区：空快照 + 独立台账文件，评审流程保持 idle。
        let base = std::env::temp_dir().join(format!("hook-admission-{id}"));
        let store = std::sync::Arc::new(crate::workspace_hooks::TrustStore::new(
            base.join(crate::workspace_hooks::TRUST_STORE_FILE),
        ));
        std::sync::Arc::new(crate::workspace_hooks::WorkspaceHookAdmission::new(
            base,
            format!("/ws/{id}"),
            id.to_string(),
            store,
        ))
    }

    fn test_session(id: &str) -> Arc<Session> {
        let store = test_store(id);
        let _ = std::fs::remove_dir_all(store.session_dir(id));
        // 与 `create_session` 同一形状：transcript.jsonl 必须先存在，否则第一次
        // 追加会因文件缺失失败（Unix) 或依赖 append 创建（Windows）——两种平台
        // 行为不一致正是这里要消灭的。
        store
            .create_session(&SessionMeta {
                session_id: id.to_string(),
                title: String::new(),
                title_source: "default".to_string(),
                log_epoch: "epoch-1".to_string(),
                revision: 0,
                workspace_identity: format!("/ws/{id}"),
                created_at: 0,
                followup_mode: "queue".to_string(),
                collaboration_mode: "build".to_string(),
                provider: String::new(),
                model: String::new(),
                thought: String::new(),
                goal: None,
            })
            .expect("create session dir");
        Session::new(SessionInit {
            id: id.to_string(),
            workspace_root: std::env::temp_dir().join(format!("ws-{id}")),
            workspace_identity: format!("/ws/{id}"),
            log_epoch: "epoch-1".to_string(),
            title: String::new(),
            title_source: "default".to_string(),
            rows: Vec::new(),
            revision: 0,
            transcript_acks: HashMap::new(),
            next_row_id: 0,
            history: Vec::new(),
            turn_history_starts: Vec::new(),
            file_changes: Vec::new(),
            followup_mode: "queue".to_string(),
            collaboration_mode: "build".to_string(),
            provider: String::new(),
            model: String::new(),
            thought: String::new(),
            goal: None,
            hooks: test_hook_admission(id),
            store,
        })
    }

    #[test]
    fn rows_assigned_to_no_subscriber_are_buffered_not_emitted() {
        let session = test_session("monotonic");
        let lines = session.project_user_input("t1", "a", "cmd-1", "c1");
        assert!(
            lines.is_empty(),
            "no subscriber: projection is buffered, not emitted"
        );
        let rows = session.rows_snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["rowId"], json!(1));
    }

    #[test]
    fn set_phase_rejects_values_outside_schema_enum() {
        let session = test_session("phase");
        assert_eq!(session.phase(), "draft");
        // "idle" 不在 sessionPhaseSchema 闭集里（旧 run_compact 会写它）。
        session.set_phase("idle");
        assert_eq!(session.phase(), "draft", "invalid phase must be rejected");
        session.set_phase("running");
        assert_eq!(session.phase(), "running");
        for valid in [
            "prewarming",
            "completedSuccess",
            "completedInterrupted",
            "error",
            "draft",
        ] {
            session.set_phase(valid);
            assert_eq!(session.phase(), valid);
        }
    }

    #[test]
    fn resubscribing_same_id_replaces_and_does_not_double_emit() {
        let session = test_session("resub");
        let id = "conn:conversation/resub".to_string();
        session.add_subscriber(id.clone(), DeliveryProfileName::Continuous);
        session.add_subscriber(id, DeliveryProfileName::Continuous);
        let lines = session.project_deltas(
            vec![json!({"op": "state.updated", "patch": {"revision": 1}})],
            true,
        );
        assert_eq!(
            lines.len(),
            1,
            "re-subscribing the same id must replace, not duplicate the subscriber"
        );
    }

    #[test]
    fn assistant_stream_then_final_is_idempotent() {
        let session = test_session("stream");
        session.project_user_input("t1", "hi", "cmd-1", "c1");
        let row_id = session.project_assistant_row("t1", "resp-1", Some("m"));
        session.project_assistant_delta(row_id, "he");
        session.project_assistant_delta(row_id, "llo");
        session.project_assistant_final(row_id, "resp-1", "hello", "complete", Some("m"));
        let rows = session.rows_snapshot();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["text"], json!("hello"));
        assert_eq!(rows[1]["state"], json!("complete"));
    }

    #[test]
    fn rows_range_is_bounded() {
        let session = test_session("range");
        for index in 0..5 {
            session.project_user_input("t1", &format!("m{index}"), "cmd", "c1");
        }
        assert_eq!(session.rows_range(Some(2), None, None).len(), 4);
        assert_eq!(session.rows_range(None, Some(2), None).len(), 2);
    }

    // ── session memory（specs/rust-native-crates.md S3 规则 1-4）──

    #[test]
    fn history_accumulates_across_turns_instead_of_restarting() {
        let session = test_session("history-accumulates");
        for text in ["turn one", "answer one", "turn two"] {
            let role = if text == "answer one" {
                MessageRole::Assistant
            } else {
                MessageRole::User
            };
            session
                .append_history(Message::text(role, text))
                .expect("append");
        }

        // 规则 1：第二轮的快照必须带上第一轮的问答，而不是只剩最后一条输入。
        let snapshot = session.history_snapshot().expect("history readable");
        assert_eq!(snapshot.len(), 3);
        let texts: Vec<String> = snapshot
            .iter()
            .map(|message| {
                message
                    .parts
                    .iter()
                    .map(|part| match part {
                        ContentPart::Text { text } => text.clone(),
                        _ => String::new(),
                    })
                    .collect::<String>()
            })
            .collect();
        assert_eq!(texts, vec!["turn one", "answer one", "turn two"]);
    }

    #[test]
    fn history_survives_restart_via_transcript_replay() {
        let session = test_session("history-restart");
        session
            .append_history(Message::text(MessageRole::User, "remember this"))
            .expect("append user");
        let mut with_tool = Message::text(MessageRole::Tool, "wrote 3 bytes");
        with_tool.tool_call_id = Some("toolu_1".to_string());
        session.append_history(with_tool).expect("append tool");
        session
            .append_history(Message::text(MessageRole::Assistant, "done"))
            .expect("append assistant");

        // 规则 2/4：重新打开 store 必须逐字段还原同一份历史（含 tool_call_id），
        // 否则重启后模型会丢掉工具配对。
        let replayed = test_store("history-restart")
            .load_transcript("history-restart")
            .expect("transcript replay")
            .history;
        assert_eq!(replayed.len(), 3);
        assert_eq!(replayed[0].role, MessageRole::User);
        assert_eq!(replayed[1].role, MessageRole::Tool);
        assert_eq!(
            replayed[1].tool_call_id.as_deref(),
            Some("toolu_1"),
            "tool_call_id must survive the roundtrip"
        );
        assert_eq!(replayed[2].role, MessageRole::Assistant);
    }

    /// 分支截断必须把**被截掉那一轮自己的锚点**也删掉。
    ///
    /// WHY 这条用例存在：目标轮的起始点正好等于保留长度 `history_keep`，早先只用
    /// `start < history_keep` 过滤会把它的锚点留下；下一次 edit/retry 于是会对一个
    /// 已经不存在的轮做截断，历史会被切到错误位置（回放侧同一 bug 由
    /// `benches::replay_transcript_after_branch_cut` 钉住）。
    /// 台账的两条 durable 性质：随 transcript 落盘（重启后还能 rewind），
    /// 并且分支截断时**磁盘与内存一起裁**（否则重启会把已放弃的轮次又变成可回滚）。
    #[test]
    fn file_ledger_is_durable_and_pruned_with_the_branch() {
        let session = test_session("ledger-durable");
        for (index, (turn_id, command_id, path)) in
            [("t1", "cmd-1", "a.txt"), ("t2", "cmd-2", "b.txt")]
                .iter()
                .enumerate()
        {
            session.project_turn_started(turn_id, command_id, rows::TURN_ORIGIN_USER_INPUT);
            session.project_user_input(turn_id, &format!("q{index}"), command_id, "c1");
            session
                .append_history(Message::text(MessageRole::User, format!("q{index}")))
                .expect("history");
            session
                .file_ledger()
                .record(crate::file_ledger::FileChangeRecord {
                    turn_id: turn_id.to_string(),
                    tool_call_id: format!("call-{turn_id}"),
                    tool_name: "Write".to_string(),
                    path: path.to_string(),
                    existed_before: false,
                    before_content: None,
                    after_content: Some("written".to_string()),
                    additions: 1,
                    deletions: 0,
                })
                .expect("ledger record persists");
        }

        let transcript = session
            .store
            .load_transcript(&session.id)
            .expect("transcript readable");
        assert_eq!(
            transcript.file_changes.len(),
            2,
            "每一轮的文件变更都必须落进 transcript"
        );
        assert_eq!(transcript.turn_history_starts.len(), 2);

        // 截掉第二轮（它的 turnHeader 是 rowId 2，历史保留 1 条）。
        let header_t2 = transcript
            .rows
            .iter()
            .find(|row| {
                row.get("kind").and_then(Value::as_str) == Some("turnHeader")
                    && row.get("turnId").and_then(Value::as_str) == Some("t2")
            })
            .and_then(|row| row.get("rowId").and_then(Value::as_u64))
            .expect("t2 header");
        session.truncate_branch(header_t2, 1).expect("truncate");

        let after = session
            .store
            .load_transcript(&session.id)
            .expect("transcript readable");
        assert_eq!(
            after.file_changes.len(),
            1,
            "被截断轮次的文件记录必须从磁盘消失，否则重启后又变成可回滚"
        );
        assert_eq!(after.file_changes[0].turn_id, "t1");
        assert_eq!(
            session.file_ledger().for_turn("t2").len(),
            0,
            "内存同样裁掉"
        );
        assert_eq!(session.file_ledger().for_turn("t1").len(), 1);

        // 冷恢复：从 transcript 重建的会话必须带回剩下的那条记录与锚点。
        let resumed = Session::new(SessionInit {
            id: session.id.clone(),
            workspace_root: session.workspace_root.clone(),
            workspace_identity: session.workspace_identity.clone(),
            log_epoch: session.log_epoch(),
            title: String::new(),
            title_source: "default".to_string(),
            rows: after.rows,
            revision: 0,
            transcript_acks: HashMap::new(),
            next_row_id: 4,
            history: after.history,
            turn_history_starts: after.turn_history_starts,
            file_changes: after.file_changes,
            followup_mode: "queue".to_string(),
            collaboration_mode: "build".to_string(),
            provider: String::new(),
            model: String::new(),
            thought: String::new(),
            goal: None,
            hooks: test_hook_admission("resume"),
            store: session.store.clone(),
        });
        assert_eq!(resumed.file_ledger().records().len(), 1);
        assert_eq!(
            resumed
                .file_ledger()
                .summary_for_turn("t1")
                .map(|s| s.files),
            Some(1),
            "重启后仍能算出该轮改了 1 个文件（rewind 的门禁依赖它）"
        );
    }

    #[test]
    fn truncate_branch_prunes_the_truncated_turn_anchor() {
        let session = test_session("branch-prune");
        // 两轮，每轮 2 条历史。
        for (turn_id, command_id, text) in [
            ("t1", "cmd-1", "question one"),
            ("t2", "cmd-2", "question two"),
        ] {
            session.project_turn_started(turn_id, command_id, rows::TURN_ORIGIN_USER_INPUT);
            session.project_user_input(turn_id, text, command_id, "c1");
            session
                .append_history(Message::text(MessageRole::User, text))
                .expect("history");
            session
                .append_history(Message::text(MessageRole::Assistant, "answer"))
                .expect("history");
        }
        assert_eq!(
            session.turn_history_snapshot(),
            vec![("t1".to_string(), 0usize), ("t2".to_string(), 2usize)]
        );
        let header_t2 = session
            .rows_snapshot()
            .iter()
            .find(|row| {
                row.get("kind").and_then(Value::as_str) == Some("turnHeader")
                    && row.get("turnId").and_then(Value::as_str) == Some("t2")
            })
            .and_then(|row| row.get("rowId").and_then(Value::as_u64))
            .expect("t2 header row");

        session.truncate_branch(header_t2, 2).expect("truncate");

        assert_eq!(
            session.turn_history_snapshot(),
            vec![("t1".to_string(), 0usize)],
            "the truncated turn must not leave an anchor behind"
        );
        assert_eq!(
            session.history_len(),
            2,
            "history cut back to the kept prefix"
        );
        let surviving_turns: Vec<String> = session
            .rows_snapshot()
            .iter()
            .filter(|row| row.get("kind").and_then(Value::as_str) == Some("turnHeader"))
            .filter_map(|row| row.get("turnId").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        assert_eq!(surviving_turns, vec!["t1".to_string()]);
        // 磁盘侧同样只剩 t1：冷恢复不会把已放弃的分支放回来（回放裁剪与内存裁剪
        // 必须是同一条规则，否则第二次 edit 会拿到一个只在磁盘上存在的轮）。
        let transcript = session
            .store
            .load_transcript(&session.id)
            .expect("transcript readable");
        assert_eq!(
            transcript.turn_history_starts,
            vec![("t1".to_string(), 0usize)],
            "replay must prune the truncated turn's anchor as well"
        );
        assert_eq!(
            transcript.history.len(),
            2,
            "durable history cut to the prefix"
        );
        assert_eq!(
            transcript
                .rows
                .iter()
                .filter(|row| row.get("kind").and_then(Value::as_str) == Some("turnHeader"))
                .count(),
            1,
            "only the surviving turn's header is replayed"
        );
    }

    #[test]
    fn replace_history_is_the_only_way_messages_disappear() {
        let session = test_session("history-replace");
        for text in ["a", "b", "c"] {
            session
                .append_history(Message::text(MessageRole::User, text))
                .expect("append");
        }
        session
            .replace_history(&[Message::text(MessageRole::User, "summary")])
            .expect("replace");
        let snapshot = session.history_snapshot().expect("history readable");
        assert_eq!(snapshot.len(), 1, "compaction replaced the whole history");

        // 替换之后的新消息继续追加在压缩结果之后。
        session
            .append_history(Message::text(MessageRole::User, "next"))
            .expect("append after compaction");
        assert_eq!(session.history_snapshot().expect("readable").len(), 2);
    }

    /// 真正打中 fail-closed 分支（review MINOR-1：旧用例只断言了成功路径，
    /// 于是 MAJOR-1 才能一路溜过）。
    ///
    /// 造真实的写失败：删掉 `transcript.jsonl`——`append_entry` 用 `.append(true)`
    /// 且**没有** `.create(true)`，文件缺失时每次写都失败。断言三件事：
    /// 写返回 Err（调用方能 fail-closed）、消息**不**进内存、内存不偏离磁盘。
    #[test]
    fn failed_history_write_reports_error_and_keeps_memory_in_sync_with_disk() {
        let session = test_session("history-write-fail");
        let store = test_store("history-write-fail");
        let transcript_path = store
            .session_dir("history-write-fail")
            .join("transcript.jsonl");
        std::fs::remove_file(&transcript_path).expect("remove transcript to force a write failure");

        let result = session.append_history(Message::text(MessageRole::User, "never stored"));
        assert!(
            result.is_err(),
            "a failed durable append must be observable so the turn can fail closed"
        );
        assert!(
            session.history_snapshot().expect("readable").is_empty(),
            "a message that never reached disk must not enter memory"
        );
        // 缺失文件现在走容错通路：返回空 transcript 但**带** skipped_lines，
        // 这样 resume 的诊断才会响（review round2 MAJOR-2 的第二扇门）。
        let replayed = store
            .load_transcript("history-write-fail")
            .expect("load must not fail wholesale");
        assert!(
            replayed.history.is_empty(),
            "nothing was ever written, so nothing replays"
        );
        assert_eq!(
            replayed.skipped_lines.len(),
            1,
            "a missing transcript must be reported, not silent: {:?}",
            replayed.skipped_lines
        );
    }

    /// 压缩写失败必须可观测（review round2 NEW MAJOR-A）：`replace_history` 与
    /// `append_history` 是同一条不变量的两个出口，静默吞掉会让磁盘永久停在
    /// 压缩前的历史、内存停在压缩后，两者分叉且无信号。
    #[test]
    fn failed_replace_history_reports_error_and_keeps_memory_in_sync() {
        let session = test_session("history-replace-fail");
        let store = test_store("history-replace-fail");
        session
            .append_history(Message::text(MessageRole::User, "one"))
            .expect("append");
        std::fs::remove_file(
            store
                .session_dir("history-replace-fail")
                .join("transcript.jsonl"),
        )
        .expect("remove transcript to force a write failure");

        let result = session.replace_history(&[Message::text(MessageRole::User, "summary")]);
        assert!(
            result.is_err(),
            "a failed compaction write must be observable"
        );
        assert_eq!(
            session.history_snapshot().expect("readable").len(),
            1,
            "memory must keep the pre-compaction history that disk still has"
        );
    }

    /// 历史锁中毒必须**失败**而不是返回空历史（review round2 MINOR-C）：
    /// 返回 `Vec::new()` 会让 turn 拿空上下文去请求模型——正是这条防线要防的事。
    #[test]
    fn poisoned_history_lock_fails_instead_of_fabricating_empty_history() {
        use std::sync::Mutex;

        // 直接测被毒化的 Mutex 上的取值形状。`Mutex` 不可重入，所以在**另一个线程**里
        // panic——同一线程第二次 `lock()` 是阻塞等待而不是毒化，用例会死锁（第一版就
        // 是这么挂住的）。
        let poisoned: Arc<Mutex<Vec<Message>>> = Arc::new(Mutex::new(Vec::new()));
        let for_panic = Arc::clone(&poisoned);
        let handle = std::thread::spawn(move || {
            let _guard = for_panic.lock().expect("lock in panicking thread");
            panic!("poison the history lock on purpose");
        });
        assert!(handle.join().is_err(), "the thread must have panicked");
        // 这正是 `Session::history_snapshot` 的取值形状：中毒 → Err，绝不是空历史
        // （空历史会让 turn 拿零上下文去请求模型）。
        assert!(poisoned.lock().map(|guard| guard.clone()).is_err());
    }

    /// 容错回放（review MAJOR-2）：一行坏、其余好时，坏行被跳过并计数，**不**
    /// 静默清空整份历史。
    #[test]
    fn corrupt_line_is_skipped_and_counted_instead_of_erasing_history() {
        let session = test_session("history-tolerate");
        let store = test_store("history-tolerate");
        session
            .append_history(Message::text(MessageRole::User, "first"))
            .expect("append first");
        session
            .append_history(Message::text(MessageRole::Assistant, "second"))
            .expect("append second");

        // 追加一行截断的 JSON（模拟崩溃写到一半），再追加一行未来标签的条目
        // （模拟旧二进制读新版本写的文件）。
        let path = store
            .session_dir("history-tolerate")
            .join("transcript.jsonl");
        let mut raw = std::fs::read_to_string(&path).expect("read transcript");
        raw.push_str("{\"t\":\"history\",\"message\":{\"role\":\"user\",\n");
        raw.push_str("{\"t\":\"futureThingFromANewerBinary\",\"payload\":{}}\n");
        std::fs::write(&path, raw).expect("write corrupt lines");

        let replayed = store
            .load_transcript("history-tolerate")
            .expect("replay must not fail wholesale");
        assert_eq!(
            replayed.history.len(),
            2,
            "the two intact messages must survive: {:?}",
            replayed.history
        );
        assert_eq!(
            replayed.skipped_lines.len(),
            2,
            "both bad lines must be reported, not swallowed: {:?}",
            replayed.skipped_lines
        );
    }

    /// 容错回放 + 压缩的组合：压缩事件之后出现坏行时，**不能**把压缩前的旧消息
    /// 复活（那会是一条「看起来合理但已经作废」的历史，比丢消息更危险）。
    #[test]
    fn corrupt_line_after_compaction_does_not_resurrect_pre_compaction_history() {
        let session = test_session("history-compact-corrupt");
        let store = test_store("history-compact-corrupt");
        for text in ["old one", "old two", "old three"] {
            session
                .append_history(Message::text(MessageRole::User, text))
                .expect("append");
        }
        session
            .replace_history(&[
                Message::text(MessageRole::User, "summary of old three"),
                Message::text(MessageRole::User, "current"),
            ])
            .expect("replace history");

        let path = store
            .session_dir("history-compact-corrupt")
            .join("transcript.jsonl");
        let mut raw = std::fs::read_to_string(&path).expect("read transcript");
        raw.push_str("{\"t\":\"history\",\"message\":{\"role\":\"user\",\n");
        std::fs::write(&path, raw).expect("write corrupt line");

        let replayed = store
            .load_transcript("history-compact-corrupt")
            .expect("replay must not fail wholesale");
        assert_eq!(
            replayed.skipped_lines.len(),
            1,
            "the corrupt line is reported"
        );
        let texts: Vec<String> = replayed
            .history
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec!["summary of old three", "current"],
            "compaction must stay applied; pre-compaction messages must not come back"
        );
    }

    /// CRLF 容忍：Windows 编辑器写出的 transcript 行末带 `\\r` 时，行仍能被正确解析，
    /// 且 `skipped_lines` 为空（不是把整行当成损坏）。
    #[test]
    fn crlf_terminated_lines_are_parsed_not_counted_as_corrupt() {
        let store = test_store("crlf-lines");
        let meta = SessionMeta {
            session_id: "crlf-lines".to_string(),
            title: String::new(),
            title_source: "default".to_string(),
            log_epoch: "epoch".to_string(),
            revision: 0,
            workspace_identity: "/tmp/ws".to_string(),
            created_at: 0,
            followup_mode: "queue".to_string(),
            collaboration_mode: "build".to_string(),
            provider: String::new(),
            model: String::new(),
            thought: String::new(),
            goal: None,
        };
        store.create_session(&meta).expect("create session");

        // 手工写入带 CRLF 的 transcript（模拟 Windows 编辑器）。
        let path = store.session_dir("crlf-lines").join("transcript.jsonl");
        let line = "{\"t\":\"row\",\"row\":{\"rowId\":1,\"kind\":\"x\"}}\r\n";
        std::fs::write(&path, line).expect("write crlf transcript");

        let transcript = store.load_transcript("crlf-lines").expect("replay");
        assert!(
            transcript.skipped_lines.is_empty(),
            "CRLF line must not be counted as corrupt: {:?}",
            transcript.skipped_lines
        );
        assert_eq!(transcript.rows.len(), 1, "the row must be recovered");
    }

    #[test]
    fn usage_report_matches_the_protocol_shape_and_counts_requests() {
        let session = test_session("usage-report");
        assert_eq!(session.usage_report()["modelRequestCount"], json!(0));
        session.record_model_request();
        session.record_model_request();
        session.record_model_error();
        session.accumulate_usage(&json!({
            "inputTokens": 10,
            "outputTokens": 4,
            "reasoningTokens": 3,
            "cacheReadTokens": 2,
            "cacheWriteTokens": 1,
        }));
        session.accumulate_usage(&json!({"inputTokens": 5, "outputTokens": 1}));
        let report = session.usage_report();
        assert_eq!(report["sessionId"], json!(session.id));
        assert_eq!(report["inputTokens"], json!(15));
        assert_eq!(report["outputTokens"], json!(5));
        assert_eq!(report["reasoningTokens"], json!(3));
        assert_eq!(report["cacheReadTokens"], json!(2));
        assert_eq!(report["cacheCreationTokens"], json!(1));
        assert_eq!(report["totalTokens"], json!(20));
        assert_eq!(report["modelRequestCount"], json!(2));
        assert_eq!(report["modelErrorCount"], json!(1));
        assert_eq!(report["inputBaselineBySource"], json!({}));
    }

    #[test]
    fn usage_accumulates_across_rounds_instead_of_overwriting() {
        let session = test_session("usage-acc");
        session.accumulate_usage(&json!({
            "inputTokens": 10, "outputTokens": 2, "cacheReadTokens": 1, "cacheWriteTokens": 0
        }));
        session.accumulate_usage(&json!({
            "inputTokens": 5, "outputTokens": 3, "cacheReadTokens": 0, "cacheWriteTokens": 4
        }));
        let usage = session.usage();
        assert_eq!(usage["cumulative"]["inputTokens"], json!(15));
        assert_eq!(usage["cumulative"]["outputTokens"], json!(5));
        assert_eq!(usage["cumulative"]["cacheReadTokens"], json!(1));
        assert_eq!(usage["cumulative"]["cacheWriteTokens"], json!(4));
        assert_eq!(usage["contextWindow"], Value::Null);
    }

    #[test]
    fn assistant_feedback_sets_clears_and_rejects_wrong_kind() {
        let session = test_session("feedback");
        session.append_row(json!({
            "rowId": 7, "kind": "assistantText", "text": "hi", "state": "complete"
        }));
        // 设置 like
        session
            .apply_assistant_feedback(7, Some("like"))
            .expect("assistantText row accepts feedback");
        let row = session
            .rows_snapshot()
            .into_iter()
            .find(|row| row["rowId"] == json!(7))
            .expect("row present");
        assert_eq!(row["feedback"], json!("like"));

        // null → 清除字段
        session
            .apply_assistant_feedback(7, None)
            .expect("null clears feedback");
        let row = session
            .rows_snapshot()
            .into_iter()
            .find(|row| row["rowId"] == json!(7))
            .expect("row present");
        assert!(row.get("feedback").is_none(), "feedback must be removed");

        // 非 assistantText → 拒绝
        session.append_row(json!({"rowId": 8, "kind": "userInput", "text": "x"}));
        assert!(session.apply_assistant_feedback(8, Some("like")).is_err());

        // 不存在的行 → 拒绝
        assert!(session.apply_assistant_feedback(999, Some("like")).is_err());
    }
}
