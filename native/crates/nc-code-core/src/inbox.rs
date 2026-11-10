//! CommandInbox: mirror of `apps/zcode-cli/packages/bootstrap/src/zcode-protocol-v4/command-inbox.ts`.
//!
//! Serial admission semantics for v4 commands: one in-flight command per session bucket,
//! a FIFO queue behind it, and idempotent replay keyed by the command id.
//!
//! Ownership (unique state owner): [`CommandInbox`] internal `Mutex<InboxState>` + `Condvar`.
//! The TS call point is a single gateway instance that is reused by concurrent `handleCommand` (`v4-gateway.ts` multi-tasking
//! At the same time, await `inbox.handle`, settle is called back by the execution completion party), that is, shared ownership - so this implementation
//! All methods use `&self` (mutually exclusive internally), and cross-task sharing uses [`SharedCommandInbox`] (`Arc`).
//! See the README for trade-offs with the `&mut self` single-owner solution.
//!
//! Event surface (TS has no emitter, host subscription surface is these):
//! - `handle` → [`Outcome::Ack`] (reject/repeat/noop, close immediately) or [`Outcome::Execute`]
//! (admitted: carries admissionSeq / admittedAt / queueItemId / settle credentials);
//! - `settle` closes the in-flight facts and releases the session gate → wakes up the FIFO queue head and duplicate/query waiters;
//! - `query` waits for the final state of the key in transit (TS `await inflight.final`);
//! - The pending in the queue is not observable in TS, and is additionally exposed here [`CommandInbox::pending_command_ids`]
//! with [`CommandInbox::waiter_stats`] as Rust-side diagnostics (does not change protocol behavior).

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

// ── Internal state ──

#[derive(Debug, Clone)]
struct InFlightEntry {
    /// Admission decision ACK (accepted); settle uses this as the basis for final state merging.
    ack: CommandAck,
    /// run token: settle credential; forged/expired tokens are rejected by stale-run protection.
    run_token: u64,
}

#[derive(Debug, Clone)]
struct SettledEntry {
    ack: CommandAck,
    /// settle idempotent tombstone: the same token repeats settle → noop (the settled flag of TS closure).
    run_token: Option<u64>,
}

#[derive(Debug, Clone)]
struct LiveInputEntry {
    ack: CommandAck,
    run_token: Option<u64>,
}

/// Session FIFO queued entries (TS's AsyncGateRegistry promise chain is made explicit here).
#[derive(Debug, Clone)]
struct PendingEntry {
    /// Unique id for each `handle` call: concurrent submitters with the same commandId are not enqueued (key-gate semantics).
    waiter_id: u64,
    command_id: String,
    cancelled: bool,
}

#[derive(Debug)]
struct InboxState {
    in_flight: HashMap<String, HashMap<String, InFlightEntry>>,
    live_inputs: HashMap<String, HashMap<String, LiveInputEntry>>,
    /// Order-preserving idempotent table per bucket (TS Map insertion order = LRU elimination order); upper limit 512.
    settled: HashMap<String, Vec<(String, SettledEntry)>>,
    admission_seq: HashMap<String, u64>,
    pending: HashMap<String, VecDeque<PendingEntry>>,
    /// Number of handle/query calls waiting for final state (diagnostics).
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

/// TS `retryAck`: failed is the final state, keep the original state, and the rest will be rewritten as duplicate.
fn retry_ack(ack: &CommandAck) -> CommandAck {
    if ack.status == CommandStatus::Failed {
        ack.clone()
    } else {
        let mut next = ack.clone();
        next.status = CommandStatus::Duplicate;
        next
    }
}

/// Settled order-preserving table: Hit is touch (delete the old and insert the new), keeping the LRU elimination order consistent with the TS Map.
fn settled_take(st: &mut InboxState, bucket: &str, command_id: &str) -> Option<CommandAck> {
    let list = st.settled.get_mut(bucket)?;
    let pos = list.iter().position(|(id, _)| id == command_id)?;
    // touch: Delete the old and insert the new to maintain LRU order; the entire entry is moved back, and only the ack used for cloning is returned.
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
        // TS `bucket.keys().next()`: Eliminate the oldest (earliest inserted bit).
        if list.is_empty() {
            break;
        }
        list.remove(0);
    }
}

/// TS `lookupExact` removes the part after the in-flight branch (in-flight requires the caller to wait for the final state first):
/// live → settled(touch) → host four-way return to source.
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

/// TS `queryUnavailableAck`: Failed ACK (extreme race conditions such as being cleared by clearSession in transit).
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

// ── External type ──

/// settle credentials: bind bucket + commandId + run token; token does not match → stale reject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettleTicket {
    pub bucket: String,
    pub command_id: String,
    pub run_token: u64,
}

#[derive(Debug)]
pub enum SettleOutcome {
    /// The port has been closed and carries the merged final state ACK.
    Settled(CommandAck),
    /// Repeat settle with the same token: settled idempotent noop of TS closure.
    AlreadySettled,
}

/// stale-run protection: Non-current (or token mismatched) settles are rejected and the current run will never be accidentally killed.
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

/// `handle` result: ack-only closing, or admitted execution certificate.
#[derive(Debug)]
pub enum Outcome {
    Ack(CommandAck),
    Execute(Box<Admitted>),
}

