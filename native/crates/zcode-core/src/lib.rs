//! zcode-core：S3 核心原语（specs/rust-native-crates.md crate map）。
//!
//! - [`inbox`]：`command-inbox.ts` 串行 admission 镜像（单运行、单调 admissionSeq、
//!   FIFO 排队、幂等重提、stale-settle 防护）。
//! - [`lease`]：owner/lease 表（acquire/renew/release/steal-if-expired + run token
//!   stale 防护），注入时钟，测试零 sleep。
//! - [`identity`]：AGENTS.md `workspaceIdentity?.trim() || workspacePath` 身份 key，
//!   trim 行为镜像 JS（含 NBSP/ZWNBSP/NEL 边界）。
//!
//! 所有权与事件面详见 crate 根 `README.md`。

pub mod identity;
pub mod inbox;
pub mod lease;
pub mod protocol;

pub use identity::{identity_key, js_trim};
pub use inbox::{
    queue_item_id_for_command, Admitted, CancelOutcome, CommandInbox, Outcome, QueryItem,
    QueryOutcome, SettleError, SettleOutcome, SettleTicket, SharedCommandInbox, WaiterStats,
};
pub use lease::{
    lease_key, AcquireError, Clock, LeaseCheckError, LeaseGrant, LeaseGrantState, LeaseView,
    ManualClock, OwnerLeaseTable, RenewError, StealError, SystemClock,
};
pub use protocol::{
    CommandAck, CommandEnvelope, CommandFinal, CommandInboxHost, CommandKey, CommandStatus,
    GuardDecision, ParseEnvelopeError, GLOBAL_BUCKET, IDEMPOTENCY_TABLE_PER_SESSION,
    REASON_ADMISSION_CANCELLED, REASON_INVALID_PAYLOAD, REASON_MISSING_BASE_REVISION,
    REASON_SESSION_NOT_FOUND, REASON_STALE_LOG_EPOCH, REASON_STALE_REVISION,
};
