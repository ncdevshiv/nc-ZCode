//! owner/lease table behavior test.
//!
//! TS refers to `packages/desktop/src/main/taskRealtimeBus.ts` (key combination, owner/run conflict,
//! Refresh retained acquiredAt, release does not match silent no-op); TTL/renew/steal/assert_current
//! Added for S3 acceptance (no expiration semantics for TS sources). **Zero wall clock sleep**: expires all injected by
//! [`ManualClock`] Deterministic advancement.

use nc_code_core::lease::{
    lease_key, AcquireError, LeaseCheckError, LeaseGrantState, ManualClock, OwnerLeaseTable,
    RenewError, StealError,
};

const KEY: &str = "ws-key";
const TTL: i64 = 30_000;

fn table_at(start_ms: i64) -> (OwnerLeaseTable<ManualClock>, ManualClock) {
    let clock = ManualClock::new(start_ms);
    (OwnerLeaseTable::new(clock.clone()), clock)
}

// ── 1. key combination mirror TS `getLeaseKey`: `${workspaceKey}\0${taskId}` ──

#[test]
fn lease_key_uses_nul_separator() {
    assert_eq!(lease_key("ws", "task"), "ws\0task");
    assert_eq!(lease_key("", ""), "\0");
    // Different workspace/task combinations produce different keys (the prerequisite for routing isolation).
    assert_ne!(lease_key("ws-a", "t"), lease_key("ws-a\0t", ""));
}

// ── 2. acquire → renew reserved acquiredAt; different workspace keys do not affect each other ──

#[test]
fn acquire_then_renew_preserves_acquired_at_and_isolates_keys() {
    let (mut table, clock) = table_at(1_000);

    let first = table
        .acquire(KEY, "h1", "r1", TTL)
        .expect("First time acquisition");
    assert_eq!(first.state, LeaseGrantState::Acquired);
    assert_eq!(first.acquired_at, 1_000);
    assert_eq!(first.updated_at, 1_000);
    assert_eq!(first.expires_at, 1_000 + TTL);

    //Same as owner+run refresh: mirror TS re-set, retain acquiredAt, and only push expiresAt.
    clock.advance_ms(5_000);
    let renewed = table.acquire(KEY, "h1", "r1", TTL).expect("refresh");
    assert_eq!(renewed.state, LeaseGrantState::Renewed);
    assert_eq!(renewed.acquired_at, 1_000);
    assert_eq!(renewed.updated_at, 6_000);
    assert_eq!(renewed.expires_at, 6_000 + TTL);

    // Another workspace key is striped independently (not affected by KEY holding).
    let other = table
        .acquire("ws-b", "h2", "r9", TTL)
        .expect("independent key");
    assert_eq!(other.state, LeaseGrantState::Acquired);
    assert_eq!(other.acquired_at, 6_000);
    assert_eq!(table.len(), 2);

    let view = table.get(KEY).expect("KEY should exist");
    assert_eq!(view.owner_id, "h1");
    assert_eq!(view.run_id, "r1");
    assert_eq!(view.acquired_at, 1_000);
    assert!(!view.expired);
}

// ── 3. When not expired, owner/run is different → owned_by_other_host ──

#[test]
fn conflicting_owner_or_run_rejected_while_valid() {
    let (mut table, _clock) = table_at(0);
    table
        .acquire(KEY, "h1", "r1", TTL)
        .expect("First time acquisition");

    // Change owner (TS `owned_by_other_host`).
    let err = table
        .acquire(KEY, "h2", "r1", TTL)
        .expect_err("owner conflict");
    assert_eq!(
        err,
        AcquireError::OwnedByOther {
            owner_id: "h1".to_owned(),
            run_id: "r1".to_owned(),
        }
    );
    //Same as owner for run: the new run cannot preempt when the old run is not released.
    let err = table
        .acquire(KEY, "h1", "r2", TTL)
        .expect_err("run conflict");
    assert_eq!(
        err,
        AcquireError::OwnedByOther {
            owner_id: "h1".to_owned(),
            run_id: "r1".to_owned(),
        }
    );
    // Conflicts do not change existing entries.
    let view = table.get(KEY).expect("The entry is still");
    assert_eq!((view.owner_id.as_str(), view.run_id.as_str()), ("h1", "r1"));
}

