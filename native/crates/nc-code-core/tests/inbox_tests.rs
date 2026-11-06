//! CommandInbox behavior test.
//!
//! There is no corresponding TS test file for `command-inbox.ts` in the warehouse (grep `*.test.ts`/`*.spec.ts`
//! verified as 0), so there is no portable TS assertion; this example is written one by one according to the TS source code behavior:
//! `command-inbox.ts` (admission/FIFO/idempotent/retryAck/query), `command.ts`
//! (CAS/ROW collection and parsing error). Full determinism: threads for concurrency + bounded spin (no wall clock sleep),
//! Time to go `now_ms` injected by host.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

use nc_code_core::protocol::{
    CommandAck, CommandEnvelope, CommandFinal, CommandInboxHost, CommandKey, CommandStatus,
    GuardDecision, IDEMPOTENCY_TABLE_PER_SESSION, PARSE_CAS_MESSAGE, REASON_INVALID_PAYLOAD,
    REASON_SESSION_NOT_FOUND, REASON_STALE_LOG_EPOCH, REASON_STALE_REVISION,
};
use nc_code_core::{
    queue_item_id_for_command, Admitted, CancelOutcome, CommandInbox, Outcome, QueryOutcome,
    SettleError, SettleOutcome, SharedCommandInbox, REASON_ADMISSION_CANCELLED,
};

/// Bounded spin upper limit: only yields the CPU, without any wall clock wait.
const SPIN_LIMIT: usize = 5_000_000;

/// Wait for the condition to be established (bounded yield, no sleep); it will fail when it times out, exposing implementation flaws.
fn spin_until(label: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..SPIN_LIMIT {
        if cond() {
            return;
        }
        thread::yield_now();
    }
    panic!("spin_until timeout not met: {label}");
}

// ── Test host (fixed now_ms, callback decoupling) ──

type GuardFn = Arc<dyn Fn(&CommandEnvelope) -> GuardDecision + Send + Sync>;

#[derive(Default)]
struct TestHost {
    revisions: Mutex<HashMap<String, i64>>,
    epochs: Mutex<HashMap<String, String>>,
    guard: Mutex<Option<GuardFn>>,
    row_guard: Mutex<Option<GuardFn>>,
    transcript: Mutex<HashMap<String, CommandAck>>,
    now_ms: AtomicI64,
}

impl TestHost {
    fn set_session(&self, session_id: &str, revision: i64) {
        self.revisions
            .lock()
            .unwrap()
            .insert(session_id.to_owned(), revision);
    }

    fn set_epoch(&self, session_id: &str, epoch: &str) {
        self.epochs
            .lock()
            .unwrap()
            .insert(session_id.to_owned(), epoch.to_owned());
    }

    fn set_now(&self, ms: i64) {
        self.now_ms.store(ms, Ordering::SeqCst);
    }

    fn set_guard<F>(&self, f: F)
    where
        F: Fn(&CommandEnvelope) -> GuardDecision + Send + Sync + 'static,
    {
        *self.guard.lock().unwrap() = Some(Arc::new(f));
    }

    fn set_row_guard<F>(&self, f: F)
    where
        F: Fn(&CommandEnvelope) -> GuardDecision + Send + Sync + 'static,
    {
        *self.row_guard.lock().unwrap() = Some(Arc::new(f));
    }

    fn session(session_id: &str, revision: i64) -> Self {
        let host = Self::default();
        host.set_session(session_id, revision);
        host
    }
}

impl CommandInboxHost for TestHost {
    fn get_revision(&self, session_id: &str) -> Option<i64> {
        self.revisions.lock().unwrap().get(session_id).copied()
    }

    fn get_log_epoch(&self, session_id: &str) -> Option<String> {
        self.epochs.lock().unwrap().get(session_id).cloned()
    }

    fn validate_row_target(&self, envelope: &CommandEnvelope) -> GuardDecision {
        match &*self.row_guard.lock().unwrap() {
            Some(f) => f(envelope),
            None => GuardDecision::Allow,
        }
    }

    fn guard(&self, envelope: &CommandEnvelope) -> GuardDecision {
        match &*self.guard.lock().unwrap() {
            Some(f) => f(envelope),
            None => GuardDecision::Allow,
        }
    }

    fn lookup_transcript_command(&self, key: &CommandKey) -> Option<CommandAck> {
        self.transcript
            .lock()
            .unwrap()
            .get(&key.command_id)
            .cloned()
    }

