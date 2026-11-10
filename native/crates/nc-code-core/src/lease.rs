//! owner/lease table: routing level owner exclusive + run token stale-run protection.
//!
//! TS reference: lease table of `packages/desktop/src/main/taskRealtimeBus.ts`
//! (key combination, same as owner+run refresh retained acquiredAt, any owner/run does not match →
//! `owned_by_other_host`, release does not match silent no-op, streamOp is verified by run).
//! **This TS source does not have TTL/steal** - TTL, steal-if-expired, renew error classification is
//! `specs/rust-native-crates.md` S3 acceptance is added (AGENTS.md "owner/lease is responsible for routing +
//! Retain stale run protection"), see the crate README for design choices.
//!
//! Clock injection ([`Clock`]): all expiration determinations go to `now_ms`, test with [`ManualClock`] deterministic
//! Advance, zero wall clock sleep. Ownership: form owner (`&mut self`, consistent with TS bus single thread),
//! Cross-thread sharing is synchronized by the caller's outer layer.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

/// Injection clock: the only time source for admittedAt and lease expiration determination.
pub trait Clock {
    fn now_ms(&self) -> i64;
}

/// Test clock: explicit set/advance, deterministic trigger expiration.
#[derive(Debug, Clone)]
pub struct ManualClock {
    inner: Arc<AtomicI64>,
}

impl ManualClock {
    pub fn new(now_ms: i64) -> Self {
        Self {
            inner: Arc::new(AtomicI64::new(now_ms)),
        }
    }

    pub fn set_ms(&self, now_ms: i64) {
        self.inner.store(now_ms, Ordering::SeqCst);
    }

    pub fn advance_ms(&self, delta_ms: i64) {
        self.inner.fetch_add(delta_ms, Ordering::SeqCst);
    }

    pub fn now(&self) -> i64 {
        self.inner.load(Ordering::SeqCst)
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.now()
    }
}

/// Production clock (host default).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }
}

/// TS `taskRealtimeBus.getLeaseKey`：`${workspaceKey}\0${taskId}`。
pub fn lease_key(workspace_key: &str, task_id: &str) -> String {
    format!("{workspace_key}\0{task_id}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseGrantState {
    /// New acquisition (including reconstruction with owner+run after expiration).
    Acquired,
    /// Refresh that has not expired and owner+run remains unchanged (mirror TS retains the re-set of acquiredAt).
    Renewed,
    /// Take over the expired lease (original owner/run or expired entries held by others).
    Stolen,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseGrant {
    pub owner_id: String,
    pub run_id: String,
    pub acquired_at: i64,
    pub updated_at: i64,
    pub expires_at: i64,
    pub state: LeaseGrantState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseView {
    pub owner_id: String,
    pub run_id: String,
    pub acquired_at: i64,
    pub updated_at: i64,
    pub expires_at: i64,
    pub expired: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireError {
    /// The lease is still valid and owner/run does not match (TS `owned_by_other_host`,
    /// Contains the same owner as run - the new run cannot preempt when the old run is not released).
    OwnedByOther {
        owner_id: String,
        run_id: String,
    },
    InvalidTtl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenewError {
    NotFound,
    /// Expired: must acquire/steal again, renew does not revive the lease.
    Expired,
    NotOwner {
        owner_id: String,
        run_id: String,
    },
    /// Same as owner but run token has changed (old run call after steal).
    StaleRunToken {
        run_id: String,
    },
    InvalidTtl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StealError {
    NotFound,
    /// Not expired, cannot be snatched.
    Held {
        owner_id: String,
        run_id: String,
    },
    InvalidTtl,
}

/// stale-run protection decision error (called before frame routing/lease renewal/release).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseCheckError {
    NotFound,
    Expired,
    NotOwner { owner_id: String, run_id: String },
    StaleRunToken { run_id: String },
}

impl fmt::Display for AcquireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AcquireError::OwnedByOther { owner_id, run_id } => {
                write!(f, "owned_by_other_host: owner={owner_id} run={run_id}")
            }
            AcquireError::InvalidTtl => write!(f, "invalid ttl: must be > 0"),
        }
    }
}

impl fmt::Display for RenewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenewError::NotFound => write!(f, "lease not found"),
            RenewError::Expired => write!(f, "lease expired: re-acquire required"),
            RenewError::NotOwner { owner_id, run_id } => {
                write!(f, "not lease owner: owner={owner_id} run={run_id}")
            }
            RenewError::StaleRunToken { run_id } => {
                write!(f, "stale run token: current run={run_id}")
            }
            RenewError::InvalidTtl => write!(f, "invalid ttl: must be > 0"),
        }
    }
}

