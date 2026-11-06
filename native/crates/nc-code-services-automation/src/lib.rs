//! nc-code-services-automation - scheduled task (Automation) service (TS `packages/services/src/session/
//! automation{Repo,Service,Cron}.ts` + Rust port of `automationIntervalCarrier.ts`).
//!
//! Layering is consistent with TS:
//! - [`types`]: Domain type (camelCase, homologous to TS cross-end reuse);
//! - [`cron`]: cron expression parsing + next trigger calculation + custom scheduleRule date engine
//!   (replacement of TS's croner library, handwritten 5-segment parser, Vixie OR semantic alignment);
//! - [`repo`]: sqlite repository for automations/automation_runs with claim→dispatch→settle
//!   Scheduling state machine (DB connection and schema reuse nc-code-host::storage::open to ensure cross-language compatibility);
//! - [`service`]: cron semantics (verification/nextRunAt recalculation) + life cycle recalculation + input parameter normalization.
//!
//! The warehouse only performs storage and atomic state migration, and is not aware of cron semantics; the scheduling loop (polling claimDue dispatch) is
//! Host/scheduler driver, this crate provides its persistence API.

pub mod cron;
pub mod repo;
pub mod service;
pub mod types;

pub use cron::{
    compute_automation_next_run_at, compute_initial_next_run_at, compute_schedule_rule_next_run_at,
    is_valid_cron, CronExpr,
};
pub use repo::{
    compute_retry_at, AutomationRepo, CreateOptions, CreateParams, FailKind, UpdateOptions,
    UpdateParams, CLAIM_STALE_MS, DISPATCH_MAX_ATTEMPTS, DISPATCH_RETRY_BASE_MS,
    DISPATCH_RETRY_CAP_MS,
};
pub use service::{AutomationService, CreateInput, Scope, ServiceError, UpdateInput};
pub use types::{
    parse_run_id, Automation, AutomationRun, DispatchStatus, LifecycleStatus, ModelSelection,
    MonthlyMode, RunDispatchStatus, RunOutcome, ScheduleRule, ScheduleUnit, Trigger,
    AUTOMATION_CREATE_LIMIT, AUTOMATION_CREATE_LIMIT_ERROR_CODE,
};
