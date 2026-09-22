//! owner/lease 表：路由级 owner 独占 + run token stale-run 防护。
//!
//! TS 参照：`packages/desktop/src/main/taskRealtimeBus.ts` 的 lease 表
//! （key 组合、同 owner+run 刷新保留 acquiredAt、任何 owner/run 不匹配 →
//! `owned_by_other_host`、release 不匹配静默 no-op、streamOp 按 run 校验）。
//! **该 TS 源没有 TTL/steal**——TTL、steal-if-expired、renew 错误分类是
//! `specs/rust-native-crates.md` S3 验收新增（AGENTS.md「owner/lease 负责路由 +
//! 保留 stale run 防护」），设计取舍见 crate README。
//!
//! 时钟注入（[`Clock`]）：过期判定全部走 `now_ms`，测试用 [`ManualClock`] 确定性
//! 推进，零墙钟 sleep。所有权：表格单所有者（`&mut self`，与 TS bus 单线程一致），
//! 跨线程共享由调用方外层同步。

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

/// 注入时钟：admittedAt 与租约过期判定的唯一时间来源。
pub trait Clock {
    fn now_ms(&self) -> i64;
}

/// 测试时钟：显式 set/advance，确定性触发过期。
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

/// 生产时钟（宿主默认）。
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
    /// 全新获取（含过期后同 owner+run 重建）。
    Acquired,
    /// 未过期且 owner+run 不变的刷新（镜 TS 保留 acquiredAt 的 re-set）。
    Renewed,
    /// 接管已过期的租约（原 owner/run 或已被他人持有过期条目）。
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
    /// 租约仍有效且 owner/run 不匹配（TS `owned_by_other_host`，
    /// 含同 owner 换 run——旧 run 未释放时新 run 不得抢占）。
    OwnedByOther {
        owner_id: String,
        run_id: String,
    },
    InvalidTtl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenewError {
    NotFound,
    /// 已过期：必须重新 acquire/steal，renew 不复活租约。
    Expired,
    NotOwner {
        owner_id: String,
        run_id: String,
    },
    /// 同 owner 但 run token 已变（steal 后旧 run 的调用）。
    StaleRunToken {
        run_id: String,
    },
    InvalidTtl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StealError {
    NotFound,
    /// 未过期，不可抢。
    Held {
        owner_id: String,
        run_id: String,
    },
    InvalidTtl,
}

/// stale-run 防护裁决错误（帧路由/续租/释放前调用）。
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
        // 边界取 >=：到点即失效（fail closed）。
        now_ms >= self.expires_at
    }
}

/// owner/lease 表：单所有者（`&mut self`）。
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

    /// 获取/刷新租约。
    /// - 无条目 → [`LeaseGrantState::Acquired`]；
    /// - 未过期且 owner+run 相同 → [`LeaseGrantState::Renewed`]（保留 acquiredAt，
    ///   镜 TS re-set）；
    /// - 未过期且 owner 或 run 任一不同 → [`AcquireError::OwnedByOther`]
    ///   （TS `owned_by_other_host`）；
    /// - 已过期 → 接管：同 owner+run 视为重建（`Acquired`），否则 `Stolen`。
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
                // 过期租约视为失效 epoch：acquired_at 归零重计。
                *record = LeaseRecord::fresh(now, ttl_ms, owner_id, run_id);
                Ok(grant_of(record, state))
            }
            Some(record) => Err(AcquireError::OwnedByOther {
                owner_id: record.owner_id.clone(),
                run_id: record.run_id.clone(),
            }),
        }
    }

    /// 续租：必须未过期且 owner+run 全匹配；过期不复活（须重新 acquire）。
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

    /// 释放：owner+run 匹配才删除；否则静默 no-op（镜 TS `releaseLease` 返回 void）。
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

    /// 仅当租约已过期时接管；未过期拒绝（不与 acquire 的隐式接管混用）。
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

    /// stale-run 防护：路由/帧发布前校验 (owner, run) 仍是当前持有者。
    /// 过期即 fail closed——持有者必须在 TTL 内 renew。
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