impl fmt::Display for StealError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StealError::NotFound => write!(f, "lease not found: nothing to steal"),
            StealError::Held { owner_id, run_id } => {
                write!(f, "lease still held: owner={owner_id} run={run_id}")
            }
            StealError::InvalidTtl => write!(f, "invalid ttl: must be > 0"),
        }
    }
}

impl fmt::Display for LeaseCheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LeaseCheckError::NotFound => write!(f, "no lease for key"),
            LeaseCheckError::Expired => write!(f, "lease expired"),
            LeaseCheckError::NotOwner { owner_id, run_id } => {
                write!(f, "not lease owner: owner={owner_id} run={run_id}")
            }
            LeaseCheckError::StaleRunToken { run_id } => {
                write!(f, "stale run token: current run={run_id}")
            }
        }
    }
}

impl std::error::Error for AcquireError {}
impl std::error::Error for RenewError {}
impl std::error::Error for StealError {}
impl std::error::Error for LeaseCheckError {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LeaseRecord {
    owner_id: String,
    run_id: String,
    acquired_at: i64,
    updated_at: i64,
    expires_at: i64,
}

impl LeaseRecord {
    fn fresh(now_ms: i64, ttl_ms: i64, owner_id: &str, run_id: &str) -> Self {
        Self {
            owner_id: owner_id.to_owned(),
            run_id: run_id.to_owned(),
            acquired_at: now_ms,
            updated_at: now_ms,
            expires_at: now_ms + ttl_ms,
        }
    }

