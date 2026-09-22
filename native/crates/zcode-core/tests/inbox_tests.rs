//! CommandInbox 行为测试。
//!
//! 仓库内 `command-inbox.ts` 没有对应 TS 测试文件（已 grep `*.test.ts`/`*.spec.ts`
//! 核实为 0），因此无可移植的 TS 断言；本套例按 TS 源码行为逐条编写：
//! `command-inbox.ts`（admission/FIFO/幂等/retryAck/查询）、`command.ts`
//! （CAS/ROW 集合与解析错误）。全部确定性：并发用线程 + 有界自旋（无墙钟 sleep），
//! 时间走 host 注入的 `now_ms`。

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

use zcode_core::protocol::{
    CommandAck, CommandEnvelope, CommandFinal, CommandInboxHost, CommandKey, CommandStatus,
    GuardDecision, IDEMPOTENCY_TABLE_PER_SESSION, PARSE_CAS_MESSAGE, REASON_INVALID_PAYLOAD,
    REASON_SESSION_NOT_FOUND, REASON_STALE_LOG_EPOCH, REASON_STALE_REVISION,
};
use zcode_core::{
    queue_item_id_for_command, Admitted, CancelOutcome, CommandInbox, Outcome, QueryOutcome,
    SettleError, SettleOutcome, SharedCommandInbox, REASON_ADMISSION_CANCELLED,
};

/// 有界自旋上限：仅让出 CPU，不含任何墙钟等待。
const SPIN_LIMIT: usize = 5_000_000;

/// 等待条件成立（有界 yield，无 sleep）；超时即失败，暴露实现缺陷。
fn spin_until(label: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..SPIN_LIMIT {
        if cond() {
            return;
        }
        thread::yield_now();
    }
    panic!("spin_until 超时未满足: {label}");
}

// ── 测试宿主（固定 now_ms，回调解耦） ──

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

// ── 信封构造（null 字段 = TS 缺省语义） ──

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

/// 默认非 CAS 命令（sendText 不在 `COMMANDS_REQUIRING_BASE_REVISION` 集合内）。
fn env(command_id: &str, session_id: Option<&str>) -> Value {
    env_of("sendText", command_id, session_id, None, None)
}

// ── 断言辅助 ──

fn expect_ack(outcome: Outcome) -> CommandAck {
    match outcome {
        Outcome::Ack(ack) => ack,
        Outcome::Execute(a) => panic!("期望 ack-only，却 admitted: {}", a.ack.command_id),
    }
}

fn expect_execute(outcome: Outcome) -> Box<Admitted> {
    match outcome {
        Outcome::Execute(a) => a,
        Outcome::Ack(ack) => panic!(
            "期望 admitted，却 ack: {} status={:?}",
            ack.command_id, ack.status
        ),
    }
}

fn assert_reason(ack: &CommandAck, expected: &str) {
    assert_eq!(
        ack.reason_code.as_deref(),
        Some(expected),
        "command {} 的 reasonCode 不符: {:?}",
        ack.command_id,
        ack
    );
}

fn settle_ok(
    inbox: &CommandInbox<TestHost>,
    ticket: zcode_core::SettleTicket,
    final_: CommandFinal,
) -> CommandAck {
    match inbox.settle(ticket, final_) {
        Ok(SettleOutcome::Settled(ack)) => ack,
        other => panic!("settle 应成功收口，得到: {other:?}"),
    }
}

// ── 1. 解析失败：ACK + extractCommandId ──