    fn now_ms(&self) -> i64 {
        self.now_ms.load(Ordering::SeqCst)
    }
}

// ── Envelope construction (null field = TS default semantics) ──

fn env_of(
    command_type: &str,
    command_id: &str,
    session_id: Option<&str>,
    base_revision: Option<i64>,
    base_log_epoch: Option<&str>,
) -> Value {
    json!({
        "type": command_type,
        "commandId": command_id,
        "clientId": "client-1",
        "sessionId": session_id,
        "baseRevision": base_revision,
        "baseLogEpoch": base_log_epoch,
        "payload": {},
        "issuedAt": 1_700_000_000_000.0f64,
    })
}

/// Default non-CAS command (sendText is not in the `COMMANDS_REQUIRING_BASE_REVISION` set).
fn env(command_id: &str, session_id: Option<&str>) -> Value {
    env_of("sendText", command_id, session_id, None, None)
}

// ── Assertion helper ──

fn expect_ack(outcome: Outcome) -> CommandAck {
    match outcome {
        Outcome::Ack(ack) => ack,
        Outcome::Execute(a) => panic!("Expected ack-only, but admitted: {}", a.ack.command_id),
    }
}

fn expect_execute(outcome: Outcome) -> Box<Admitted> {
    match outcome {
        Outcome::Execute(a) => a,
        Outcome::Ack(ack) => panic!(
            "Expected admitted, but got ack: {} status={:?}",
            ack.command_id, ack.status
        ),
    }
}

fn assert_reason(ack: &CommandAck, expected: &str) {
    assert_eq!(
        ack.reason_code.as_deref(),
        Some(expected),
        "The reasonCode of command {} does not match: {:?}",
        ack.command_id,
        ack
    );
}

fn settle_ok(
    inbox: &CommandInbox<TestHost>,
    ticket: nc_code_core::SettleTicket,
    final_: CommandFinal,
) -> CommandAck {
    match inbox.settle(ticket, final_) {
        Ok(SettleOutcome::Settled(ack)) => ack,
        other => panic!("settle should be closed successfully and get: {other:?}"),
    }
}

// ── 1. Parsing failure: ACK + extractCommandId ──

#[test]
fn invalid_payload_rejected_and_extract_command_id() {
    let inbox = CommandInbox::new(TestHost::session("s1", 3));

    // Missing clientId → proto.invalidPayload (copywriter's own, reasonCode aligned with TS).
    let raw = json!({ "commandId": "c-bad", "sessionId": "s1", "type": "sendText" });
    let ack = expect_ack(inbox.handle(&raw));
    assert_eq!(ack.status, CommandStatus::Rejected);
    assert_reason(&ack, REASON_INVALID_PAYLOAD);
    assert!(ack.message.as_deref().unwrap_or("").contains("clientId"));
    assert_eq!(ack.revision_at_decision, 0);
    // Parse failure must not consume admissionSeq.
    assert_eq!(inbox.admission_seq_for(Some("s1")), 0);

    // TS `extractCommandId`: If parsing fails, try to retrieve the commandId (non-string → "").
    assert_eq!(nc_code_core::protocol::extract_command_id(&raw), "c-bad");
    assert_eq!(
        nc_code_core::protocol::extract_command_id(&json!({ "commandId": 7 })),
        ""
    );
    assert_eq!(nc_code_core::protocol::extract_command_id(&json!(42)), "");

    //Illegal commandType is also rejected at the parsing layer.
    let bad_type = env_of("notACommand", "c-type", Some("s1"), None, None);
    let ack = expect_ack(inbox.handle(&bad_type));
    assert_eq!(ack.status, CommandStatus::Rejected);
    assert_reason(&ack, REASON_INVALID_PAYLOAD);
}

// ── 2. Missing CAS/ROW fields are rejected at the parsing layer (mirroring the synthetic copy of command.ts) ──