    fn expired_at(&self, now_ms: i64) -> bool {
        // The boundary is >=: it will fail when it reaches the point (fail closed).
        now_ms >= self.expires_at
    }
}

/// owner/lease table: single owner (`&mut self`).
#[derive(Debug)]
pub struct OwnerLeaseTable<C: Clock> {
    entries: HashMap<String, LeaseRecord>,
    clock: C,
}

impl<C: Clock> OwnerLeaseTable<C> {
    pub fn new(clock: C) -> Self {
        Self {
            entries: HashMap::new(),
            clock,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get/refresh the lease.
    /// - no entries → [`LeaseGrantState::Acquired`];
    /// - not expired and same owner+run → [`LeaseGrantState::Renewed`] (retained acquiredAt,
    /// Mirror TS re-set);
    /// - Not expired and either owner or run are different → [`AcquireError::OwnedByOther`]
    ///   （TS `owned_by_other_host`）；
    /// - Expired → Takeover: Same as owner+run as rebuild (`Acquired`), otherwise `Stolen`.
    pub fn acquire(
        &mut self,
        workspace_key: &str,
        owner_id: &str,
        run_id: &str,
        ttl_ms: i64,
    ) -> Result<LeaseGrant, AcquireError> {
        if ttl_ms <= 0 {
            return Err(AcquireError::InvalidTtl);
        }
        let now = self.clock.now_ms();
        match self.entries.get_mut(workspace_key) {
            None => {
                let record = LeaseRecord::fresh(now, ttl_ms, owner_id, run_id);
                self.entries
                    .insert(workspace_key.to_owned(), record.clone());
                Ok(grant_of(&record, LeaseGrantState::Acquired))
            }
            Some(record)
                if !record.expired_at(now)
                    && record.owner_id == owner_id
                    && record.run_id == run_id =>
            {
                record.updated_at = now;
                record.expires_at = now + ttl_ms;
                Ok(grant_of(record, LeaseGrantState::Renewed))
            }
            Some(record) if record.expired_at(now) => {
                let state = if record.owner_id == owner_id && record.run_id == run_id {
                    LeaseGrantState::Acquired
                } else {
                    LeaseGrantState::Stolen
                };
                // Expired leases are considered invalid. Epoch: acquired_at is reset to zero and recalculated.
                *record = LeaseRecord::fresh(now, ttl_ms, owner_id, run_id);
                Ok(grant_of(record, state))
            }
            Some(record) => Err(AcquireError::OwnedByOther {
                owner_id: record.owner_id.clone(),
                run_id: record.run_id.clone(),
            }),
        }
    }

    /// Lease renewal: it must not have expired and owner+run must fully match; it will not be resurrected after expiration (must be acquired again).
    pub fn renew(
        &mut self,
        workspace_key: &str,
        owner_id: &str,
        run_id: &str,
        ttl_ms: i64,
    ) -> Result<LeaseGrant, RenewError> {
        if ttl_ms <= 0 {
            return Err(RenewError::InvalidTtl);
        }
        let now = self.clock.now_ms();
        let record = self
            .entries
            .get_mut(workspace_key)
            .ok_or(RenewError::NotFound)?;
        if record.expired_at(now) {
            return Err(RenewError::Expired);
        }
        if record.owner_id != owner_id {
            return Err(RenewError::NotOwner {
                owner_id: record.owner_id.clone(),
                run_id: record.run_id.clone(),
            });
        }
        if record.run_id != run_id {
            return Err(RenewError::StaleRunToken {
                run_id: record.run_id.clone(),
            });
        }
        record.updated_at = now;
        record.expires_at = now + ttl_ms;
        Ok(grant_of(record, LeaseGrantState::Renewed))
    }

    /// Release: Delete only if owner+run matches; otherwise silent no-op (mirror TS `releaseLease` returns void).
    pub fn release(&mut self, workspace_key: &str, owner_id: &str, run_id: &str) -> bool {
        let matches = self
            .entries
            .get(workspace_key)
            .is_some_and(|r| r.owner_id == owner_id && r.run_id == run_id);
        if matches {
            self.entries.remove(workspace_key);
        }
        matches
    }

    /// Only take over if the lease has expired; reject if it has not expired (not to be mixed with acquire's implicit takeover).
    pub fn steal_if_expired(
        &mut self,
        workspace_key: &str,
        owner_id: &str,
        run_id: &str,
        ttl_ms: i64,
    ) -> Result<LeaseGrant, StealError> {
        if ttl_ms <= 0 {
            return Err(StealError::InvalidTtl);
        }
        let now = self.clock.now_ms();
        let record = self
            .entries
            .get_mut(workspace_key)
            .ok_or(StealError::NotFound)?;
        if !record.expired_at(now) {
            return Err(StealError::Held {
                owner_id: record.owner_id.clone(),
                run_id: record.run_id.clone(),
            });
        }
        *record = LeaseRecord::fresh(now, ttl_ms, owner_id, run_id);
        Ok(grant_of(record, LeaseGrantState::Stolen))
    }

    /// stale-run protection: Verify that (owner, run) is still the current owner before routing/frame publishing.
    /// Expiration means fail closed - the holder must renew within TTL.
    pub fn assert_current(
        &self,
        workspace_key: &str,
        owner_id: &str,
        run_id: &str,
    ) -> Result<(), LeaseCheckError> {
        let now = self.clock.now_ms();
        let record = self
            .entries
            .get(workspace_key)
            .ok_or(LeaseCheckError::NotFound)?;
        if record.expired_at(now) {
            return Err(LeaseCheckError::Expired);
        }
        if record.owner_id != owner_id {
            return Err(LeaseCheckError::NotOwner {
                owner_id: record.owner_id.clone(),
                run_id: record.run_id.clone(),
            });
        }
        if record.run_id != run_id {
            return Err(LeaseCheckError::StaleRunToken {
                run_id: record.run_id.clone(),
            });
        }
        Ok(())
    }

    pub fn get(&self, workspace_key: &str) -> Option<LeaseView> {
        let now = self.clock.now_ms();
        self.entries.get(workspace_key).map(|record| LeaseView {
            owner_id: record.owner_id.clone(),
            run_id: record.run_id.clone(),
            acquired_at: record.acquired_at,
            updated_at: record.updated_at,
            expires_at: record.expires_at,
            expired: record.expired_at(now),
        })
    }
}

fn grant_of(record: &LeaseRecord, state: LeaseGrantState) -> LeaseGrant {
    LeaseGrant {
        owner_id: record.owner_id.clone(),
        run_id: record.run_id.clone(),
        acquired_at: record.acquired_at,
        updated_at: record.updated_at,
        expires_at: record.expires_at,
        state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// The manual clock is the lease tests' single time source; advancing it must
    /// change the observed value (otherwise expiry tests are silently time-independent).
    #[test]
    fn manual_clock_advances_the_observed_time() {
        let clock = ManualClock::new(1000);
        assert_eq!(clock.now(), 1000);
        clock.advance_ms(500);
        assert_eq!(clock.now(), 1500);
        clock.set_ms(42);
        assert_eq!(clock.now(), 42);
    }
}