// ── 4. Injection clock expires → steal; fail closed at point boundary ──

#[test]
fn expired_lease_can_be_stolen_and_old_owner_rejected() {
    let (mut table, clock) = table_at(1_000);
    table
        .acquire(KEY, "h1", "r1", TTL)
        .expect("First time acquisition"); // expires = 31_000

    // The difference is 1ms and has not expired: it cannot be snatched, and the owner route verification has passed.
    clock.advance_ms(TTL - 1); // now = 30_999
    let err = table
        .steal_if_expired(KEY, "h2", "r2", TTL)
        .expect_err("Not available before expiration");
    assert_eq!(
        err,
        StealError::Held {
            owner_id: "h1".to_owned(),
            run_id: "r1".to_owned(),
        }
    );
    table
        .assert_current(KEY, "h1", "r1")
        .expect("It has not expired and is still the current holder.");

    // Advance to expiresAt (now >= expires, that is, invalid) → can be taken over.
    clock.advance_ms(1); // now = 31_000
    let stolen = table
        .steal_if_expired(KEY, "h2", "r2", TTL)
        .expect("Expired and available for grabs");
    assert_eq!(stolen.state, LeaseGrantState::Stolen);
    assert_eq!(stolen.owner_id, "h2");
    assert_eq!(stolen.acquired_at, 31_000);

    // The original owner's route verification was rejected (stale-run protection), and the new owner passed.
    assert_eq!(
        table.assert_current(KEY, "h1", "r1"),
        Err(LeaseCheckError::NotOwner {
            owner_id: "h2".to_owned(),
            run_id: "r2".to_owned(),
        })
    );
    table
        .assert_current(KEY, "h2", "r2")
        .expect("The new owner is the current owner");
}

// ── 5. Rebuild with owner after expiration (acquiredAt is reset to zero and recalculated); change owner to steal ──

#[test]
fn expired_same_owner_reacquire_resets_acquired_at() {
    let (mut table, clock) = table_at(0);
    table
        .acquire(KEY, "h1", "r1", 1_000)
        .expect("First time acquisition");

    clock.advance_ms(1_000); // 到期
    let rebuilt = table
        .acquire(KEY, "h1", "r1", 1_000)
        .expect("reconstruction");
    assert_eq!(rebuilt.state, LeaseGrantState::Acquired);
    assert_eq!(rebuilt.acquired_at, 1_000); // 过期视为失效 epoch，从 now 重计

    clock.advance_ms(1_000); // 再次到期
    let stolen = table.acquire(KEY, "h2", "r2", 1_000).expect("take over");
    assert_eq!(stolen.state, LeaseGrantState::Stolen);
    assert_eq!(stolen.acquired_at, 2_000);
}

// ── 6. After changing the owner to run and taking over, the call of the old run → stale run token; release silent no-op ──

#[test]
fn stale_run_token_rejected_after_same_owner_steal() {
    let (mut table, clock) = table_at(0);
    table
        .acquire(KEY, "h1", "r1", 1_000)
        .expect("First time acquisition"); // expires = 1_000

    clock.advance_ms(1_000); // 过期
    let stolen = table
        .steal_if_expired(KEY, "h1", "r2", 1_000)
        .expect("Use owner instead of run to take over");
    assert_eq!(stolen.run_id, "r2");

    // Old run lease renewal: owner is the same, run has changed → StaleRunToken (current run = r2).
    assert_eq!(
        table.renew(KEY, "h1", "r1", 1_000),
        Err(RenewError::StaleRunToken {
            run_id: "r2".to_owned(),
        })
    );
    //Route verification also rejects old runs.
    assert_eq!(
        table.assert_current(KEY, "h1", "r1"),
        Err(LeaseCheckError::StaleRunToken {
            run_id: "r2".to_owned(),
        })
    );
    table
        .assert_current(KEY, "h1", "r2")
        .expect("The new run is the current owner");

    // release does not match (run is different) → silent no-op, entries are kept intact (mirror TS returns void).
    assert!(!table.release(KEY, "h1", "r1"));
    let view = table.get(KEY).expect("The entry is still");
    assert_eq!(view.run_id, "r2");
    // Owner is different and the same is no-op.
    assert!(!table.release(KEY, "h2", "r2"));
    assert!(table.get(KEY).is_some());
    //Delete only if matched.
    assert!(table.release(KEY, "h1", "r2"));
    assert!(table.get(KEY).is_none());
}

