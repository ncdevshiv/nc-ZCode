//! CommandInbox：镜像 `apps/zcode-cli/packages/bootstrap/src/zcode-protocol-v4/command-inbox.ts`
//! 的串行 admission 语义。
//!
//! 所有权（唯一状态所有者）：[`CommandInbox`] 内部 `Mutex<InboxState>` + `Condvar`。
//! TS 调用点是**单 gateway 实例被并发 `handleCommand` 复用**（`v4-gateway.ts` 多任务
//! 同时 await `inbox.handle`，settle 由执行完成方回调），即共享所有权——因此本实现
//! 全部方法取 `&self`（互斥在内部），跨任务共享用 [`SharedCommandInbox`]（`Arc`）。
//! 与 `&mut self` 单所有者方案的取舍见 README。
//!
//! 事件面（TS 无 emitter，host 订阅面就是这些）：
//! - `handle` → [`Outcome::Ack`]（拒绝/重复/noop，立即收口）或 [`Outcome::Execute`]
//!   （admitted：携带 admissionSeq / admittedAt / queueItemId / settle 凭证）；
//! - `settle` 收口在途事实并释放 session gate → 唤醒 FIFO 队头与 duplicate/query 等待者；
//! - `query` 对在途 key 等待终态（TS `await inflight.final`）；
//! - 排队中的 pending 在 TS 不可观测，这里额外暴露 [`CommandInbox::pending_command_ids`]
//!   与 [`CommandInbox::waiter_stats`] 作为 Rust 侧诊断（不改变协议行为）。

use std::collections::{HashMap, VecDeque};
use std::ops::Deref;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use serde_json::Value;

use crate::protocol::{
    extract_command_id, merge_final, parse_command_envelope, requires_base_revision, CommandAck,
    CommandEnvelope, CommandFinal, CommandInboxHost, CommandKey, CommandStatus, GuardDecision,
    CREATE_SESSION_TYPE, GLOBAL_BUCKET, IDEMPOTENCY_TABLE_PER_SESSION, REASON_ADMISSION_CANCELLED,
    REASON_INVALID_PAYLOAD, REASON_MISSING_BASE_REVISION, REASON_SESSION_NOT_FOUND,
    REASON_STALE_LOG_EPOCH, REASON_STALE_REVISION,
};

/// TS `queueItemIdForCommand`：`queue_${commandId}`。
pub fn queue_item_id_for_command(command_id: &str) -> String {
    format!("queue_{command_id}")
}

// ── 内部状态 ──

#[derive(Debug, Clone)]
struct InFlightEntry {
    /// admission 时的决策 ACK（accepted）；settle 以此为底做终态合并。
    ack: CommandAck,
    /// run token：settle 凭证；伪造/过期 token 被 stale-run 防护拒绝。
    run_token: u64,
}

#[derive(Debug, Clone)]
struct SettledEntry {
    ack: CommandAck,
    /// settle 幂等墓碑：同 token 重复 settle → noop（TS closure 的 settled 标志）。
    run_token: Option<u64>,
}

#[derive(Debug, Clone)]
struct LiveInputEntry {
    ack: CommandAck,
    run_token: Option<u64>,
}

/// session FIFO 排队条目（TS 的 AsyncGateRegistry 承诺链在这里显式化）。
#[derive(Debug, Clone)]
struct PendingEntry {
    /// 每次 `handle` 调用的唯一 id：同 commandId 的并发提交者不入队（key-gate 语义）。
    waiter_id: u64,
    command_id: String,
    cancelled: bool,
}

#[derive(Debug)]
struct InboxState {
    in_flight: HashMap<String, HashMap<String, InFlightEntry>>,
    live_inputs: HashMap<String, HashMap<String, LiveInputEntry>>,
    /// 每桶保序幂等表（TS Map 插入序 = LRU 淘汰序）；上限 512。
    settled: HashMap<String, Vec<(String, SettledEntry)>>,
    admission_seq: HashMap<String, u64>,
    pending: HashMap<String, VecDeque<PendingEntry>>,
    /// 正在等待在途终态的 handle/query 调用数（诊断）。
    final_waiters: usize,
    next_waiter_id: u64,
    next_run_token: u64,
}