#[test]
fn invalid_payload_rejected_and_extract_command_id() {
    let inbox = CommandInbox::new(TestHost::session("s1", 3));

    // 缺 clientId → proto.invalidPayload（文案自有，reasonCode 对齐 TS）。
    let raw = json!({ "commandId": "c-bad", "sessionId": "s1", "type": "sendText" });
    let ack = expect_ack(inbox.handle(&raw));
    assert_eq!(ack.status, CommandStatus::Rejected);
    assert_reason(&ack, REASON_INVALID_PAYLOAD);
    assert!(ack.message.as_deref().unwrap_or("").contains("clientId"));
    assert_eq!(ack.revision_at_decision, 0);
    // 解析失败不得消耗 admissionSeq。
    assert_eq!(inbox.admission_seq_for(Some("s1")), 0);

    // TS `extractCommandId`：解析失败仍尽力取回 commandId（非字符串 → ""）。
    assert_eq!(zcode_core::protocol::extract_command_id(&raw), "c-bad");
    assert_eq!(
        zcode_core::protocol::extract_command_id(&json!({ "commandId": 7 })),
        ""
    );
    assert_eq!(zcode_core::protocol::extract_command_id(&json!(42)), "");

    // 非法 commandType 同样在解析层拒绝。
    let bad_type = env_of("notACommand", "c-type", Some("s1"), None, None);
    let ack = expect_ack(inbox.handle(&bad_type));
    assert_eq!(ack.status, CommandStatus::Rejected);
    assert_reason(&ack, REASON_INVALID_PAYLOAD);
}

// ── 2. CAS/ROW 字段缺失在解析层拒绝（镜 command.ts 的合成文案） ──

#[test]
fn cas_commands_missing_fields_fail_at_parse() {
    let host = TestHost::session("s1", 5);
    host.set_epoch("s1", "e2");
    let inbox = CommandInbox::new(host);

    // CAS 命令缺 baseRevision → parse 拒绝，文案 = TS zod 合成错误。
    let missing_base = env_of("switchModelConfig", "c-cas", Some("s1"), None, None);
    let ack = expect_ack(inbox.handle(&missing_base));
    assert_eq!(ack.status, CommandStatus::Rejected);
    assert_reason(&ack, REASON_INVALID_PAYLOAD);
    assert_eq!(ack.message.as_deref(), Some(PARSE_CAS_MESSAGE));
    assert_eq!(inbox.admission_seq_for(Some("s1")), 0);

    // ROW 命令缺 baseLogEpoch → 同一合成文案（parse 级要求）。
    let missing_epoch = env_of("retryTurn", "c-row", Some("s1"), Some(5), None);
    let ack = expect_ack(inbox.handle(&missing_epoch));
    assert_eq!(ack.status, CommandStatus::Rejected);
    assert_reason(&ack, REASON_INVALID_PAYLOAD);
    assert_eq!(ack.message.as_deref(), Some(PARSE_CAS_MESSAGE));
    assert_eq!(inbox.admission_seq_for(Some("s1")), 0);

    // 字段齐备 + 与 host 一致 → 正常 admitted（证明上面失败确因缺字段）。
    let ok = env_of("retryTurn", "c-row-ok", Some("s1"), Some(5), Some("e2"));
    let admitted = expect_execute(inbox.handle(&ok));
    assert_eq!(admitted.ack.status, CommandStatus::Accepted);
    settle_ok(&inbox, admitted.ticket, CommandFinal::accepted());
}

// ── 3. 并发 admission 串行（max-active == 1 + admissionSeq 1..=N 唯一） ──

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
            // “活跃窗口” = admitted → settle 前；窗口重叠 ⇔ 在途生命周期重叠。
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
                other => panic!("settle 失败: {other:?}"),
            }
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }

    let mut seqs = Arc::try_unwrap(seqs).unwrap().into_inner().unwrap();
    seqs.sort_unstable();
    // 串行 admission：任意时刻至多 1 条在途，且 seq 恰为 1..=N 各一次。
    assert_eq!(max_active.load(Ordering::SeqCst), 1);
    assert_eq!(seqs, (1..=N as u64).collect::<Vec<u64>>());
    assert_eq!(inbox.admission_seq_for(Some("s1")), N as u64);
    assert!(!inbox.has_pinned_session_state(Some("s1")));
}