// ── 7. renew Error classification: NotFound / Expired (no resurrection after expiration) ──

#[test]
fn renew_errors_not_found_and_expired() {
    let (mut table, clock) = table_at(0);

    assert_eq!(
        table.renew("missing", "h1", "r1", TTL),
        Err(RenewError::NotFound)
    );

    table
        .acquire(KEY, "h1", "r1", 1_000)
        .expect("First time acquisition");
    clock.advance_ms(1_000); // now == expires → 过期
                             // Expiration takes precedence over owner verification: acquisition/steal must be reacquired, renew does not revive the lease.
    assert_eq!(
        table.renew(KEY, "h1", "r1", 1_000),
        Err(RenewError::Expired)
    );
    // The entry before steal is still there (lazy expiration, not deleted).
    assert_eq!(table.len(), 1);
}

// ── 8. release must match owner+run ──

#[test]
fn release_requires_owner_and_run_match() {
    let (mut table, _clock) = table_at(0);
    table
        .acquire(KEY, "h1", "r1", TTL)
        .expect("First time acquisition");

    assert!(!table.release(KEY, "h2", "r1"), "owner does not match");
    assert!(!table.release(KEY, "h1", "r9"), "run does not match");
    assert_eq!(table.len(), 1);
    assert!(table.release(KEY, "h1", "r1"));
    assert!(table.is_empty());
    assert!(!table.release(KEY, "h1", "r1"), "deleted → false");
}

// ── 9. assert_current at the exact boundary of expiresAt fail closed ──

#[test]
fn assert_current_fails_closed_at_exact_expiry() {
    let (mut table, clock) = table_at(0);
    table
        .acquire(KEY, "h1", "r1", 1_000)
        .expect("First time acquisition"); // expires = 1_000

    table.assert_current(KEY, "h1", "r1").expect("Not expired");
    clock.set_ms(999);
    table
        .assert_current(KEY, "h1", "r1")
        .expect("1ms difference, not expired");
    clock.set_ms(1_000); // now >= expires → 失效
    assert_eq!(
        table.assert_current(KEY, "h1", "r1"),
        Err(LeaseCheckError::Expired)
    );
}

// ── 10. Non-positive TTL rejects directly on all operations (read and write before any status) ──

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
    assert!(table.is_empty(), "Illegal TTL entry must not be written");

    // TTL check for renew/steal precedes lookup: empty tables also return InvalidTtl instead of NotFound.
    assert_eq!(table.renew(KEY, "h1", "r1", 0), Err(RenewError::InvalidTtl));
    assert_eq!(
        table.steal_if_expired(KEY, "h1", "r1", -1),
        Err(StealError::InvalidTtl)
    );

    // Illegal TTLs on already held entries are also rejected and existing entries are not modified.
    table
        .acquire(KEY, "h1", "r1", TTL)
        .expect("First time acquisition");
    assert_eq!(table.renew(KEY, "h1", "r1", 0), Err(RenewError::InvalidTtl));
    assert_eq!(
        table.steal_if_expired(KEY, "h1", "r1", 0),
        Err(StealError::InvalidTtl)
    );
    let view = table.get(KEY).expect("The entry has not been modified");
    assert_eq!(view.expires_at, TTL);
}

// ── 11. Lazy expiration: get only reports the expired flag and does not delete the entry ──

#[test]
fn expiry_is_lazy_in_view_and_table_len() {
    let (mut table, clock) = table_at(0);
    table
        .acquire(KEY, "h1", "r1", 1_000)
        .expect("First time acquisition");

    clock.set_ms(5_000); // 远超 expiresAt
    let view = table
        .get(KEY)
        .expect("Expired entries are still in the table");
    assert!(view.expired);
    assert_eq!(view.owner_id, "h1");
    assert_eq!(table.len(), 1);
    assert!(!table.is_empty());

    // After steal, the entry is overwritten by takeover; it will be removed only after release.
    table
        .steal_if_expired(KEY, "h2", "r2", 1_000)
        .expect("Expired and available for grabs");
    assert!(!table.get(KEY).expect("still after taking over").expired);
    assert!(table.release(KEY, "h2", "r2"));
    assert!(table.is_empty());
}