impl InboxState {
    fn new() -> Self {
        Self {
            in_flight: HashMap::new(),
            live_inputs: HashMap::new(),
            settled: HashMap::new(),
            admission_seq: HashMap::new(),
            pending: HashMap::new(),
            final_waiters: 0,
            next_waiter_id: 0,
            next_run_token: 0,
        }
    }
}

fn bucket_key(session_id: Option<&str>) -> String {
    session_id
        .map(str::to_owned)
        .unwrap_or_else(|| GLOBAL_BUCKET.to_owned())
}

fn in_flight_of<'a>(
    st: &'a InboxState,
    bucket: &str,
    command_id: &str,
) -> Option<&'a InFlightEntry> {
    st.in_flight.get(bucket).and_then(|m| m.get(command_id))
}

fn has_in_flight(st: &InboxState, bucket: &str, command_id: &str) -> bool {
    in_flight_of(st, bucket, command_id).is_some()
}

fn bucket_busy(st: &InboxState, bucket: &str) -> bool {
    st.in_flight.get(bucket).is_some_and(|m| !m.is_empty())
}

/// TS `retryAck`：failed 是终态事实保留原状态，其余一律改写为 duplicate。
fn retry_ack(ack: &CommandAck) -> CommandAck {
    if ack.status == CommandStatus::Failed {
        ack.clone()
    } else {
        let mut next = ack.clone();
        next.status = CommandStatus::Duplicate;
        next
    }
}

/// settled 保序表：命中即 touch（删旧插新），保持 LRU 淘汰序与 TS Map 一致。
fn settled_take(st: &mut InboxState, bucket: &str, command_id: &str) -> Option<CommandAck> {
    let list = st.settled.get_mut(bucket)?;
    let pos = list.iter().position(|(id, _)| id == command_id)?;
    // touch：删旧插新保持 LRU 序；条目整体移回，仅克隆返回用的 ack。
    let (_, entry) = list.remove(pos);
    let ack = entry.ack.clone();
    list.push((command_id.to_owned(), entry));
    Some(ack)
}

fn settled_token(st: &InboxState, bucket: &str, command_id: &str) -> Option<Option<u64>> {
    st.settled
        .get(bucket)?
        .iter()
        .find(|(id, _)| id == command_id)
        .map(|(_, e)| e.run_token)
}

fn remember_settled(
    st: &mut InboxState,
    bucket: &str,
    command_id: &str,
    ack: CommandAck,
    run_token: Option<u64>,
) {
    let list = st.settled.entry(bucket.to_owned()).or_default();
    list.retain(|(id, _)| id != command_id);
    list.push((command_id.to_owned(), SettledEntry { ack, run_token }));
    while list.len() > IDEMPOTENCY_TABLE_PER_SESSION {
        // TS `bucket.keys().next()`：淘汰最旧（最早插入位）。
        if list.is_empty() {
            break;
        }
        list.remove(0);
    }
}

/// TS `lookupExact` 去掉 in-flight 分支后的部分（in-flight 由调用方先等待终态）：
/// live → settled(touch) → host 四路回源。
fn lookup_exact<H: CommandInboxHost>(
    st: &mut InboxState,
    host: &H,
    session_id: Option<&str>,
    command_id: &str,
) -> Option<CommandAck> {
    let bucket = bucket_key(session_id);
    if let Some(live) = st.live_inputs.get(&bucket).and_then(|m| m.get(command_id)) {
        return Some(live.ack.clone());
    }
    if let Some(ack) = settled_take(st, &bucket, command_id) {
        return Some(ack);
    }
    let key = CommandKey::new(session_id, command_id.to_owned());
    host.lookup_transcript_command(&key)
        .or_else(|| host.lookup_timeline_command(&key))
        .or_else(|| host.lookup_child_command(&key))
        .or_else(|| host.lookup_discarded_command(&key))
}