// ── 4. 幂等重提（在途等待终态 / 收口后 duplicate） ──

#[test]
fn duplicate_command_id_returns_same_admission() {
    let inbox = SharedCommandInbox::new(CommandInbox::new(TestHost::session("s1", 3)));
    let first = expect_execute(inbox.handle(&env("c1", Some("s1"))));
    assert_eq!(first.admission_seq, 1);

    // 在途期间重提：阻塞到同一终态（TS key gate `await pinned.final`），套 duplicate。
    let (tx, rx) = mpsc::channel();
    let dup_inbox = inbox.clone();
    let resubmit = thread::spawn(move || {
        tx.send(expect_ack(dup_inbox.handle(&env("c1", Some("s1")))))
            .unwrap();
    });
    spin_until("在途 duplicate 等待者", || {
        inbox.waiter_stats().final_waiters == 1
    });
    let settled = settle_ok(&inbox, first.ticket, CommandFinal::accepted());
    assert_eq!(settled.status, CommandStatus::Accepted);
    let inflight_dup = rx.recv().unwrap();
    resubmit.join().unwrap();

    // 收口后再提：走 settled 幂等表，同样 duplicate，且不产生新 admissionSeq。
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

// ── 5. pending 严格 FIFO ──

#[test]
fn pending_queue_is_strict_fifo() {
    let inbox = SharedCommandInbox::new(CommandInbox::new(TestHost::session("s1", 1)));
    let held = expect_execute(inbox.handle(&env("c1", Some("s1"))));
    assert_eq!(held.admission_seq, 1);

    let order = Arc::new(Mutex::new(Vec::new()));
    let mut workers = Vec::new();
    // 依次放行三个提交者，逐个确认入队顺序后再放下一个 → 队列序确定。
    for (idx, cid) in ["c2", "c3", "c4"].iter().enumerate() {
        let worker_inbox = inbox.clone();
        let order = Arc::clone(&order);
        let cid = (*cid).to_owned();
        workers.push(thread::spawn(move || {
            let admitted = expect_execute(worker_inbox.handle(&env(&cid, Some("s1"))));
            order.lock().unwrap().push((cid, admitted.admission_seq));
            match worker_inbox.settle(admitted.ticket, CommandFinal::accepted()) {
                Ok(SettleOutcome::Settled(_)) => {}
                other => panic!("settle 失败: {other:?}"),
            }
        }));
        let expect: Vec<String> = ["c2", "c3", "c4"]
            .iter()
            .take(idx + 1)
            .map(|s| (*s).to_owned())
            .collect();
        spin_until("pending FIFO 前缀", || {
            inbox.pending_command_ids(Some("s1")) == expect
        });
    }
    assert_eq!(
        inbox.pending_command_ids(Some("s1")),
        ["c2".to_owned(), "c3".to_owned(), "c4".to_owned()]
    );

    // 放行队头：c1 收口后按 c2 → c3 → c4 顺序 admit。
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

// ── 6. cancel pending：出队不消耗 admissionSeq；在途/未知 → NotPending ──

#[test]
fn cancel_pending_dequeues_without_consuming_seq() {
    let inbox = SharedCommandInbox::new(CommandInbox::new(TestHost::session("s1", 2)));
    let held = expect_execute(inbox.handle(&env("c1", Some("s1"))));
    assert_eq!(held.admission_seq, 1);

    // c2 排队中被取消：其 handle 返回 admissionCancelled（设计分支，见 README）。
    let (tx, rx) = mpsc::channel();
    let waiter_inbox = inbox.clone();
    let waiter = thread::spawn(move || {
        tx.send(expect_ack(waiter_inbox.handle(&env("c2", Some("s1")))))
            .unwrap();
    });
    spin_until("c2 入队", || {
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

    // 取消不消耗 admissionSeq；在途命令与未知命令都不是 pending。
    assert_eq!(inbox.admission_seq_for(Some("s1")), 1);
    assert_eq!(
        inbox.cancel_pending(Some("s1"), "c1"),
        CancelOutcome::NotPending
    );
    assert_eq!(
        inbox.cancel_pending(Some("s1"), "c-nope"),
        CancelOutcome::NotPending
    );
    // 等待者已出队 → 再取消同一 id 同样 NotPending。
    assert_eq!(
        inbox.cancel_pending(Some("s1"), "c2"),
        CancelOutcome::NotPending
    );
    assert!(inbox.pending_command_ids(Some("s1")).is_empty());

    // 队列恢复后，新命令从 2 开始（取消未占用序号）。
    settle_ok(&inbox, held.ticket, CommandFinal::accepted());
    let next = expect_execute(inbox.handle(&env("c3", Some("s1"))));
    assert_eq!(next.admission_seq, 2);
    settle_ok(&inbox, next.ticket, CommandFinal::accepted());
}

// ── 7. settle 凭证：stale token 拒绝、同 token 幂等、清库后 NotCurrent ──

#[test]
fn settle_guards_stale_and_idempotent() {
    let inbox = CommandInbox::new(TestHost::session("s1", 1));
    let admitted = expect_execute(inbox.handle(&env("c1", Some("s1"))));
    let ticket = admitted.ticket;

    // 伪造 run token（stale-run 防护）：绝不误杀当前运行。
    let forged = zcode_core::SettleTicket {
        bucket: ticket.bucket.clone(),
        command_id: ticket.command_id.clone(),
        run_token: ticket.run_token + 999,
    };
    match inbox.settle(forged, CommandFinal::accepted()) {
        Err(SettleError::NotCurrent { command_id }) => assert_eq!(command_id, "c1"),
        other => panic!("伪造 token 应被拒绝，得到: {other:?}"),
    }
    // 当前运行不受影响：仍可正常收口。
    let ack = settle_ok(&inbox, ticket.clone(), CommandFinal::accepted());
    assert_eq!(ack.status, CommandStatus::Accepted);

    // 同 token 重复 settle → 幂等 noop（TS closure 的 settled 标志）。
    match inbox.settle(ticket.clone(), CommandFinal::accepted()) {
        Ok(SettleOutcome::AlreadySettled) => {}
        other => panic!("重复 settle 应为 AlreadySettled，得到: {other:?}"),
    }

    // clearSession 抹掉幂等墓碑后，旧 ticket 既非当前也无墓碑 → NotCurrent。
    assert!(inbox.clear_session(Some("s1")));
    match inbox.settle(ticket, CommandFinal::accepted()) {
        Err(SettleError::NotCurrent { command_id }) => assert_eq!(command_id, "c1"),
        other => panic!("清库后的 settle 应 NotCurrent，得到: {other:?}"),
    }
    // admissionSeq 一并清零（镜 TS clearSession）。
    assert_eq!(inbox.admission_seq_for(Some("s1")), 0);
}

// ── 8. query 等终态 + retryAck 状态规则 ──

#[test]
fn query_waits_for_final_and_retry_ack_rules() {
    let inbox = SharedCommandInbox::new(CommandInbox::new(TestHost::session("s1", 4)));
    let key = CommandKey::new(Some("s1"), "c1");

    let admitted = expect_execute(inbox.handle(&env("c1", Some("s1"))));
    assert_eq!(admitted.admission_seq, 1);

    // query 对在途 key 阻塞等待终态（TS `await inflight.final`）。
    let (tx, rx) = mpsc::channel();
    let query_inbox = inbox.clone();
    let querier = thread::spawn(move || {
        tx.send(query_inbox.query(std::slice::from_ref(&key)))
            .unwrap();
    });
    spin_until("query 等待在途终态", || {
        inbox.waiter_stats().final_waiters == 1
    });
    let final_ = CommandFinal::with_reason(CommandStatus::Failed, "fault.exec");
    let settled = settle_ok(&inbox, admitted.ticket, final_);
    assert_eq!(settled.status, CommandStatus::Failed);
    let items = rx.recv().unwrap();
    querier.join().unwrap();
    match &items[0].outcome {
        QueryOutcome::Ack(ack) => {
            // query 返回原终态 ACK，不套 duplicate。
            assert_eq!(ack.status, CommandStatus::Failed);
            assert_eq!(ack.reason_code.as_deref(), Some("fault.exec"));
            assert_eq!(ack.revision_at_decision, 4);
        }
        other => panic!("query 应命中终态，得到: {other:?}"),
    }

    // 已收口 key：直接命中 settled；未知 key → Unknown。
    let items = inbox.query(&[
        CommandKey::new(Some("s1"), "c1"),
        CommandKey::new(Some("s1"), "missing"),
    ]);
    match &items[0].outcome {
        QueryOutcome::Ack(ack) => assert_eq!(ack.status, CommandStatus::Failed),
        other => panic!("settled key 应命中，得到: {other:?}"),
    }
    assert_eq!(items[1].outcome, QueryOutcome::Unknown);

    // retryAck：failed 是终态事实 → 保留原状态（不转 duplicate），不消耗 seq。
    let retry = expect_ack(inbox.handle(&env("c1", Some("s1"))));
    assert_eq!(retry.status, CommandStatus::Failed);
    assert_eq!(retry.reason_code.as_deref(), Some("fault.exec"));
    assert_eq!(inbox.admission_seq_for(Some("s1")), 1);

    // retryAck：非 failed 的已收口事实 → 改写为 duplicate（rejected 同样被改写）。
    let rejected = expect_execute(inbox.handle(&env("c2", Some("s1"))));
    let final_rejected = CommandFinal::with_reason(CommandStatus::Rejected, "guard.blocked");
    let settled_rejected = settle_ok(&inbox, rejected.ticket, final_rejected);
    assert_eq!(settled_rejected.status, CommandStatus::Rejected);
    let dup = expect_ack(inbox.handle(&env("c2", Some("s1"))));
    assert_eq!(dup.status, CommandStatus::Duplicate);
    assert_eq!(dup.reason_code.as_deref(), Some("guard.blocked"));
    // c1、c2 各占一个序号；retry 不产生新序号。
    assert_eq!(inbox.admission_seq_for(Some("s1")), 2);
}

// ── 9. guard noop 记忆（→ duplicate） vs reject 不记忆（→ 再次裁决） ──

#[test]
fn guard_noop_is_remembered_reject_is_not() {
    let host = TestHost::session("s1", 4);
    host.set_guard(|envelope| match envelope.command_id.as_str() {
        "c-noop" => GuardDecision::noop("guard.noop"),
        "c-reject" => GuardDecision::reject("guard.reject"),
        _ => GuardDecision::Allow,
    });
    let inbox = CommandInbox::new(host);

    // noop 记入 settled 幂等表 → 重提被套 duplicate（reason 保留）。
    let noop = expect_ack(inbox.handle(&env("c-noop", Some("s1"))));
    assert_eq!(noop.status, CommandStatus::Noop);
    assert_reason(&noop, "guard.noop");
    let noop_dup = expect_ack(inbox.handle(&env("c-noop", Some("s1"))));
    assert_eq!(noop_dup.status, CommandStatus::Duplicate);
    assert_reason(&noop_dup, "guard.noop");

    // reject 不记忆 → 重提重新裁决，仍然 rejected。
    let reject = expect_ack(inbox.handle(&env("c-reject", Some("s1"))));
    assert_eq!(reject.status, CommandStatus::Rejected);
    assert_reason(&reject, "guard.reject");
    let reject_again = expect_ack(inbox.handle(&env("c-reject", Some("s1"))));
    assert_eq!(reject_again.status, CommandStatus::Rejected);
    assert_reason(&reject_again, "guard.reject");

    // 两条路径都不产生 admissionSeq。
    assert_eq!(inbox.admission_seq_for(Some("s1")), 0);
}

// ── 10. sessionNotFound / 全局桶隔离 / admissionSeq 清零 ──

#[test]
fn session_gate_and_global_bucket_independence() {
    let host = TestHost::default();
    host.set_session("s1", 2);
    let inbox = CommandInbox::new(host);

    // 未知会话 → proto.sessionNotFound（decide 级，不消耗 seq）。
    let ack = expect_ack(inbox.handle(&env("c-ghost", Some("ghost"))));
    assert_eq!(ack.status, CommandStatus::Rejected);
    assert_reason(&ack, REASON_SESSION_NOT_FOUND);
    assert_eq!(ack.revision_at_decision, 0);
    assert_eq!(inbox.admission_seq_for(Some("ghost")), 0);

    // 非 createSession 且 sessionId 为 null → 同样 sessionNotFound（镜 decide 条件）。
    let ack = expect_ack(inbox.handle(&env("c-null", None)));
    assert_eq!(ack.status, CommandStatus::Rejected);
    assert_reason(&ack, REASON_SESSION_NOT_FOUND);

    // createSession + null sessionId → 入 @global 桶，revision 恒 0。
    let create =
        expect_execute(inbox.handle(&env_of("createSession", "c-create", None, None, None)));
    assert_eq!(create.ticket.bucket, "@global");
    assert_eq!(create.admission_seq, 1);
    assert_eq!(create.ack.revision_at_decision, 0);
    assert_eq!(create.queue_item_id, queue_item_id_for_command("c-create"));
    assert_eq!(create.queue_item_id, "queue_c-create");
    settle_ok(&inbox, create.ticket, CommandFinal::accepted());
    assert_eq!(inbox.admission_seq_for(None), 1);

    // 会话桶与 @global 桶的 admissionSeq 互相独立（各自从 1 起）。
    let session_cmd = expect_execute(inbox.handle(&env("c-s1", Some("s1"))));
    assert_eq!(session_cmd.admission_seq, 1);
    assert_eq!(session_cmd.ack.revision_at_decision, 2);
    settle_ok(&inbox, session_cmd.ticket, CommandFinal::accepted());
    assert_eq!(inbox.admission_seq_for(Some("s1")), 1);
    assert_eq!(inbox.admission_seq_for(None), 1);
}

// ── 11. CAS 裁决：staleRevision / staleLogEpoch / row guard ──

#[test]
fn cas_stale_revision_epoch_and_row_guard() {
    let host = TestHost::session("s1", 5);
    host.set_epoch("s1", "e2");
    let inbox = CommandInbox::new(host);

    // baseRevision 与 host 不一致 → stale（CAS 集合内的命令才校验）。
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

    // 非 CAS 命令不校验 baseRevision（sendText 不在 16 项集合内）。
    let ignored =
        expect_execute(inbox.handle(&env_of("sendText", "c-any", Some("s1"), Some(999), None)));
    settle_ok(&inbox, ignored.ticket, CommandFinal::accepted());
    assert_eq!(inbox.admission_seq_for(Some("s1")), 1);

    // ROW 命令：epoch 先于 revision 校验，epoch 不匹配 → staleLogEpoch。
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

    // epoch + revision 全部匹配 → 正常 admitted。
    let ok = expect_execute(inbox.handle(&env_of(
        "retryTurn",
        "c-row-ok",
        Some("s1"),
        Some(5),
        Some("e2"),
    )));
    assert_eq!(ok.admission_seq, 2);
    settle_ok(&inbox, ok.ticket, CommandFinal::accepted());

    // row 同源 resolver 裁决 reject → rejected（带 row 自身 reasonCode）。
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

// ── 12. settled 幂等表 LRU：512 上限，淘汰最旧 ──

#[test]
fn settled_lru_evicts_oldest_at_512() {
    assert_eq!(IDEMPOTENCY_TABLE_PER_SESSION, 512);
    let inbox = CommandInbox::new(TestHost::session("s1", 1));

    // 连续 admit + settle 513 条（c0 … c512），插入 c512 时淘汰最旧的 c0。
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
    // c0 被 LRU 淘汰（settled 与 host 回源均未命中）→ Unknown。
    assert_eq!(items[0].outcome, QueryOutcome::Unknown);
    // 边界内两条仍命中（query 不套 duplicate）。
    for item in &items[1..] {
        match &item.outcome {
            QueryOutcome::Ack(ack) => assert_eq!(ack.status, CommandStatus::Accepted),
            other => panic!("边界内命令应命中，得到: {other:?}"),
        }
    }
    // handle 重提同样体现幂等表命中 → duplicate；被淘汰的 c0 则会重新裁决。
    let dup = expect_ack(inbox.handle(&env("c1", Some("s1"))));
    assert_eq!(dup.status, CommandStatus::Duplicate);
    assert_eq!(inbox.admission_seq_for(Some("s1")), 513);
    let evicted = expect_execute(inbox.handle(&env("c0", Some("s1"))));
    assert_eq!(evicted.admission_seq, 514);
    settle_ok(&inbox, evicted.ticket, CommandFinal::accepted());
}

// ── 13. live pin 生命周期：pin → settle → release → clear ──

#[test]
fn live_pin_release_clear_lifecycle() {
    let inbox = CommandInbox::new(TestHost::session("s1", 7));
    let key = CommandKey::new(Some("s1"), "c1");

    let admitted = expect_execute(inbox.handle(&env("c1", Some("s1"))));
    assert_eq!(admitted.admission_seq, 1);
    // 在途即有 pin 态 → clearSession 拒绝（镜 TS hasPinnedSessionState）。
    assert!(inbox.has_pinned_session_state(Some("s1")));
    assert!(!inbox.clear_session(Some("s1")));

    // gateway 在 admission 后 pin live input（intent.sourceCommandId = c1）。
    inbox.pin_live_input(Some("s1"), "c1", None);
    let settled = settle_ok(&inbox, admitted.ticket, CommandFinal::accepted());
    assert_eq!(settled.status, CommandStatus::Accepted);

    // live pin 仍在 → 仍拒绝 clear；query 走 live 分支返回完整事实。
    assert!(inbox.has_pinned_session_state(Some("s1")));
    assert!(!inbox.clear_session(Some("s1")));
    let items = inbox.query(std::slice::from_ref(&key));
    match &items[0].outcome {
        QueryOutcome::Ack(ack) => {
            assert_eq!(ack.status, CommandStatus::Accepted);
            assert_eq!(ack.revision_at_decision, 7);
        }
        other => panic!("live pin 应命中，得到: {other:?}"),
    }

    // release：终态转入 settled 幂等表（run token 墓碑保留），pin 解除。
    inbox.release_live_input(&key, None);
    assert!(!inbox.has_pinned_session_state(Some("s1")));
    let items = inbox.query(std::slice::from_ref(&key));
    match &items[0].outcome {
        QueryOutcome::Ack(ack) => assert_eq!(ack.status, CommandStatus::Accepted),
        other => panic!("settled 应命中，得到: {other:?}"),
    }
    let dup = expect_ack(inbox.handle(&env("c1", Some("s1"))));
    assert_eq!(dup.status, CommandStatus::Duplicate);
    assert_eq!(inbox.admission_seq_for(Some("s1")), 1);

    // 无任何 pin → clear 清空事实与 admissionSeq。
    assert!(inbox.clear_session(Some("s1")));
    let items = inbox.query(&[key]);
    assert_eq!(items[0].outcome, QueryOutcome::Unknown);
    assert_eq!(inbox.admission_seq_for(Some("s1")), 0);
}
