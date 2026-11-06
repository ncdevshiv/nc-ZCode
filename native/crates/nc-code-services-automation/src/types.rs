//! Scheduled task domain types (Rust equivalent of TS `packages/shared/src/automation-types.ts`).
//!
//! The field type is camelCase (same origin as TS cross-end reuse); the sqlite column name is snake_case, mapped by the repo layer.
//! Only type and pure verification/parsing are put here, and IO is not touched.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The total number of scheduled task definitions allowed to be retained by a single local task index (all life cycle states are counted).
pub const AUTOMATION_CREATE_LIMIT: usize = 20;
/// Overrun error code, retained in message across RPC for UI identification.
pub const AUTOMATION_CREATE_LIMIT_ERROR_CODE: &str = "AUTOMATION_CREATE_LIMIT_REACHED";

/// The entire automation life cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LifecycleStatus {
    Active,
    Completed,
    Failed,
    Paused,
}

/// The dispatch information status of the current round of single automation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DispatchStatus {
    Idle,
    Claimed,
    Dispatched,
    FailedToDispatch,
}

/// Trigger source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Trigger {
    Schedule,
    Manual,
}

/// The dispatch result of a single run (scheduler authority). skipped=missed the trigger window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunDispatchStatus {
    Claimed,
    Dispatched,
    FailedToDispatch,
    Skipped,
}

/// A single run produces the running result after session (written back by session runtime).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunOutcome {
    Running,
    Succeeded,
    Failed,
    Stopped,
}

/// Repeating unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScheduleUnit {
    Minute,
    Hourly,
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

/// Monthly rule mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MonthlyMode {
    Date,
    Weekday,
}

/// Customize repetition rules; cronExpr is reserved for compatible display, and scheduling uses this field as the authority. All times are in local calendar milliseconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleRule {
    pub unit: ScheduleUnit,
    pub interval: u32,
    pub hour: u32,
    pub minute: u32,
    /// Anchor point (Unix milliseconds): minute/hourly intervals advance from here and do not follow the wall clock tick.
    pub anchor_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekdays: Option<Vec<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub month_days: Option<Vec<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub months: Option<Vec<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monthly_mode: Option<MonthlyMode>,
}

/// Model selection intent (providerId + modelId + optional structured options).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelection {
    pub provider_id: String,
    pub model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Value>,
}

/// A scheduled task definition + scheduling status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Automation {
    pub automation_id: String,
    pub title: String,
    pub cron_expr: String,
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_selection: Option<ModelSelection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    pub workspace_key: String,
    pub workspace_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_identity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_task_id: Option<String>,
    /// This issue is always local.
    pub location_kind: String,
    /// true=infinite loop; false=limited times (with max_runs).
    pub recurring: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_runs: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule_rule: Option<ScheduleRule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule_edited_by_user: Option<bool>,
    pub run_count: u32,
    pub enabled: bool,
    pub lifecycle_status: LifecycleStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<i64>,
    pub dispatch_status: DispatchStatus,
    pub dispatch_attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A triggered run history / runId idempotent ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationRun {
    pub run_id: String,
    pub automation_id: String,
    pub workspace_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheduled_at: Option<i64>,
    pub trigger: Trigger,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_selection: Option<ModelSelection>,
    pub dispatch_status: RunDispatchStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RunOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub attempts: u32,
    pub created_at: i64,
    pub updated_at: i64,
}

/// The parsing result of parse_run_id; scheduled_at only exists for schedule trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunIdParts {
    pub automation_id: String,
    pub trigger: Trigger,
    pub scheduled_at: Option<i64>,
}

/// runId is the only parsing entry of the contract (TS `parseAutomationRunId`). Illegal input will always be None.
pub fn parse_run_id(run_id: &str) -> Option<RunIdParts> {
    let separator = run_id.find(':')?;
    if separator == 0 {
        return None;
    }
    let automation_id = &run_id[..separator];
    let rest = &run_id[separator + 1..];
    const MANUAL_PREFIX: &str = "manual:";
    if let Some(suffix) = rest.strip_prefix(MANUAL_PREFIX) {
        return if suffix.is_empty() {
            None
        } else {
            Some(RunIdParts {
                automation_id: automation_id.to_string(),
                trigger: Trigger::Manual,
                scheduled_at: None,
            })
        };
    }
    let scheduled_at: i64 = rest.parse().ok()?;
    if scheduled_at <= 0 {
        return None;
    }
    Some(RunIdParts {
        automation_id: automation_id.to_string(),
        trigger: Trigger::Schedule,
        scheduled_at: Some(scheduled_at),
    })
}

/// Overrun error: The error code is included in the message for UI identification (retained in text across RPCs).
pub fn create_limit_error() -> String {
    format!(
        "[{AUTOMATION_CREATE_LIMIT_ERROR_CODE}] At most {AUTOMATION_CREATE_LIMIT} automations may be retained. Delete an existing automation before creating another."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_run_id_accepts_both_declared_formats() {
        let scheduled = parse_run_id("auto-1:1730000000000").expect("schedule format");
        assert_eq!(scheduled.automation_id, "auto-1");
        assert_eq!(scheduled.trigger, Trigger::Schedule);
        assert_eq!(scheduled.scheduled_at, Some(1_730_000_000_000));
        let manual = parse_run_id("auto-1:manual:abc-123").expect("manual format");
        assert_eq!(manual.automation_id, "auto-1");
        assert_eq!(manual.trigger, Trigger::Manual);
        assert!(manual.scheduled_at.is_none());
    }

    #[test]
    fn parse_run_id_rejects_malformed_input() {
        assert!(parse_run_id("nocolon").is_none());
        assert!(parse_run_id(":123").is_none());
        assert!(parse_run_id("a:manual:").is_none());
        assert!(parse_run_id("a:notanumber").is_none());
        assert!(parse_run_id("a:0").is_none());
        assert!(parse_run_id("a:-5").is_none());
    }

    #[test]
    fn schedule_rule_serializes_camel_case_like_ts() {
        let rule = ScheduleRule {
            unit: ScheduleUnit::Weekly,
            interval: 2,
            hour: 9,
            minute: 30,
            anchor_at: 1_730_000_000_000,
            weekdays: Some(vec![1, 3]),
            month_days: None,
            months: None,
            monthly_mode: None,
        };
        let value = serde_json::to_value(&rule).expect("serialize");
        assert_eq!(value["unit"], "weekly");
        assert_eq!(value["anchorAt"], 1_730_000_000_000i64);
        assert_eq!(value["weekdays"], serde_json::json!([1, 3]));
        assert!(value.get("monthDays").is_none());
        let back: ScheduleRule = serde_json::from_value(value).expect("deserialize");
        assert_eq!(back, rule);
    }

    #[test]
    fn create_limit_matches_ts() {
        assert_eq!(AUTOMATION_CREATE_LIMIT, 20);
        assert_eq!(
            AUTOMATION_CREATE_LIMIT_ERROR_CODE,
            "AUTOMATION_CREATE_LIMIT_REACHED"
        );
        assert!(create_limit_error().contains("AUTOMATION_CREATE_LIMIT_REACHED"));
    }
}