/// TS `queryUnavailableAck`：兜底 failed ACK（在途事实被 clearSession 清除等极端竞态）。
fn query_unavailable_ack<H: CommandInboxHost>(host: &H, key: &CommandKey) -> CommandAck {
    let revision = match &key.session_id {
        None => 0,
        Some(sid) => host.get_revision(sid).unwrap_or(0),
    };
    CommandAck {
        command_id: key.command_id.clone(),
        status: CommandStatus::Failed,
        reason_code: Some("fault.command.queryUnavailable".to_owned()),
        message: None,
        revision_at_decision: revision,
        result: None,
        memory_enabled: None,
    }
}

// ── 对外类型 ──

/// settle 凭证：绑定 bucket + commandId + run token；token 不匹配 → stale 拒绝。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettleTicket {
    pub bucket: String,
    pub command_id: String,
    pub run_token: u64,
}

#[derive(Debug)]
pub enum SettleOutcome {
    /// 已收口，携带合并后的终态 ACK。
    Settled(CommandAck),
    /// 同 token 重复 settle：TS closure 的 settled 幂等 noop。
    AlreadySettled,
}

/// stale-run 防护：非当前（或 token 不匹配）的 settle 被拒绝，绝不误杀当前运行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettleError {
    NotCurrent { command_id: String },
}

impl std::fmt::Display for SettleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettleError::NotCurrent { command_id } => {
                write!(
                    f,
                    "stale settle: command {command_id} is not the current run"
                )
            }
        }
    }
}

impl std::error::Error for SettleError {}

/// `handle` 结果：ack-only 收口，或 admitted 执行凭证。
#[derive(Debug)]
pub enum Outcome {
    Ack(CommandAck),
    Execute(Box<Admitted>),
}

/// admitted：TS `CommandInboxOutcome` 的 execute 分支。
#[derive(Debug)]
pub struct Admitted {
    pub envelope: CommandEnvelope,
    pub ack: CommandAck,
    /// 本桶单调序号（首条 = 1，见 TS `(get ?? 0) + 1`）。
    pub admission_seq: u64,
    pub admitted_at: i64,
    pub queue_item_id: String,
    pub ticket: SettleTicket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// 已出队：该 pending 的 `handle` 将返回 admissionCancelled ACK。
    Dequeued,
    /// 不在 pending 队列（在途命令请走 settle/cancel 收口；未知命令同样返回此值）。
    NotPending,
}

#[derive(Debug, Clone, PartialEq)]
pub enum QueryOutcome {
    Ack(CommandAck),
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueryItem {
    pub key: CommandKey,
    pub outcome: QueryOutcome,
}

/// 诊断快照：等待在途终态的调用数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaiterStats {
    pub final_waiters: usize,
}

enum Decision {
    Execute { ack: CommandAck },
    AckOnly { ack: CommandAck, remember: bool },
}

// ── CommandInbox ──

pub struct CommandInbox<H: CommandInboxHost> {
    host: H,
    state: Mutex<InboxState>,
    gate: Condvar,
}

impl<H: CommandInboxHost> CommandInbox<H> {
    pub fn new(host: H) -> Self {
        Self {
            host,
            state: Mutex::new(InboxState::new()),
            gate: Condvar::new(),
        }
    }

    pub fn host(&self) -> &H {
        &self.host
    }

    fn lock(&self) -> MutexGuard<'_, InboxState> {
        // 单个测试线程 panic 不毒化后续收口路径：直接取回内部状态继续。
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn notify(&self) {
        self.gate.notify_all();
    }

    // ── admission ──