#[test]
fn cas_commands_missing_fields_fail_at_parse() {
    let host = TestHost::session("s1", 5);
    host.set_epoch("s1", "e2");
    let inbox = CommandInbox::new(host);

    // CAS command missing baseRevision → parse rejected, copy = TS zod synthesis error.
    let missing_base = env_of("switchModelConfig", "c-cas", Some("s1"), None, None);
    let ack = expect_ack(inbox.handle(&missing_base));
    assert_eq!(ack.status, CommandStatus::Rejected);
    assert_reason(&ack, REASON_INVALID_PAYLOAD);
    assert_eq!(ack.message.as_deref(), Some(PARSE_CAS_MESSAGE));
    assert_eq!(inbox.admission_seq_for(Some("s1")), 0);

    // ROW command is missing baseLogEpoch → the same synthetic copy (parse level requirement).
    let missing_epoch = env_of("retryTurn", "c-row", Some("s1"), Some(5), None);
    let ack = expect_ack(inbox.handle(&missing_epoch));
    assert_eq!(ack.status, CommandStatus::Rejected);
    assert_reason(&ack, REASON_INVALID_PAYLOAD);
    assert_eq!(ack.message.as_deref(), Some(PARSE_CAS_MESSAGE));
    assert_eq!(inbox.admission_seq_for(Some("s1")), 0);

    // Fields are complete + consistent with host → normal admitted (proves that the above failure is indeed due to missing fields).
    let ok = env_of("retryTurn", "c-row-ok", Some("s1"), Some(5), Some("e2"));
    let admitted = expect_execute(inbox.handle(&ok));
    assert_eq!(admitted.ack.status, CommandStatus::Accepted);
    settle_ok(&inbox, admitted.ticket, CommandFinal::accepted());
}

// ── 3. Concurrent admission serial (max-active == 1 + admissionSeq 1..=N unique) ──

