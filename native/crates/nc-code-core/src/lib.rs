//! nc-code-core: S3 core primitives (specs/rust-native-crates.md crate map).
//!
//! - [`inbox`]: `command-inbox.ts` Serial admission image (single run, monotonic admissionSeq,
//! FIFO queuing, idempotent retrieval, stale-settle protection).
//! - [`lease`]: owner/lease table (acquire/renew/release/steal-if-expired + run token
//! stale protection), inject clock, test zero sleep.
//! - [`identity`]: AGENTS.md `workspaceIdentity?.trim() || workspacePath` identity key,
//! trim behavior mirrors JS (with NBSP/ZWNBSP/NEL boundaries).
//!
//! See the crate root `README.md` for details on ownership and events.

#![allow(clippy::doc_lazy_continuation)]

pub mod identity;
pub mod inbox;
pub mod lease;
pub mod permission;
pub mod protocol;

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {
        assert!(true);
    }
}

pub use identity::{identity_key, js_trim};
pub use inbox::{
    queue_item_id_for_command, Admitted, CancelOutcome, CommandInbox, Outcome, QueryItem,
    QueryOutcome, SettleError, SettleOutcome, SettleTicket, SharedCommandInbox, WaiterStats,
};
pub use lease::{
    lease_key, AcquireError, Clock, LeaseCheckError, LeaseGrant, LeaseGrantState, LeaseView,
    ManualClock, OwnerLeaseTable, RenewError, StealError, SystemClock,
};
pub use permission::{
    find_matching_rule, matches_rule, matches_rule_content, resolve_plan_mode_transition,
    rule_subjects, webfetch_domain_subject, wildcard_to_regex, CollaborationMode,
    PermissionBehavior, PermissionConfig, PermissionDecisionResult, PermissionRuleValue,
    PermissionRuleset, RiskLevel, SideEffectScope,
};
pub use protocol::{
    CommandAck, CommandEnvelope, CommandFinal, CommandInboxHost, CommandKey, CommandStatus,
    GuardDecision, ParseEnvelopeError, GLOBAL_BUCKET, IDEMPOTENCY_TABLE_PER_SESSION,
    REASON_ADMISSION_CANCELLED, REASON_INVALID_PAYLOAD, REASON_MISSING_BASE_REVISION,
    REASON_SESSION_NOT_FOUND, REASON_STALE_LOG_EPOCH, REASON_STALE_REVISION,
};