    /// TS `handle(raw)`：解析 → key-gate 语义（duplicate/事实收口）→ session FIFO gate
    /// → decide → admissionSeq。排队等待期间整体阻塞（TS 为 await 门）。
    pub fn handle(&self, raw: &Value) -> Outcome {
        match parse_command_envelope(raw) {
            Ok(envelope) => self.handle_envelope(envelope),
            Err(error) => {
                let mut ack = CommandAck::new(extract_command_id(raw), CommandStatus::Rejected, 0);
                ack.reason_code = Some(REASON_INVALID_PAYLOAD.to_owned());
                ack.message = Some(error.message);
                Outcome::Ack(ack)
            }
        }
    }

    pub fn handle_envelope(&self, envelope: CommandEnvelope) -> Outcome {
        let bucket = bucket_key(envelope.session_id.as_deref());
        let command_id = envelope.command_id.clone();
        let mut st = self.lock();
        st.next_waiter_id += 1;
        let my_waiter = st.next_waiter_id;

        loop {
            // 1) 在途 duplicate → 等同一终态（TS key gate + `await pinned.final`）。
            if has_in_flight(&st, &bucket, &command_id) {
                // 等到终态即返回：锁随 `_guard` 在 return 时释放，无需回写 `st`。
                let (_guard, ack) = self.wait_in_flight_final(
                    st,
                    &bucket,
                    envelope.session_id.as_deref(),
                    &command_id,
                );
                let ack = ack.unwrap_or_else(|| {
                    query_unavailable_ack(
                        &self.host,
                        &CommandKey::new(envelope.session_id.as_deref(), command_id.clone()),
                    )
                });
                return Outcome::Ack(retry_ack(&ack));
            }

            // 2) 已有持久事实 → duplicate 收口。
            if let Some(ack) = lookup_exact(
                &mut st,
                &self.host,
                envelope.session_id.as_deref(),
                &command_id,
            ) {
                return Outcome::Ack(retry_ack(&ack));
            }

            // 3) session FIFO gate：在途或已有排队者时入队（严格 FIFO，不许插队）。
            let busy = bucket_busy(&st, &bucket);
            let queued = st.pending.get(&bucket).map_or(0, VecDeque::len);
            if busy || queued > 0 {
                let mine = st
                    .pending
                    .get(&bucket)
                    .and_then(|q| q.iter().find(|e| e.waiter_id == my_waiter))
                    .map(|e| e.cancelled);
                match mine {
                    Some(cancelled) => {
                        if cancelled {
                            pop_my_entry(&mut st, &bucket, my_waiter);
                            self.notify();
                            let ack = cancelled_ack(&self.host, &envelope);
                            return Outcome::Ack(ack);
                        }
                        let is_front = st
                            .pending
                            .get(&bucket)
                            .and_then(VecDeque::front)
                            .map(|e| e.waiter_id == my_waiter)
                            .unwrap_or(false);
                        if is_front && !busy {
                            break;
                        }
                        st = self.gate.wait(st).unwrap_or_else(|e| e.into_inner());
                        continue;
                    }
                    None => {
                        let other_same_id = st
                            .pending
                            .get(&bucket)
                            .is_some_and(|q| q.iter().any(|e| e.command_id == command_id));
                        if other_same_id {
                            // 同 commandId 的先行 handle 仍在排队：key-gate 语义——
                            // 只等它离开队列，不重复入队；随后顶部复查决定 duplicate 或重新裁决。
                            st = self.gate.wait(st).unwrap_or_else(|e| e.into_inner());
                            continue;
                        }
                        st.pending
                            .entry(bucket.clone())
                            .or_default()
                            .push_back(PendingEntry {
                                waiter_id: my_waiter,
                                command_id: command_id.clone(),
                                cancelled: false,
                            });
                        st = self.gate.wait(st).unwrap_or_else(|e| e.into_inner());
                        continue;
                    }
                }
            }
            // gate 空闲且队列为空 → 快速路径（对应 TS 未入队的立即 acquire）。
            break;
        }

        // 4) decide 必须发生在拿到 session gate 之后（与 TS 锁序一致）。
        let decision = decide(&self.host, &envelope);
        pop_my_entry(&mut st, &bucket, my_waiter);
        match decision {
            Decision::AckOnly { ack, remember } => {
                if remember {
                    remember_settled(&mut st, &bucket, &envelope.command_id, ack.clone(), None);
                }
                self.notify();
                Outcome::Ack(ack)
            }
            Decision::Execute { ack } => {
                let seq = st.admission_seq.get(&bucket).copied().unwrap_or(0) + 1;
                st.admission_seq.insert(bucket.clone(), seq);
                let admitted_at = self.host.now_ms();
                st.next_run_token += 1;
                let run_token = st.next_run_token;
                st.in_flight.entry(bucket.clone()).or_default().insert(
                    envelope.command_id.clone(),
                    InFlightEntry {
                        ack: ack.clone(),
                        run_token,
                    },
                );
                let ticket = SettleTicket {
                    bucket,
                    command_id: envelope.command_id.clone(),
                    run_token,
                };
                let queue_item_id = queue_item_id_for_command(&envelope.command_id);
                self.notify();
                Outcome::Execute(Box::new(Admitted {
                    envelope,
                    ack,
                    admission_seq: seq,
                    admitted_at,
                    queue_item_id,
                    ticket,
                }))
            }
        }
    }