/// admitted: execute branch of TS `CommandInboxOutcome`.
#[derive(Debug)]
pub struct Admitted {
    pub envelope: CommandEnvelope,
    pub ack: CommandAck,
    /// The monotonic sequence number of this bucket (first entry = 1, see TS `(get ?? 0) + 1`).
    pub admission_seq: u64,
    pub admitted_at: i64,
    pub queue_item_id: String,
    pub ticket: SettleTicket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// Dequeued: The pending `handle` will return admissionCancelled ACK.
    Dequeued,
    /// Not in the pending queue (please use settle/cancel to close the command in transit; unknown commands also return this value).
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

/// Diagnostic snapshot: Number of calls waiting for final state.
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
        // A single test thread panic does not poison the subsequent closing path: directly retrieve the internal state and continue.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn notify(&self) {
        self.gate.notify_all();
    }

    // ── admission ──

    /// TS `handle(raw)`: parsing → key-gate semantics (duplicate/fact closing) → session FIFO gate
    /// → decide → admissionSeq. The entire queue is blocked (TS is the await gate).
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
            // 1) Duplicate in transit → equal to the same final state (TS key gate + `await pinned.final`).
            if has_in_flight(&st, &bucket, &command_id) {
                // Wait until the final state and return: the lock is released with `_guard` on return, no need to write back `st`.
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

            // 2) There is already a persistent fact → duplicate closure.
            if let Some(ack) = lookup_exact(
                &mut st,
                &self.host,
                envelope.session_id.as_deref(),
                &command_id,
            ) {
                return Outcome::Ack(retry_ack(&ack));
            }

            // 3) session FIFO gate: join the queue when in transit or when there are already people queuing (strictly FIFO, no queue jumping).
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
                            // The previous handle with the same commandId is still queued: key-gate semantics——
                            // Just wait for it to leave the queue without re-entering the queue; then the top review determines duplicate or re-adjusting.
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
            // The gate is idle and the queue is empty → fast path (corresponding to immediate acquire when TS is not enqueued).
            break;
        }

        // 4) decide must occur after getting the session gate (consistent with TS lock order).
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

    /// Wait in flight final state (TS `await inflight.final`); predicate loop prevents false wake-up.
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

    /// settle: backfill the final state, move out in transit, (live pin priority) write to the settled idempotent table and release the gate.
    /// Consistent with TS `settle(final)`: must be called exactly once; with the same token, adjust it to noop.
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
                    // Live pin priority: the final state is merged into pinned ack, not settled LRU (TS is in the same order).
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

    /// Cancel the admission in the queue: after dequeuing, its `handle` returns `admissionCancelled` ACK,
    /// Does not consume admissionSeq. In-flight command please go `settle` (per source: cancel = settle final state).
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

    // ── Query ──

    /// TS `query(keys)`: The key in transit waits for the final state to return to the original ACK (no duplicate);
    /// Miss → `unknown`. The key-by-key serialization is consistent with the total completion time of `Promise.all`
    /// (Each in-transit key must wait until its respective settlement).
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

    // ── live input pin (called by gateway after queue/guide admission) ──

    /// TS `pinLiveInput`: pin is the same complete fact; settled churn must not touch it.
    /// `session_id`/`source_command_id` corresponds to intent.sourceCommandId.
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

    /// TS `releaseLiveInput`: Release the pin and transfer the final state to settled LRU (retain the run token tombstone).
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

    // ── Session Lifecycle/Diagnostics ──

    /// TS `hasPinnedSessionState`: Either in-flight or live pin exists.
    pub fn has_pinned_session_state(&self, session_id: Option<&str>) -> bool {
        let bucket = bucket_key(session_id);
        let st = self.lock();
        bucket_busy(&st, &bucket) || st.live_inputs.get(&bucket).is_some_and(|m| !m.is_empty())
    }

    /// TS `clearSession`: reject when there is a pin; otherwise clear all facts and admissionSeq in the bucket.
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

    /// Current bucket admissionSeq (used for diagnosis/assertion; no new status is generated).
    pub fn admission_seq_for(&self, session_id: Option<&str>) -> u64 {
        let bucket = bucket_key(session_id);
        let st = self.lock();
        st.admission_seq.get(&bucket).copied().unwrap_or(0)
    }

    /// The pending commandId in the queue (FIFO order; TS unobservable Rust-side diagnostics).
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

// ── decide (mirror TS CommandInbox.decide, lock sequence: key → session gate, execute after) ──

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

// ── Shared handle ──

/// TS call point sharing form: a `CommandInbox` is concurrently `handleCommand`/`queryCommands`
/// Multiplexing, settle is called back by the execution completion party → `Arc` is shared (internal mutual exclusion is `Arc<Mutex<...>>` packaging).
pub struct SharedCommandInbox<H: CommandInboxHost>(Arc<CommandInbox<H>>);

//Handwritten Clone:derive will automatically add `H: Clone` constraints to generic parameters, but here only clones `Arc`,
// The host does not need to be cloneable (TS call points also share the same host instance).
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

#[cfg(test)]
mod tests {
    /// Queue ids are derived from the command id: the UI keys queued rows by it, so a
    /// change of shape silently breaks reorder/delete targeting.
    #[test]
    fn queue_item_id_derives_from_the_command_id() {
        assert_eq!(super::queue_item_id_for_command("cmd-1"), "queue_cmd-1");
        assert_eq!(super::queue_item_id_for_command(""), "queue_");
    }
}