#[test]
fn concurrent_admissions_are_serial_with_unique_seqs() {
    const N: i64 = 6;
    let host = TestHost::session("s1", 7);
    host.set_now(1_700_000_123_000);
    let inbox = SharedCommandInbox::new(CommandInbox::new(host));

    let active = Arc::new(AtomicI64::new(0));
    let max_active = Arc::new(AtomicI64::new(0));
    let seqs = Arc::new(Mutex::new(Vec::new()));
    let mut workers = Vec::new();
    for i in 0..N {
        let inbox = inbox.clone();
        let active = Arc::clone(&active);
        let max_active = Arc::clone(&max_active);
        let seqs = Arc::clone(&seqs);
        workers.push(thread::spawn(move || {
            let admitted = expect_execute(inbox.handle(&env(&format!("c-{i}"), Some("s1"))));
            assert_eq!(admitted.ack.status, CommandStatus::Accepted);
            assert_eq!(admitted.admitted_at, 1_700_000_123_000);
            // "active window" = admitted → settle before; windows overlap ⇔ in-transit life cycle overlap.
            let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
            max_active.fetch_max(now_active, Ordering::SeqCst);
            for _ in 0..64 {
                thread::yield_now();
            }
            active.fetch_sub(1, Ordering::SeqCst);
            let seq = admitted.admission_seq;
            seqs.lock().unwrap().push(seq);
            match inbox.settle(admitted.ticket, CommandFinal::accepted()) {
                Ok(SettleOutcome::Settled(_)) => {}
                other => panic!("settle failed: {other:?}"),
            }
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }

    let mut seqs = Arc::try_unwrap(seqs).unwrap().into_inner().unwrap();
    seqs.sort_unstable();
    // Serial admission: At most 1 admission is in progress at any time, and seq is exactly 1..=N once each.
    assert_eq!(max_active.load(Ordering::SeqCst), 1);
    assert_eq!(seqs, (1..=N as u64).collect::<Vec<u64>>());
    assert_eq!(inbox.admission_seq_for(Some("s1")), N as u64);
    assert!(!inbox.has_pinned_session_state(Some("s1")));
}

// ── 4. Idempotent duplicate (waiting for final state in transit / duplicate after closing) ──

#[test]
fn duplicate_command_id_returns_same_admission() {
    let inbox = SharedCommandInbox::new(CommandInbox::new(TestHost::session("s1", 3)));
    let first = expect_execute(inbox.handle(&env("c1", Some("s1"))));
    assert_eq!(first.admission_seq, 1);

    // Repeat while in transit: Block to the same final state (TS key gate `await pinned.final`), set duplicate.
    let (tx, rx) = mpsc::channel();
    let dup_inbox = inbox.clone();
    let resubmit = thread::spawn(move || {
        tx.send(expect_ack(dup_inbox.handle(&env("c1", Some("s1")))))
            .unwrap();
    });
    spin_until("duplicate waiting person in transit", || {
        inbox.waiter_stats().final_waiters == 1
    });
    let settled = settle_ok(&inbox, first.ticket, CommandFinal::accepted());
    assert_eq!(settled.status, CommandStatus::Accepted);
    let inflight_dup = rx.recv().unwrap();
    resubmit.join().unwrap();

    // Bring it up after closing: use the settled idempotent table, the same duplicate, and no new admissionSeq will be generated.
    let settled_dup = expect_ack(inbox.handle(&env("c1", Some("s1"))));

    for dup in [&inflight_dup, &settled_dup] {
        assert_eq!(dup.status, CommandStatus::Duplicate);
        assert_eq!(dup.command_id, "c1");
        assert_eq!(dup.revision_at_decision, 3);
        assert_eq!(dup.reason_code, None);
    }
    assert_eq!(inflight_dup, settled_dup);
    assert_eq!(inbox.admission_seq_for(Some("s1")), 1);
}

// ── 5. pending strict FIFO ──

#[test]
fn pending_queue_is_strict_fifo() {
    let inbox = SharedCommandInbox::new(CommandInbox::new(TestHost::session("s1", 1)));
    let held = expect_execute(inbox.handle(&env("c1", Some("s1"))));
    assert_eq!(held.admission_seq, 1);

    let order = Arc::new(Mutex::new(Vec::new()));
    let mut workers = Vec::new();
    // Release the three submitters in turn, confirm the order of joining the queue one by one, and then put another one down → The queue order is determined.
    for (idx, cid) in ["c2", "c3", "c4"].iter().enumerate() {
        let worker_inbox = inbox.clone();
        let order = Arc::clone(&order);
        let cid = (*cid).to_owned();
        workers.push(thread::spawn(move || {
            let admitted = expect_execute(worker_inbox.handle(&env(&cid, Some("s1"))));
            order.lock().unwrap().push((cid, admitted.admission_seq));
            match worker_inbox.settle(admitted.ticket, CommandFinal::accepted()) {
                Ok(SettleOutcome::Settled(_)) => {}
                other => panic!("settle failed: {other:?}"),
            }
        }));
        let expect: Vec<String> = ["c2", "c3", "c4"]
            .iter()
            .take(idx + 1)
            .map(|s| (*s).to_owned())
            .collect();
        spin_until("pending FIFO prefix", || {
            inbox.pending_command_ids(Some("s1")) == expect
        });
    }
    assert_eq!(
        inbox.pending_command_ids(Some("s1")),
        ["c2".to_owned(), "c3".to_owned(), "c4".to_owned()]
    );

    // Release the leader of the team: c1 closes and presses the order c2 → c3 → c4 to admit.
    settle_ok(&inbox, held.ticket, CommandFinal::accepted());
    for worker in workers {
        worker.join().unwrap();
    }
    let order = Arc::try_unwrap(order).unwrap().into_inner().unwrap();
    assert_eq!(
        order,
        [
            ("c2".to_owned(), 2),
            ("c3".to_owned(), 3),
            ("c4".to_owned(), 4),
        ]
    );
    assert_eq!(inbox.admission_seq_for(Some("s1")), 4);
    assert!(inbox.pending_command_ids(Some("s1")).is_empty());
}

// ── 6. cancel pending: dequeuing does not consume admissionSeq; in transit/unknown → NotPending ──

#[test]
fn cancel_pending_dequeues_without_consuming_seq() {
    let inbox = SharedCommandInbox::new(CommandInbox::new(TestHost::session("s1", 2)));
    let held = expect_execute(inbox.handle(&env("c1", Some("s1"))));
    assert_eq!(held.admission_seq, 1);

    // c2 was canceled in queue: its handle returns admissionCancelled (design branch, see README).
    let (tx, rx) = mpsc::channel();
    let waiter_inbox = inbox.clone();
    let waiter = thread::spawn(move || {
        tx.send(expect_ack(waiter_inbox.handle(&env("c2", Some("s1")))))
            .unwrap();
    });
    spin_until("c2 join the queue", || {
        inbox.pending_command_ids(Some("s1")) == ["c2".to_owned()]
    });
    assert_eq!(
        inbox.cancel_pending(Some("s1"), "c2"),
        CancelOutcome::Dequeued
    );
    let cancelled = rx.recv().unwrap();
    waiter.join().unwrap();
    assert_eq!(cancelled.status, CommandStatus::Rejected);
    assert_reason(&cancelled, REASON_ADMISSION_CANCELLED);
    assert_eq!(cancelled.revision_at_decision, 2);

    // Cancellation does not consume admissionSeq; neither in-transit nor unknown commands are pending.
    assert_eq!(inbox.admission_seq_for(Some("s1")), 1);
    assert_eq!(
        inbox.cancel_pending(Some("s1"), "c1"),
        CancelOutcome::NotPending
    );
    assert_eq!(
        inbox.cancel_pending(Some("s1"), "c-nope"),
        CancelOutcome::NotPending
    );
    // The waiter has been dequeued → Cancel the same id and the same NotPending.
    assert_eq!(
        inbox.cancel_pending(Some("s1"), "c2"),
        CancelOutcome::NotPending
    );
    assert!(inbox.pending_command_ids(Some("s1")).is_empty());

    // After the queue is restored, new commands start from 2 (cancel the unoccupied sequence number).
    settle_ok(&inbox, held.ticket, CommandFinal::accepted());
    let next = expect_execute(inbox.handle(&env("c3", Some("s1"))));
    assert_eq!(next.admission_seq, 2);
    settle_ok(&inbox, next.ticket, CommandFinal::accepted());
}

// ── 7. Settlement credentials: stale token rejection, same token idempotence, NotCurrent after clearing the database ──

#[test]
fn settle_guards_stale_and_idempotent() {
    let inbox = CommandInbox::new(TestHost::session("s1", 1));
    let admitted = expect_execute(inbox.handle(&env("c1", Some("s1"))));
    let ticket = admitted.ticket;

    // Forge run token (stale-run protection): Never accidentally kill the current run.
    let forged = nc_code_core::SettleTicket {
        bucket: ticket.bucket.clone(),
        command_id: ticket.command_id.clone(),
        run_token: ticket.run_token + 999,
    };
    match inbox.settle(forged, CommandFinal::accepted()) {
        Err(SettleError::NotCurrent { command_id }) => assert_eq!(command_id, "c1"),
        other => panic!("Forged tokens should be rejected, getting: {other:?}"),
    }
    // The current operation is not affected: it can still be closed normally.
    let ack = settle_ok(&inbox, ticket.clone(), CommandFinal::accepted());
    assert_eq!(ack.status, CommandStatus::Accepted);

    // Repeat the same token settle → idempotent noop (the settled flag of TS closure).
    match inbox.settle(ticket.clone(), CommandFinal::accepted()) {
        Ok(SettleOutcome::AlreadySettled) => {}
        other => panic!("Repeat settle should be AlreadySettled, get: {other:?}"),
    }

    // clearSession After erasing the idempotent tombstone, the old ticket is neither current nor tombstoned → NotCurrent.
    assert!(inbox.clear_session(Some("s1")));
    match inbox.settle(ticket, CommandFinal::accepted()) {
        Err(SettleError::NotCurrent { command_id }) => assert_eq!(command_id, "c1"),
        other => panic!(
            "The settlement after clearing the library should be NotCurrent and get: {other:?}"
        ),
    }
    // Clear admissionSeq together (mirror TS clearSession).
    assert_eq!(inbox.admission_seq_for(Some("s1")), 0);
}

// ── 8. Query and other final states + retryAck state rules ──

#[test]
fn query_waits_for_final_and_retry_ack_rules() {
    let inbox = SharedCommandInbox::new(CommandInbox::new(TestHost::session("s1", 4)));
    let key = CommandKey::new(Some("s1"), "c1");

    let admitted = expect_execute(inbox.handle(&env("c1", Some("s1"))));
    assert_eq!(admitted.admission_seq, 1);

    // The query blocks the key in transit and waits for the final state (TS `await inflight.final`).
    let (tx, rx) = mpsc::channel();
    let query_inbox = inbox.clone();
    let querier = thread::spawn(move || {
        tx.send(query_inbox.query(std::slice::from_ref(&key)))
            .unwrap();
    });
    spin_until("query is waiting for the final state", || {
        inbox.waiter_stats().final_waiters == 1
    });
    let final_ = CommandFinal::with_reason(CommandStatus::Failed, "fault.exec");
    let settled = settle_ok(&inbox, admitted.ticket, final_);
    assert_eq!(settled.status, CommandStatus::Failed);
    let items = rx.recv().unwrap();
    querier.join().unwrap();
    match &items[0].outcome {
        QueryOutcome::Ack(ack) => {
            // query returns the original final state ACK, without duplicate.
            assert_eq!(ack.status, CommandStatus::Failed);
            assert_eq!(ack.reason_code.as_deref(), Some("fault.exec"));
            assert_eq!(ack.revision_at_decision, 4);
        }
        other => panic!("query should hit the final state and get: {other:?}"),
    }

    // Closed key: direct hit settled; unknown key → Unknown.
    let items = inbox.query(&[
        CommandKey::new(Some("s1"), "c1"),
        CommandKey::new(Some("s1"), "missing"),
    ]);
    match &items[0].outcome {
        QueryOutcome::Ack(ack) => assert_eq!(ack.status, CommandStatus::Failed),
        other => panic!("The settled key should be hit and get: {other:?}"),
    }
    assert_eq!(items[1].outcome, QueryOutcome::Unknown);

    // retryAck: failed is the final state fact → retain the original state (do not convert to duplicate) and do not consume seq.
    let retry = expect_ack(inbox.handle(&env("c1", Some("s1"))));
    assert_eq!(retry.status, CommandStatus::Failed);
    assert_eq!(retry.reason_code.as_deref(), Some("fault.exec"));
    assert_eq!(inbox.admission_seq_for(Some("s1")), 1);

    // retryAck: closed fact that is not failed → rewritten as duplicate (rejected is also rewritten).
    let rejected = expect_execute(inbox.handle(&env("c2", Some("s1"))));
    let final_rejected = CommandFinal::with_reason(CommandStatus::Rejected, "guard.blocked");
    let settled_rejected = settle_ok(&inbox, rejected.ticket, final_rejected);
    assert_eq!(settled_rejected.status, CommandStatus::Rejected);
    let dup = expect_ack(inbox.handle(&env("c2", Some("s1"))));
    assert_eq!(dup.status, CommandStatus::Duplicate);
    assert_eq!(dup.reason_code.as_deref(), Some("guard.blocked"));
    // c1 and c2 each occupy a sequence number; retry does not generate a new sequence number.
    assert_eq!(inbox.admission_seq_for(Some("s1")), 2);
}

// ── 9. guard noop remember (→ duplicate) vs reject do not remember (→ rule again) ──

#[test]
fn guard_noop_is_remembered_reject_is_not() {
    let host = TestHost::session("s1", 4);
    host.set_guard(|envelope| match envelope.command_id.as_str() {
        "c-noop" => GuardDecision::noop("guard.noop"),
        "c-reject" => GuardDecision::reject("guard.reject"),
        _ => GuardDecision::Allow,
    });
    let inbox = CommandInbox::new(host);

    // The noop is recorded in the settled idempotent table → the duvet cover is repeated (reason is retained).
    let noop = expect_ack(inbox.handle(&env("c-noop", Some("s1"))));
    assert_eq!(noop.status, CommandStatus::Noop);
    assert_reason(&noop, "guard.noop");
    let noop_dup = expect_ack(inbox.handle(&env("c-noop", Some("s1"))));
    assert_eq!(noop_dup.status, CommandStatus::Duplicate);
    assert_reason(&noop_dup, "guard.noop");

    // Reject is not remembered → Re-examine and re-judg, still rejected.
    let reject = expect_ack(inbox.handle(&env("c-reject", Some("s1"))));
    assert_eq!(reject.status, CommandStatus::Rejected);
    assert_reason(&reject, "guard.reject");
    let reject_again = expect_ack(inbox.handle(&env("c-reject", Some("s1"))));
    assert_eq!(reject_again.status, CommandStatus::Rejected);
    assert_reason(&reject_again, "guard.reject");

    // Neither path produces admissionSeq.
    assert_eq!(inbox.admission_seq_for(Some("s1")), 0);
}

// ── 10. sessionNotFound / global bucket isolation / admissionSeq clear ──

#[test]
fn session_gate_and_global_bucket_independence() {
    let host = TestHost::default();
    host.set_session("s1", 2);
    let inbox = CommandInbox::new(host);

    // Unknown session → proto.sessionNotFound (decide level, does not consume seq).
    let ack = expect_ack(inbox.handle(&env("c-ghost", Some("ghost"))));
    assert_eq!(ack.status, CommandStatus::Rejected);
    assert_reason(&ack, REASON_SESSION_NOT_FOUND);
    assert_eq!(ack.revision_at_decision, 0);
    assert_eq!(inbox.admission_seq_for(Some("ghost")), 0);

    // Not createSession and sessionId is null → Same as sessionNotFound (mirror decide condition).
    let ack = expect_ack(inbox.handle(&env("c-null", None)));
    assert_eq!(ack.status, CommandStatus::Rejected);
    assert_reason(&ack, REASON_SESSION_NOT_FOUND);

    // createSession + null sessionId → Enter the @global bucket, revision is always 0.
    let create =
        expect_execute(inbox.handle(&env_of("createSession", "c-create", None, None, None)));
    assert_eq!(create.ticket.bucket, "@global");
    assert_eq!(create.admission_seq, 1);
    assert_eq!(create.ack.revision_at_decision, 0);
    assert_eq!(create.queue_item_id, queue_item_id_for_command("c-create"));
    assert_eq!(create.queue_item_id, "queue_c-create");
    settle_ok(&inbox, create.ticket, CommandFinal::accepted());
    assert_eq!(inbox.admission_seq_for(None), 1);

    // The admissionSeq of the session bucket and the @global bucket are independent of each other (each starts from 1).
    let session_cmd = expect_execute(inbox.handle(&env("c-s1", Some("s1"))));
    assert_eq!(session_cmd.admission_seq, 1);
    assert_eq!(session_cmd.ack.revision_at_decision, 2);
    settle_ok(&inbox, session_cmd.ticket, CommandFinal::accepted());
    assert_eq!(inbox.admission_seq_for(Some("s1")), 1);
    assert_eq!(inbox.admission_seq_for(None), 1);
}

// ── 11. CAS ruling: staleRevision / staleLogEpoch / row guard ──

#[test]
fn cas_stale_revision_epoch_and_row_guard() {
    let host = TestHost::session("s1", 5);
    host.set_epoch("s1", "e2");
    let inbox = CommandInbox::new(host);

    // baseRevision is inconsistent with host → stale (only commands in the CAS collection are verified).
    let stale = expect_ack(inbox.handle(&env_of(
        "switchModelConfig",
        "c-stale",
        Some("s1"),
        Some(4),
        None,
    )));
    assert_eq!(stale.status, CommandStatus::Stale);
    assert_reason(&stale, REASON_STALE_REVISION);
    assert_eq!(stale.revision_at_decision, 5);
    assert_eq!(inbox.admission_seq_for(Some("s1")), 0);

    // Non-CAS commands do not check baseRevision (sendText is not in the 16-item set).
    let ignored =
        expect_execute(inbox.handle(&env_of("sendText", "c-any", Some("s1"), Some(999), None)));
    settle_ok(&inbox, ignored.ticket, CommandFinal::accepted());
    assert_eq!(inbox.admission_seq_for(Some("s1")), 1);

    // ROW command: epoch is verified before revision, epoch does not match → staleLogEpoch.
    let epoch_stale = expect_ack(inbox.handle(&env_of(
        "retryTurn",
        "c-epoch",
        Some("s1"),
        Some(5),
        Some("e1"),
    )));
    assert_eq!(epoch_stale.status, CommandStatus::Stale);
    assert_reason(&epoch_stale, REASON_STALE_LOG_EPOCH);
    assert_eq!(epoch_stale.revision_at_decision, 5);

    // epoch + revision matches all → normal admitted.
    let ok = expect_execute(inbox.handle(&env_of(
        "retryTurn",
        "c-row-ok",
        Some("s1"),
        Some(5),
        Some("e2"),
    )));
    assert_eq!(ok.admission_seq, 2);
    settle_ok(&inbox, ok.ticket, CommandFinal::accepted());

    // Row has the same origin as resolver and rejects → rejected (with row’s own reasonCode).
    inbox.host().set_row_guard(|envelope| {
        if envelope.command_id == "c-rowguard" {
            GuardDecision::reject("row.notFound")
        } else {
            GuardDecision::Allow
        }
    });
    let row_reject = expect_ack(inbox.handle(&env_of(
        "retryTurn",
        "c-rowguard",
        Some("s1"),
        Some(5),
        Some("e2"),
    )));
    assert_eq!(row_reject.status, CommandStatus::Rejected);
    assert_reason(&row_reject, "row.notFound");
    assert_eq!(inbox.admission_seq_for(Some("s1")), 2);
}

// ── 12. settled idempotent table LRU: 512 upper limit, eliminate the oldest ──

#[test]
fn settled_lru_evicts_oldest_at_512() {
    assert_eq!(IDEMPOTENCY_TABLE_PER_SESSION, 512);
    let inbox = CommandInbox::new(TestHost::session("s1", 1));

    // Continuously admit + settle 513 items (c0 ... c512), and when inserting c512, the oldest c0 will be eliminated.
    for i in 0..513 {
        let admitted = expect_execute(inbox.handle(&env(&format!("c{i}"), Some("s1"))));
        settle_ok(&inbox, admitted.ticket, CommandFinal::accepted());
    }
    assert_eq!(inbox.admission_seq_for(Some("s1")), 513);

    let items = inbox.query(&[
        CommandKey::new(Some("s1"), "c0"),
        CommandKey::new(Some("s1"), "c1"),
        CommandKey::new(Some("s1"), "c512"),
    ]);
    // c0 is eliminated by LRU (both settled and host return to origin miss) → Unknown.
    assert_eq!(items[0].outcome, QueryOutcome::Unknown);
    // Two items within the boundary are still hit (query does not include duplicate).
    for item in &items[1..] {
        match &item.outcome {
            QueryOutcome::Ack(ack) => assert_eq!(ack.status, CommandStatus::Accepted),
            other => panic!("The command within the bounds should hit and get: {other:?}"),
        }
    }
    // Handle retrieval also reflects the idempotent table hit → duplicate; the eliminated c0 will be re-adjudicated.
    let dup = expect_ack(inbox.handle(&env("c1", Some("s1"))));
    assert_eq!(dup.status, CommandStatus::Duplicate);
    assert_eq!(inbox.admission_seq_for(Some("s1")), 513);
    let evicted = expect_execute(inbox.handle(&env("c0", Some("s1"))));
    assert_eq!(evicted.admission_seq, 514);
    settle_ok(&inbox, evicted.ticket, CommandFinal::accepted());
}

// ── 13. live pin life cycle: pin → settle → release → clear ──

#[test]
fn live_pin_release_clear_lifecycle() {
    let inbox = CommandInbox::new(TestHost::session("s1", 7));
    let key = CommandKey::new(Some("s1"), "c1");

    let admitted = expect_execute(inbox.handle(&env("c1", Some("s1"))));
    assert_eq!(admitted.admission_seq, 1);
    // There is pin state in transit → clearSession rejects (mirror TS hasPinnedSessionState).
    assert!(inbox.has_pinned_session_state(Some("s1")));
    assert!(!inbox.clear_session(Some("s1")));

    // gateway pin live input after admission (intent.sourceCommandId = c1).
    inbox.pin_live_input(Some("s1"), "c1", None);
    let settled = settle_ok(&inbox, admitted.ticket, CommandFinal::accepted());
    assert_eq!(settled.status, CommandStatus::Accepted);

    // The live pin is still there → Clear is still rejected; query takes the live branch to return the complete facts.
    assert!(inbox.has_pinned_session_state(Some("s1")));
    assert!(!inbox.clear_session(Some("s1")));
    let items = inbox.query(std::slice::from_ref(&key));
    match &items[0].outcome {
        QueryOutcome::Ack(ack) => {
            assert_eq!(ack.status, CommandStatus::Accepted);
            assert_eq!(ack.revision_at_decision, 7);
        }
        other => panic!("The live pin should hit and get: {other:?}"),
    }

    // release: The final state is transferred to the settled idempotent table (the run token tombstone is retained), and the pin is released.
    inbox.release_live_input(&key, None);
    assert!(!inbox.has_pinned_session_state(Some("s1")));
    let items = inbox.query(std::slice::from_ref(&key));
    match &items[0].outcome {
        QueryOutcome::Ack(ack) => assert_eq!(ack.status, CommandStatus::Accepted),
        other => panic!("settled If hit, get: {other:?}"),
    }
    let dup = expect_ack(inbox.handle(&env("c1", Some("s1"))));
    assert_eq!(dup.status, CommandStatus::Duplicate);
    assert_eq!(inbox.admission_seq_for(Some("s1")), 1);

    // No pins → clear clears facts and admissionSeq.
    assert!(inbox.clear_session(Some("s1")));
    let items = inbox.query(&[key]);
    assert_eq!(items[0].outcome, QueryOutcome::Unknown);
    assert_eq!(inbox.admission_seq_for(Some("s1")), 0);
}