    /// 等在途终态（TS `await inflight.final`）；谓词循环防虚假唤醒。
    fn wait_in_flight_final<'a>(
        &'a self,
        mut st: MutexGuard<'a, InboxState>,
        bucket: &str,
        session_id: Option<&str>,
        command_id: &str,
    ) -> (MutexGuard<'a, InboxState>, Option<CommandAck>) {
        st.final_waiters += 1;
        while has_in_flight(&st, bucket, command_id) {
            st = self.gate.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        st.final_waiters -= 1;
        let ack = lookup_exact(&mut st, &self.host, session_id, command_id);
        (st, ack)
    }

    /// settle：回填终态、移出在途、（live pin 优先）写入 settled 幂等表并释放 gate。
    /// 与 TS `settle(final)` 一致：必须恰好调用一次；同 token 再调为 noop。
    pub fn settle(
        &self,
        ticket: SettleTicket,
        final_: CommandFinal,
    ) -> Result<SettleOutcome, SettleError> {
        let mut st = self.lock();
        match in_flight_of(&st, &ticket.bucket, &ticket.command_id) {
            Some(entry) if entry.run_token == ticket.run_token => {
                let base = entry.ack.clone();
                let ack = merge_final(&base, &final_);
                st.in_flight
                    .get_mut(&ticket.bucket)
                    .map(|m| m.remove(&ticket.command_id));
                let live = st
                    .live_inputs
                    .get_mut(&ticket.bucket)
                    .and_then(|m| m.get_mut(&ticket.command_id));
                if let Some(live) = live {
                    // live pin 优先：终态合并进 pinned ack，不进 settled LRU（TS 同序）。
                    live.ack = ack.clone();
                    live.run_token = Some(ticket.run_token);
                } else {
                    remember_settled(
                        &mut st,
                        &ticket.bucket,
                        &ticket.command_id,
                        ack.clone(),
                        Some(ticket.run_token),
                    );
                }
                self.notify();
                Ok(SettleOutcome::Settled(ack))
            }
            Some(_) => Err(SettleError::NotCurrent {
                command_id: ticket.command_id,
            }),
            None => {
                let tomb = settled_token(&st, &ticket.bucket, &ticket.command_id).or_else(|| {
                    st.live_inputs
                        .get(&ticket.bucket)
                        .and_then(|m| m.get(&ticket.command_id))
                        .map(|e| e.run_token)
                });
                if tomb == Some(Some(ticket.run_token)) {
                    Ok(SettleOutcome::AlreadySettled)
                } else {
                    Err(SettleError::NotCurrent {
                        command_id: ticket.command_id,
                    })
                }
            }
        }
    }

    /// 取消排队中的 admission：出队后其 `handle` 返回 `admissionCancelled` ACK，
    /// 不消耗 admissionSeq。在途命令请走 `settle`（per source：cancel = settle 终态）。
    pub fn cancel_pending(&self, session_id: Option<&str>, command_id: &str) -> CancelOutcome {
        let bucket = bucket_key(session_id);
        let mut st = self.lock();
        let mut found = false;
        if let Some(queue) = st.pending.get_mut(&bucket) {
            if let Some(entry) = queue
                .iter_mut()
                .find(|e| e.command_id == command_id && !e.cancelled)
            {
                entry.cancelled = true;
                found = true;
            }
        }
        if found {
            self.notify();
            CancelOutcome::Dequeued
        } else {
            CancelOutcome::NotPending
        }
    }

    // ── 查询 ──

    /// TS `query(keys)`：在途 key 等待终态返回原 ACK（不套 duplicate）；
    /// 未命中 → `unknown`。逐 key 串行与 `Promise.all` 的总完成时间一致
    /// （每个在途 key 都要等到各自 settle）。
    pub fn query(&self, keys: &[CommandKey]) -> Vec<QueryItem> {
        let mut st = self.lock();
        let mut items = Vec::with_capacity(keys.len());
        for key in keys {
            let bucket = bucket_key(key.session_id.as_deref());
            if has_in_flight(&st, &bucket, &key.command_id) {
                let (guard, ack) = self.wait_in_flight_final(
                    st,
                    &bucket,
                    key.session_id.as_deref(),
                    &key.command_id,
                );
                st = guard;
                let ack = ack.unwrap_or_else(|| query_unavailable_ack(&self.host, key));
                items.push(QueryItem {
                    key: key.clone(),
                    outcome: QueryOutcome::Ack(ack),
                });
                continue;
            }
            let outcome = match lookup_exact(
                &mut st,
                &self.host,
                key.session_id.as_deref(),
                &key.command_id,
            ) {
                Some(ack) => QueryOutcome::Ack(ack),
                None => QueryOutcome::Unknown,
            };
            items.push(QueryItem {
                key: key.clone(),
                outcome,
            });
        }
        items
    }

    // ── live input pin（queue/guide admission 后由 gateway 调用） ──

    /// TS `pinLiveInput`：pin 同一完整事实；settled churn 不得触及它。
    /// `session_id`/`source_command_id` 对应 intent.sourceCommandId。
    pub fn pin_live_input(
        &self,
        session_id: Option<&str>,
        source_command_id: &str,
        ack: Option<CommandAck>,
    ) {
        let bucket = bucket_key(session_id);
        let mut st = self.lock();
        let (fallback, run_token) = match in_flight_of(&st, &bucket, source_command_id) {
            Some(entry) => (Some(entry.ack.clone()), Some(entry.run_token)),
            None => (None, None),
        };
        let entry_ack = ack.unwrap_or_else(|| {
            fallback
                .unwrap_or_else(|| CommandAck::new(source_command_id, CommandStatus::Accepted, 0))
        });
        st.live_inputs.entry(bucket.clone()).or_default().insert(
            source_command_id.to_owned(),
            LiveInputEntry {
                ack: entry_ack,
                run_token,
            },
        );
        if let Some(list) = st.settled.get_mut(&bucket) {
            list.retain(|(id, _)| id != source_command_id);
        }
    }

    /// TS `releaseLiveInput`：解除 pin，可把终态转入 settled LRU（保留 run token 墓碑）。
    pub fn release_live_input(&self, key: &CommandKey, final_ack: Option<CommandAck>) {
        let bucket = bucket_key(key.session_id.as_deref());
        let mut st = self.lock();
        let live = st
            .live_inputs
            .get_mut(&bucket)
            .and_then(|m| m.remove(&key.command_id));
        let run_token = live.as_ref().and_then(|l| l.run_token);
        let ack = final_ack.or_else(|| live.as_ref().map(|l| l.ack.clone()));
        if let Some(ack) = ack {
            remember_settled(&mut st, &bucket, &key.command_id, ack, run_token);
        }
    }

    // ── 会话生命周期 / 诊断 ──

    /// TS `hasPinnedSessionState`：in-flight 或 live pin 任一存在。
    pub fn has_pinned_session_state(&self, session_id: Option<&str>) -> bool {
        let bucket = bucket_key(session_id);
        let st = self.lock();
        bucket_busy(&st, &bucket) || st.live_inputs.get(&bucket).is_some_and(|m| !m.is_empty())
    }

    /// TS `clearSession`：有 pin 时拒绝；否则清空该桶全部事实与 admissionSeq。
    pub fn clear_session(&self, session_id: Option<&str>) -> bool {
        let bucket = bucket_key(session_id);
        let mut st = self.lock();
        if bucket_busy(&st, &bucket) || st.live_inputs.get(&bucket).is_some_and(|m| !m.is_empty()) {
            return false;
        }
        st.in_flight.remove(&bucket);
        st.live_inputs.remove(&bucket);
        st.settled.remove(&bucket);
        st.admission_seq.remove(&bucket);
        self.notify();
        true
    }

    /// 当前桶 admissionSeq（诊断/断言用；不产生新状态）。
    pub fn admission_seq_for(&self, session_id: Option<&str>) -> u64 {
        let bucket = bucket_key(session_id);
        let st = self.lock();
        st.admission_seq.get(&bucket).copied().unwrap_or(0)
    }

    /// 排队中的 pending commandId（FIFO 序；TS 不可观测的 Rust 侧诊断）。
    pub fn pending_command_ids(&self, session_id: Option<&str>) -> Vec<String> {
        let bucket = bucket_key(session_id);
        let st = self.lock();
        st.pending
            .get(&bucket)
            .map(|q| q.iter().map(|e| e.command_id.clone()).collect())
            .unwrap_or_default()
    }

    pub fn waiter_stats(&self) -> WaiterStats {
        let st = self.lock();
        WaiterStats {
            final_waiters: st.final_waiters,
        }
    }
}

