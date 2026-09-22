//! owner/lease 表行为测试。
//!
//! TS 参照 `packages/desktop/src/main/taskRealtimeBus.ts`（key 组合、owner/run 冲突、
//! 刷新保留 acquiredAt、release 不匹配静默 no-op）；TTL/renew/steal/assert_current
//! 为 S3 验收新增（TS 源无过期语义）。**零墙钟 sleep**：过期全部由注入的
//! [`ManualClock`] 确定性推进。

use zcode_core::lease::{
    lease_key, AcquireError, LeaseCheckError, LeaseGrantState, ManualClock, OwnerLeaseTable,
    RenewError, StealError,
};

const KEY: &str = "ws-key";
const TTL: i64 = 30_000;

fn table_at(start_ms: i64) -> (OwnerLeaseTable<ManualClock>, ManualClock) {
    let clock = ManualClock::new(start_ms);
    (OwnerLeaseTable::new(clock.clone()), clock)
}

// ── 1. key 组合镜 TS `getLeaseKey`：`${workspaceKey}\0${taskId}` ──

#[test]
fn lease_key_uses_nul_separator() {
    assert_eq!(lease_key("ws", "task"), "ws\0task");
    assert_eq!(lease_key("", ""), "\0");
    // 不同 workspace / task 组合出不同 key（路由隔离的前提）。
    assert_ne!(lease_key("ws-a", "t"), lease_key("ws-a\0t", ""));
}

// ── 2. acquire → renew 保留 acquiredAt；不同 workspace key 互不影响 ──

#[test]
fn acquire_then_renew_preserves_acquired_at_and_isolates_keys() {
    let (mut table, clock) = table_at(1_000);

    let first = table.acquire(KEY, "h1", "r1", TTL).expect("首次获取");
    assert_eq!(first.state, LeaseGrantState::Acquired);
    assert_eq!(first.acquired_at, 1_000);
    assert_eq!(first.updated_at, 1_000);
    assert_eq!(first.expires_at, 1_000 + TTL);

    // 同 owner+run 刷新：镜 TS re-set，保留 acquiredAt，仅推 expiresAt。
    clock.advance_ms(5_000);
    let renewed = table.acquire(KEY, "h1", "r1", TTL).expect("刷新");
    assert_eq!(renewed.state, LeaseGrantState::Renewed);
    assert_eq!(renewed.acquired_at, 1_000);
    assert_eq!(renewed.updated_at, 6_000);
    assert_eq!(renewed.expires_at, 6_000 + TTL);

    // 另一个 workspace key 独立成条（不受 KEY 持有影响）。
    let other = table.acquire("ws-b", "h2", "r9", TTL).expect("独立 key");
    assert_eq!(other.state, LeaseGrantState::Acquired);
    assert_eq!(other.acquired_at, 6_000);
    assert_eq!(table.len(), 2);

    let view = table.get(KEY).expect("KEY 应存在");
    assert_eq!(view.owner_id, "h1");
    assert_eq!(view.run_id, "r1");
    assert_eq!(view.acquired_at, 1_000);
    assert!(!view.expired);
}

// ── 3. 未过期时 owner/run 任一不同 → owned_by_other_host ──

#[test]
fn conflicting_owner_or_run_rejected_while_valid() {
    let (mut table, _clock) = table_at(0);
    table.acquire(KEY, "h1", "r1", TTL).expect("首次获取");

    // 换 owner（TS `owned_by_other_host`）。
    let err = table.acquire(KEY, "h2", "r1", TTL).expect_err("owner 冲突");
    assert_eq!(
        err,
        AcquireError::OwnedByOther {
            owner_id: "h1".to_owned(),
            run_id: "r1".to_owned(),
        }
    );
    // 同 owner 换 run：旧 run 未释放时新 run 不得抢占。
    let err = table.acquire(KEY, "h1", "r2", TTL).expect_err("run 冲突");
    assert_eq!(
        err,
        AcquireError::OwnedByOther {
            owner_id: "h1".to_owned(),
            run_id: "r1".to_owned(),
        }
    );
    // 冲突不改变既有条目。
    let view = table.get(KEY).expect("条目仍在");
    assert_eq!((view.owner_id.as_str(), view.run_id.as_str()), ("h1", "r1"));
}

// ── 4. 注入时钟过期 → steal；到点边界 fail closed ──