fn pop_my_entry(st: &mut InboxState, bucket: &str, waiter_id: u64) {
    if let Some(queue) = st.pending.get_mut(bucket) {
        if let Some(pos) = queue.iter().position(|e| e.waiter_id == waiter_id) {
            queue.remove(pos);
        }
    }
}

fn cancelled_ack<H: CommandInboxHost>(host: &H, envelope: &CommandEnvelope) -> CommandAck {
    let revision = envelope
        .session_id
        .as_deref()
        .and_then(|sid| host.get_revision(sid))
        .unwrap_or(0);
    let mut ack = CommandAck::new(
        envelope.command_id.as_str(),
        CommandStatus::Rejected,
        revision,
    );
    ack.reason_code = Some(REASON_ADMISSION_CANCELLED.to_owned());
    ack
}

// ── decide（镜 TS CommandInbox.decide，锁序：key → session gate 之后执行） ──

fn decide<H: CommandInboxHost>(host: &H, envelope: &CommandEnvelope) -> Decision {
    let is_create_session = envelope.command_type == CREATE_SESSION_TYPE;
    let revision = match &envelope.session_id {
        None => Some(0),
        Some(sid) => host.get_revision(sid),
    };
    if revision.is_none() || (!is_create_session && envelope.session_id.is_none()) {
        return ack_only(
            envelope,
            CommandStatus::Rejected,
            Some(REASON_SESSION_NOT_FOUND),
            0,
            None,
            false,
        );
    }
    let revision = revision.unwrap_or(0);

    if requires_base_revision(&envelope.command_type) {
        let Some(base_revision) = envelope.base_revision else {
            return ack_only(
                envelope,
                CommandStatus::Rejected,
                Some(REASON_MISSING_BASE_REVISION),
                revision,
                None,
                false,
            );
        };
        let log_epoch = envelope
            .session_id
            .as_deref()
            .and_then(|sid| host.get_log_epoch(sid));
        if crate::protocol::is_row_targeting(&envelope.command_type)
            && envelope.base_log_epoch.as_deref() != log_epoch.as_deref()
        {
            return ack_only(
                envelope,
                CommandStatus::Stale,
                Some(REASON_STALE_LOG_EPOCH),
                revision,
                None,
                false,
            );
        }
        if base_revision != revision {
            return ack_only(
                envelope,
                CommandStatus::Stale,
                Some(REASON_STALE_REVISION),
                revision,
                None,
                false,
            );
        }
    }

    match host.validate_row_target(envelope) {
        GuardDecision::Stale {
            reason_code,
            message,
        } => {
            return ack_only(
                envelope,
                CommandStatus::Stale,
                Some(&reason_code),
                revision,
                message.as_deref(),
                false,
            )
        }
        GuardDecision::Reject {
            reason_code,
            message,
        } => {
            return ack_only(
                envelope,
                CommandStatus::Rejected,
                Some(&reason_code),
                revision,
                message.as_deref(),
                false,
            )
        }
        GuardDecision::Noop {
            reason_code,
            result,
        } => {
            let mut ack =
                CommandAck::new(envelope.command_id.as_str(), CommandStatus::Noop, revision);
            ack.reason_code = Some(reason_code);
            ack.result = result;
            return Decision::AckOnly {
                ack,
                remember: true,
            };
        }
        GuardDecision::Allow => {}
    }

    match host.guard(envelope) {
        GuardDecision::Stale {
            reason_code,
            message,
        } => ack_only(
            envelope,
            CommandStatus::Stale,
            Some(&reason_code),
            revision,
            message.as_deref(),
            false,
        ),
        GuardDecision::Reject {
            reason_code,
            message,
        } => ack_only(
            envelope,
            CommandStatus::Rejected,
            Some(&reason_code),
            revision,
            message.as_deref(),
            false,
        ),
        GuardDecision::Noop {
            reason_code,
            result,
        } => {
            let mut ack =
                CommandAck::new(envelope.command_id.as_str(), CommandStatus::Noop, revision);
            ack.reason_code = Some(reason_code);
            ack.result = result;
            Decision::AckOnly {
                ack,
                remember: true,
            }
        }
        GuardDecision::Allow => Decision::Execute {
            ack: CommandAck::new(
                envelope.command_id.as_str(),
                CommandStatus::Accepted,
                revision,
            ),
        },
    }
}

fn ack_only(
    envelope: &CommandEnvelope,
    status: CommandStatus,
    reason_code: Option<&str>,
    revision: i64,
    message: Option<&str>,
    remember: bool,
) -> Decision {
    let mut ack = CommandAck::new(envelope.command_id.as_str(), status, revision);
    ack.reason_code = reason_code.map(str::to_owned);
    ack.message = message.map(str::to_owned);
    Decision::AckOnly { ack, remember }
}

// ── 共享句柄 ──

/// TS 调用点共享形态：一个 `CommandInbox` 被并发 `handleCommand`/`queryCommands`
/// 复用，settle 由执行完成方回调 → `Arc` 共享（内部互斥即 `Arc<Mutex<...>>` 包装）。
pub struct SharedCommandInbox<H: CommandInboxHost>(Arc<CommandInbox<H>>);

// 手写 Clone：derive 会给泛型参数自动加 `H: Clone` 约束，而这里只克隆 `Arc`，
// host 无需可克隆（TS 调用点同样共享同一 host 实例）。
impl<H: CommandInboxHost> Clone for SharedCommandInbox<H> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<H: CommandInboxHost> SharedCommandInbox<H> {
    pub fn new(inbox: CommandInbox<H>) -> Self {
        Self(Arc::new(inbox))
    }
}

impl<H: CommandInboxHost> Deref for SharedCommandInbox<H> {
    type Target = CommandInbox<H>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