#[test]
fn expired_lease_can_be_stolen_and_old_owner_rejected() {
    let (mut table, clock) = table_at(1_000);
    table.acquire(KEY, "h1", "r1", TTL).expect("首次获取"); // expires = 31_000

    // 差 1ms 未过期：不可抢，现 owner 路由校验通过。
    clock.advance_ms(TTL - 1); // now = 30_999
    let err = table
        .steal_if_expired(KEY, "h2", "r2", TTL)
        .expect_err("未过期不可抢");
    assert_eq!(
        err,
        StealError::Held {
            owner_id: "h1".to_owned(),
            run_id: "r1".to_owned(),
        }
    );
    table
        .assert_current(KEY, "h1", "r1")
        .expect("未过期仍为当前持有者");

    // 推进到 expiresAt（now >= expires 即失效）→ 可被接管。
    clock.advance_ms(1); // now = 31_000
    let stolen = table
        .steal_if_expired(KEY, "h2", "r2", TTL)
        .expect("过期可抢");
    assert_eq!(stolen.state, LeaseGrantState::Stolen);
    assert_eq!(stolen.owner_id, "h2");
    assert_eq!(stolen.acquired_at, 31_000);

    // 原 owner 的路由校验被拒（stale-run 防护），新 owner 通过。
    assert_eq!(
        table.assert_current(KEY, "h1", "r1"),
        Err(LeaseCheckError::NotOwner {
            owner_id: "h2".to_owned(),
            run_id: "r2".to_owned(),
        })
    );
    table
        .assert_current(KEY, "h2", "r2")
        .expect("新 owner 为当前持有者");
}

// ── 5. 过期后同 owner 重建（acquiredAt 归零重计）；换 owner 为 steal ──

#[test]
fn expired_same_owner_reacquire_resets_acquired_at() {
    let (mut table, clock) = table_at(0);
    table.acquire(KEY, "h1", "r1", 1_000).expect("首次获取");

    clock.advance_ms(1_000); // 到期
    let rebuilt = table.acquire(KEY, "h1", "r1", 1_000).expect("重建");
    assert_eq!(rebuilt.state, LeaseGrantState::Acquired);
    assert_eq!(rebuilt.acquired_at, 1_000); // 过期视为失效 epoch，从 now 重计

    clock.advance_ms(1_000); // 再次到期
    let stolen = table.acquire(KEY, "h2", "r2", 1_000).expect("接管");
    assert_eq!(stolen.state, LeaseGrantState::Stolen);
    assert_eq!(stolen.acquired_at, 2_000);
}

// ── 6. 同 owner 换 run 接管后，旧 run 的调用 → stale run token；release 静默 no-op ──

#[test]
fn stale_run_token_rejected_after_same_owner_steal() {
    let (mut table, clock) = table_at(0);
    table.acquire(KEY, "h1", "r1", 1_000).expect("首次获取"); // expires = 1_000

    clock.advance_ms(1_000); // 过期
    let stolen = table
        .steal_if_expired(KEY, "h1", "r2", 1_000)
        .expect("同 owner 换 run 接管");
    assert_eq!(stolen.run_id, "r2");

    // 旧 run 续租：owner 相同、run 已变 → StaleRunToken（当前 run = r2）。
    assert_eq!(
        table.renew(KEY, "h1", "r1", 1_000),
        Err(RenewError::StaleRunToken {
            run_id: "r2".to_owned(),
        })
    );
    // 路由校验同样拒绝旧 run。
    assert_eq!(
        table.assert_current(KEY, "h1", "r1"),
        Err(LeaseCheckError::StaleRunToken {
            run_id: "r2".to_owned(),
        })
    );
    table
        .assert_current(KEY, "h1", "r2")
        .expect("新 run 为当前持有者");

    // release 不匹配（run 不同）→ 静默 no-op，条目完整保留（镜 TS 返回 void）。
    assert!(!table.release(KEY, "h1", "r1"));
    let view = table.get(KEY).expect("条目仍在");
    assert_eq!(view.run_id, "r2");
    // owner 不同同样 no-op。
    assert!(!table.release(KEY, "h2", "r2"));
    assert!(table.get(KEY).is_some());
    // 匹配才删除。
    assert!(table.release(KEY, "h1", "r2"));
    assert!(table.get(KEY).is_none());
}

// ── 7. renew 错误分类：NotFound / Expired（过期不复活） ──

#[test]
fn renew_errors_not_found_and_expired() {
    let (mut table, clock) = table_at(0);

    assert_eq!(
        table.renew("missing", "h1", "r1", TTL),
        Err(RenewError::NotFound)
    );

    table.acquire(KEY, "h1", "r1", 1_000).expect("首次获取");
    clock.advance_ms(1_000); // now == expires → 过期
                             // 过期优先于 owner 校验：必须重新 acquire/steal，renew 不复活租约。
    assert_eq!(
        table.renew(KEY, "h1", "r1", 1_000),
        Err(RenewError::Expired)
    );
    // steal 前条目仍在（惰性过期，不删除）。
    assert_eq!(table.len(), 1);
}

// ── 8. release 必须 owner+run 全匹配 ──

#[test]
fn release_requires_owner_and_run_match() {
    let (mut table, _clock) = table_at(0);
    table.acquire(KEY, "h1", "r1", TTL).expect("首次获取");

    assert!(!table.release(KEY, "h2", "r1"), "owner 不匹配");
    assert!(!table.release(KEY, "h1", "r9"), "run 不匹配");
    assert_eq!(table.len(), 1);
    assert!(table.release(KEY, "h1", "r1"));
    assert!(table.is_empty());
    assert!(!table.release(KEY, "h1", "r1"), "已删除 → false");
}

// ── 9. assert_current 在 expiresAt 精确边界 fail closed ──

#[test]
fn assert_current_fails_closed_at_exact_expiry() {
    let (mut table, clock) = table_at(0);
    table.acquire(KEY, "h1", "r1", 1_000).expect("首次获取"); // expires = 1_000

    table.assert_current(KEY, "h1", "r1").expect("未过期");
    clock.set_ms(999);
    table
        .assert_current(KEY, "h1", "r1")
        .expect("差 1ms 未过期");
    clock.set_ms(1_000); // now >= expires → 失效
    assert_eq!(
        table.assert_current(KEY, "h1", "r1"),
        Err(LeaseCheckError::Expired)
    );
}

// ── 10. 非正 TTL 在所有操作上直接拒绝（先于任何状态读写） ──

#[test]
fn non_positive_ttl_rejected_on_all_operations() {
    let (mut table, _clock) = table_at(0);

    assert_eq!(
        table.acquire(KEY, "h1", "r1", 0),
        Err(AcquireError::InvalidTtl)
    );
    assert_eq!(
        table.acquire(KEY, "h1", "r1", -5),
        Err(AcquireError::InvalidTtl)
    );
    assert!(table.is_empty(), "非法 TTL 不得写入条目");

    // renew / steal 的 TTL 校验先于查找：空表也返回 InvalidTtl 而非 NotFound。
    assert_eq!(table.renew(KEY, "h1", "r1", 0), Err(RenewError::InvalidTtl));
    assert_eq!(
        table.steal_if_expired(KEY, "h1", "r1", -1),
        Err(StealError::InvalidTtl)
    );

    // 已持有条目上的非法 TTL 同样拒绝，且不改动既有条目。
    table.acquire(KEY, "h1", "r1", TTL).expect("首次获取");
    assert_eq!(table.renew(KEY, "h1", "r1", 0), Err(RenewError::InvalidTtl));
    assert_eq!(
        table.steal_if_expired(KEY, "h1", "r1", 0),
        Err(StealError::InvalidTtl)
    );
    let view = table.get(KEY).expect("条目未被改动");
    assert_eq!(view.expires_at, TTL);
}

// ── 11. 惰性过期：get 只报告 expired 标志，不删除条目 ──

#[test]
fn expiry_is_lazy_in_view_and_table_len() {
    let (mut table, clock) = table_at(0);
    table.acquire(KEY, "h1", "r1", 1_000).expect("首次获取");

    clock.set_ms(5_000); // 远超 expiresAt
    let view = table.get(KEY).expect("过期条目仍在表内");
    assert!(view.expired);
    assert_eq!(view.owner_id, "h1");
    assert_eq!(table.len(), 1);
    assert!(!table.is_empty());

    // steal 后条目被接管覆盖；release 才会移除。
    table
        .steal_if_expired(KEY, "h2", "r2", 1_000)
        .expect("过期可抢");
    assert!(!table.get(KEY).expect("接管后仍在").expired);
    assert!(table.release(KEY, "h2", "r2"));
    assert!(table.is_empty());
}
