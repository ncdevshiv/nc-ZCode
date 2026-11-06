//! automation management service: package AutomationRepo, responsible for cron semantics (verification/next time calculation) +
//! Life cycle recalculation + input parameter normalization (relative delay / interval carrier). The warehouse is only for storage; it is hosted here
//! "Calculate next_run_at when create" "Change cron_expr and recalculate" "Change recurring/max_runs and recalculate the life cycle"
//! "restart recalculation" and other cron related services (automationService.ts:136-142, same note).

use std::sync::Arc;

use crate::cron::{
    build_interval_schedule_rule, build_relative_delay_schedule, compute_automation_next_run_at,
    compute_initial_next_run_at, infer_minute_interval_schedule_rule, is_valid_cron,
    schedule_rule_definition,
};
use crate::repo::{AutomationRepo, CreateOptions, CreateParams, UpdateOptions, UpdateParams};
use crate::types::{
    Automation, AutomationRun, LifecycleStatus, ModelSelection, ScheduleRule, ScheduleUnit,
};

const MAX_SESSION_INTERVAL: u32 = 200;
const MAX_RELATIVE_DELAY_MINUTES: u32 = 525_600;
const MAX_MONTHLY_INTERVAL: u32 = 1_200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceError {
    InvalidCron(String),
    InvalidRule(String),
    InvalidRelativeDelay(String),
    InvalidCarrier(String),
    InvalidMaxRunsUpdate(String),
    StaleOneShot(String),
    Repo(String),
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCron(expr) => write!(f, "Illegal cron expression: {expr}"),
            Self::InvalidRule(m) => write!(f, "Illegal scheduled task scheduling rule: {m}"),
            Self::InvalidRelativeDelay(m) => write!(f, "Illegal relative time scheduled task: {m}"),
            Self::InvalidCarrier(m) => write!(f, "Illegal custom repeated input parameter: {m}"),
            Self::InvalidMaxRunsUpdate(m) | Self::StaleOneShot(m) | Self::Repo(m) => {
                write!(f, "{m}")
            }
        }
    }
}

impl std::error::Error for ServiceError {}

impl From<String> for ServiceError {
    fn from(message: String) -> Self {
        Self::Repo(message)
    }
}

/// Create input parameters.
#[derive(Debug, Clone, Default)]
pub struct CreateInput {
    pub title: String,
    pub cron_expr: String,
    pub relative_delay_minutes: Option<u32>,
    pub prompt: String,
    pub model_selection: Option<ModelSelection>,
    pub mode: Option<String>,
    pub workspace_path: String,
    pub workspace_identity: Option<String>,
    pub target_task_id: Option<String>,
    pub recurring: bool,
    pub max_runs: Option<u32>,
    pub end_at: Option<i64>,
    pub schedule_rule: Option<ScheduleRule>,
    pub interval_unit: Option<ScheduleUnit>,
    pub interval: Option<u32>,
}

/// Edit input parameters.
#[derive(Debug, Clone, Default)]
pub struct UpdateInput {
    pub title: Option<String>,
    pub cron_expr: Option<String>,
    pub prompt: Option<String>,
    pub model_selection: Option<Option<ModelSelection>>,
    pub mode: Option<Option<String>>,
    pub recurring: Option<bool>,
    pub max_runs: Option<Option<u32>>,
    pub end_at: Option<Option<i64>>,
    pub schedule_rule: Option<Option<ScheduleRule>>,
    pub interval_unit: Option<ScheduleUnit>,
    pub interval: Option<u32>,
    pub schedule_edited_by_user: Option<bool>,
}

/// workspace write/single query attribution filtering.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    pub workspace_path: Option<String>,
    pub workspace_identity: Option<String>,
}

/// automation management service.
pub struct AutomationService {
    repo: Arc<AutomationRepo>,
}

impl AutomationService {
    pub fn new(repo: Arc<AutomationRepo>) -> Self {
        Self { repo }
    }

    pub fn repo(&self) -> &Arc<AutomationRepo> {
        &self.repo
    }

    fn scope_key(scope: &Scope) -> Option<String> {
        let path = scope.workspace_path.as_deref()?.trim();
        if path.is_empty() {
            return None;
        } // workspaceIdentity?.trim() || workspacePath (AGENTS.md identity key caliber).
        let identity = scope
            .workspace_identity
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty());
        Some(identity.unwrap_or(path).to_string())
    }

    pub fn create(&self, input: &CreateInput, scope: &Scope) -> Result<Automation, ServiceError> {
        let created_at = now_ms();
        validate_relative_delay(input)?;
        validate_interval_carrier(input)?;

        // relative delay: anchored by the real clock of the server, and the absolute cron converted by the model is not trusted.
        let mut cron_expr = input.cron_expr.clone();
        let mut schedule_rule = input.schedule_rule.clone();
        let mut recurring = input.recurring;
        let mut max_runs = input.max_runs;
        if let Some(delay) = input.relative_delay_minutes {
            let (cron, rule) = build_relative_delay_schedule(delay, created_at)
                .ok_or_else(|| ServiceError::Repo("Time conversion failed".to_string()))?;
            cron_expr = cron;
            schedule_rule = Some(rule);
            recurring = false;
            max_runs = None;
        }
        // interval carrier normalization: the real interval is carried by scheduleRule, cronExpr is only displayed.
        if let (Some(unit), Some(interval)) = (input.interval_unit, input.interval) {
            let rule = build_interval_schedule_rule(unit, interval, &cron_expr, created_at)
                .ok_or_else(|| ServiceError::Repo("carrier normalization failed".to_string()))?;
            schedule_rule = Some(rule);
            recurring = true;
            max_runs = None;
        }
        if !is_valid_cron(&cron_expr) {
            return Err(ServiceError::InvalidCron(cron_expr));
        }
        if let Some(rule) = &schedule_rule {
            validate_schedule_rule(rule)?;
        } // `*/N` minutes cron supplements the creation time anchor point (to avoid aligning the wall clock from triggering in advance); if there are explicit rules, it will be anchored to the creation time.
        let schedule_rule = schedule_rule
            .map(|mut rule| {
                rule.anchor_at = created_at;
                rule
            })
            .or_else(|| infer_minute_interval_schedule_rule(&cron_expr, created_at));
        let next_run_at = compute_initial_next_run_at(
            &cron_expr,
            recurring,
            max_runs,
            schedule_rule.as_ref(),
            created_at,
        )
        .map_err(ServiceError::StaleOneShot)?;
        let ended_before_first = input
            .end_at
            .is_some_and(|end| next_run_at.is_none_or(|next| next > end));
        let params = CreateParams {
            title: input.title.clone(),
            cron_expr,
            prompt: input.prompt.clone(),
            model_selection: input.model_selection.clone(),
            mode: input.mode.clone(),
            workspace_path: input.workspace_path.clone(),
            workspace_identity: input.workspace_identity.clone(),
            target_task_id: input.target_task_id.clone(),
            recurring,
            max_runs,
            end_at: input.end_at,
            schedule_rule,
        };
        let options = CreateOptions {
            next_run_at: if ended_before_first {
                None
            } else {
                next_run_at
            },
            lifecycle_status: ended_before_first.then_some(LifecycleStatus::Completed),
        };
        let key = Self::scope_key(scope)
            .ok_or_else(|| ServiceError::Repo("create requires workspacePath".to_string()))?;
        self.repo
            .create(&params, &key, &options)
            .map_err(Into::into)
    }

    pub fn list(&self, scope: &Scope) -> Result<Vec<Automation>, ServiceError> {
        self.repo
            .list(Self::scope_key(scope).as_deref())
            .map_err(Into::into)
    }

    pub fn get(
        &self,
        automation_id: &str,
        scope: &Scope,
    ) -> Result<Option<Automation>, ServiceError> {
        self.repo
            .get(automation_id, Self::scope_key(scope).as_deref())
            .map_err(Into::into)
    }

    pub fn delete(&self, automation_id: &str, scope: &Scope) -> Result<bool, ServiceError> {
        self.repo
            .delete(automation_id, Self::scope_key(scope).as_deref())
            .map_err(Into::into)
    }

    pub fn set_enabled(
        &self,
        automation_id: &str,
        enabled: bool,
        scope: &Scope,
    ) -> Result<(), ServiceError> {
        self.repo
            .set_enabled(automation_id, enabled, Self::scope_key(scope).as_deref())
            .map_err(Into::into)
    }

    /// Manual rerun of failed tasks: Only failed is a recoverable exception. Return to active and press cron to recalculate the next time.
    pub fn restart(&self, automation_id: &str, scope: &Scope) -> Result<(), ServiceError> {
        let key = Self::scope_key(scope);
        let Some(existing) = self
            .repo
            .get(automation_id, key.as_deref())
            .map_err(ServiceError::Repo)?
        else {
            return Ok(());
        };
        // completed is the final state of the finite plan that is naturally exhausted and cannot be revived by restart.
        if existing.lifecycle_status != LifecycleStatus::Failed {
            return Ok(());
        }
        let next_run_at = compute_automation_next_run_at(
            &existing.cron_expr,
            existing.schedule_rule.as_ref(),
            now_ms(),
        );
        self.repo
            .restart(automation_id, next_run_at, key.as_deref())
            .map_err(Into::into)
    }

    /// Run immediately: Create a manual run for the host to dispatch directly without changing the original cron plan.
    pub fn run_now(
        &self,
        automation_id: &str,
        scope: &Scope,
    ) -> Result<Option<(Automation, AutomationRun)>, ServiceError> {
        self.repo
            .run_now(automation_id, now_ms(), Self::scope_key(scope).as_deref())
            .map_err(Into::into)
    }

    pub fn list_runs(
        &self,
        automation_id: &str,
        scope: &Scope,
    ) -> Result<Vec<AutomationRun>, ServiceError> {
        self.repo
            .list_runs(automation_id, Self::scope_key(scope).as_deref())
            .map_err(Into::into)
    }

    pub fn delete_run(&self, run_id: &str, scope: &Scope) -> Result<(), ServiceError> {
        self.repo
            .delete_run(run_id, Self::scope_key(scope).as_deref())
            .map_err(Into::into)
    }
    /// Edit definition fields: cron/rule verification, carrier normalization, anchor point retention, life cycle recalculation.
    pub fn update(
        &self,
        automation_id: &str,
        input: &UpdateInput,
        scope: &Scope,
    ) -> Result<Option<Automation>, ServiceError> {
        let key = Self::scope_key(scope);
        let Some(existing) = self
            .repo
            .get(automation_id, key.as_deref())
            .map_err(ServiceError::Repo)?
        else {
            return Ok(None);
        };
        if let Some(Some(rule)) = input.schedule_rule.as_ref() {
            validate_schedule_rule(rule)?;
        }
        let direct_rule = input.schedule_rule.clone().flatten();
        validate_update_carrier(input, direct_rule.is_some())?;
        let has_carrier = input.interval_unit.is_some() && input.interval.is_some();
        if !has_carrier && matches!(input.max_runs, Some(None)) && input.recurring != Some(true) {
            return Err(ServiceError::InvalidMaxRunsUpdate(
                "When clearing maxRuns, recurring=true must be set in the same update".to_string(),
            ));
        }
        let next_recurring = if has_carrier {
            true
        } else {
            input.recurring.unwrap_or(existing.recurring)
        };
        if next_recurring && matches!(input.max_runs, Some(Some(_))) {
            return Err(ServiceError::InvalidMaxRunsUpdate(
                "Infinite loop tasks cannot set a limited number of times maxRuns".to_string(),
            ));
        }
        let effective_max_runs = if has_carrier {
            None
        } else if next_recurring
            && input.max_runs.is_none()
            && (input.recurring == Some(true) || existing.max_runs.is_some())
        {
            Some(None)
        } else {
            input.max_runs
        };
        let updated_at = now_ms();
        let cron_changed = input
            .cron_expr
            .as_deref()
            .is_some_and(|c| c != existing.cron_expr);
        if let Some(cron) = input.cron_expr.as_deref() {
            if !is_valid_cron(cron) {
                return Err(ServiceError::InvalidCron(cron.to_string()));
            }
        }
        let effective_cron = input.cron_expr.clone().unwrap_or(existing.cron_expr);
        let carrier_rule = if has_carrier {
            build_interval_schedule_rule(
                input.interval_unit.expect("carrier unit"),
                input.interval.expect("carrier interval"),
                &effective_cron,
                updated_at,
            )
        } else {
            None
        };
        let explicit_rule = direct_rule.or(carrier_rule).map(|mut rule| {
            let unchanged = existing.schedule_rule.as_ref().is_some_and(|old| {
                schedule_rule_definition(old) == schedule_rule_definition(&rule)
            });
            if unchanged {
                rule.anchor_at = existing
                    .schedule_rule
                    .as_ref()
                    .map(|r| r.anchor_at)
                    .unwrap_or(updated_at);
            } else if !has_carrier {
                rule.anchor_at = updated_at;
            }
            rule
        });
        let inferred_rule = if cron_changed && input.schedule_rule.is_none() && !has_carrier {
            infer_minute_interval_schedule_rule(&effective_cron, updated_at)
        } else {
            None
        };
        let effective_rule = match &explicit_rule {
            Some(rule) => Some(rule.clone()),
            None if cron_changed => inferred_rule,
            None => existing.schedule_rule.clone(),
        };
        let mut options = UpdateOptions {
            next_run_at: None,
            lifecycle_status: None,
            reset_retry: false,
        };
        if cron_changed || input.end_at.is_some() || explicit_rule.is_some() {
            options.next_run_at = Some(compute_automation_next_run_at(
                &effective_cron,
                effective_rule.as_ref(),
                updated_at,
            ));
            options.reset_retry = true;
        }
        let effective_end_at = match input.end_at {
            None => existing.end_at,
            Some(v) => v,
        };
        let next_now = options
            .next_run_at
            .flatten()
            .or(existing.next_run_at)
            .unwrap_or(i64::MAX);
        if effective_end_at.is_some_and(|end| next_now > end) {
            options.next_run_at = Some(None);
            options.lifecycle_status = Some(LifecycleStatus::Completed);
        } else if input.end_at.is_some()
            && matches!(
                existing.lifecycle_status,
                LifecycleStatus::Completed | LifecycleStatus::Failed
            )
        {
            options.lifecycle_status = Some(LifecycleStatus::Active);
        }
        if input.recurring.is_some() || input.max_runs.is_some() {
            let next_max = match &effective_max_runs {
                None => existing.max_runs,
                Some(v) => *v,
            };
            let Some(scheduled) = self
                .repo
                .get_scheduled_run_count(automation_id, key.as_deref())
                .map_err(ServiceError::Repo)?
            else {
                return Ok(None);
            };
            let reached_max = !next_recurring && scheduled >= next_max.unwrap_or(1) as i64;
            if reached_max {
                options.lifecycle_status = Some(LifecycleStatus::Completed);
            } else if matches!(
                existing.lifecycle_status,
                LifecycleStatus::Completed | LifecycleStatus::Failed
            ) {
                options.lifecycle_status = Some(LifecycleStatus::Active);
                if options.next_run_at.is_none() {
                    options.next_run_at = Some(compute_automation_next_run_at(
                        &effective_cron,
                        effective_rule.as_ref(),
                        updated_at,
                    ));
                }
            }
        }
        let params = UpdateParams {
            title: input.title.clone(),
            cron_expr: input.cron_expr.clone(),
            prompt: input.prompt.clone(),
            model_selection: input.model_selection.clone(),
            mode: input.mode.clone(),
            recurring: input.recurring,
            max_runs: effective_max_runs,
            end_at: input.end_at,
            schedule_rule: Some(effective_rule),
            schedule_edited_by_user: input.schedule_edited_by_user,
        };
        self.repo
            .update(automation_id, &params, &options, key.as_deref())
            .map_err(Into::into)
    }
}

fn validate_relative_delay(input: &CreateInput) -> Result<(), ServiceError> {
    let Some(delay) = input.relative_delay_minutes else {
        return Ok(());
    };
    if !(1..=MAX_RELATIVE_DELAY_MINUTES).contains(&delay) {
        return Err(ServiceError::InvalidRelativeDelay(format!(
            "delayMinutes must be an integer from 1-{MAX_RELATIVE_DELAY_MINUTES}"
        )));
    }
    if input.recurring {
        return Err(ServiceError::InvalidRelativeDelay(
            "Relative delay tasks must be set recurring=false".to_string(),
        ));
    }
    if input.max_runs.is_some() {
        return Err(ServiceError::InvalidRelativeDelay(
            "The relative delay task is fixed to run only once and maxRuns cannot be set"
                .to_string(),
        ));
    }
    if input.schedule_rule.is_some() {
        return Err(ServiceError::InvalidRelativeDelay(
            "Cannot submit delayMinutes and scheduleRule at the same time".to_string(),
        ));
    }
    Ok(())
}

fn validate_interval_carrier(input: &CreateInput) -> Result<(), ServiceError> {
    let unit = input.interval_unit;
    let interval = input.interval;
    if unit.is_some() != interval.is_some() {
        return Err(ServiceError::InvalidCarrier(
            "intervalUnit and interval must be submitted at the same time or omitted at the same time".to_string(),
        ));
    }
    if let Some(interval) = interval {
        if interval == 0 || interval > MAX_SESSION_INTERVAL {
            return Err(ServiceError::InvalidCarrier(format!(
                "interval must be an integer from 1-{MAX_SESSION_INTERVAL}"
            )));
        }
    }
    if unit.is_none() {
        return Ok(());
    }
    if input.relative_delay_minutes.is_some() {
        return Err(ServiceError::InvalidCarrier(
            "intervalUnit is a period carrier and cannot be submitted at the same time as one-time relativeDelayMinutes".to_string(),
        ));
    }
    if input.schedule_rule.is_some() {
        return Err(ServiceError::InvalidCarrier(
            "intervalUnit carrier cannot be submitted at the same time as direct scheduleRule"
                .to_string(),
        ));
    }
    if !input.recurring {
        return Err(ServiceError::InvalidCarrier(
            "intervalUnit carrier must use recurring=true".to_string(),
        ));
    }
    if input.max_runs.is_some() {
        return Err(ServiceError::InvalidCarrier(
            "intervalUnit carrier cannot be submitted simultaneously with limited maxRuns"
                .to_string(),
        ));
    }
    Ok(())
}

fn validate_schedule_rule(rule: &ScheduleRule) -> Result<(), ServiceError> {
    let invalid = |message: &str| ServiceError::InvalidRule(message.to_string());
    if rule.interval == 0 {
        return Err(invalid("interval must be a positive integer"));
    }
    if rule.unit == ScheduleUnit::Monthly && rule.interval > MAX_MONTHLY_INTERVAL {
        return Err(invalid(&format!(
            "monthly interval cannot exceed {MAX_MONTHLY_INTERVAL}"
        )));
    }
    if rule.hour > 23 {
        return Err(invalid("hour must be an integer from 0-23"));
    }
    if rule.minute > 59 {
        return Err(invalid("minute must be an integer from 0-59"));
    }
    if let Some(weekdays) = &rule.weekdays {
        if weekdays.is_empty() || weekdays.iter().any(|d| *d > 6) {
            return Err(invalid("weekdays must be a non-empty integer array 0-6"));
        }
    }
    if rule.unit == ScheduleUnit::Weekly && rule.weekdays.is_none() {
        return Err(invalid("weekly rules must contain valid weekdays"));
    }
    if rule.unit == ScheduleUnit::Monthly {
        match rule.monthly_mode {
            Some(crate::types::MonthlyMode::Weekday) => {
                if rule.weekdays.is_none() {
                    return Err(invalid("monthly weekday rules must contain valid weekdays"));
                }
            }
            _ => {
                if rule.month_days.is_none() {
                    return Err(invalid("monthly date rules must contain valid monthDays"));
                }
            }
        }
    }
    if let Some(months) = &rule.months {
        if months.is_empty() || months.iter().any(|m| *m == 0 || *m > 12) {
            return Err(invalid("months must be a non-empty integer array 1-12"));
        }
    }
    if let Some(days) = &rule.month_days {
        if days.is_empty() || days.iter().any(|d| *d == 0 || *d > 31) {
            return Err(invalid("monthDays must be a non-empty integer array 1-31"));
        }
    }
    Ok(())
}

fn validate_update_carrier(input: &UpdateInput, has_direct_rule: bool) -> Result<(), ServiceError> {
    let unit = input.interval_unit;
    let interval = input.interval;
    if unit.is_some() != interval.is_some() {
        return Err(ServiceError::InvalidCarrier(
            "intervalUnit and interval must be submitted at the same time or omitted at the same time".to_string(),
        ));
    }
    if let Some(interval) = interval {
        if interval == 0 || interval > MAX_SESSION_INTERVAL {
            return Err(ServiceError::InvalidCarrier(format!(
                "interval must be an integer from 1-{MAX_SESSION_INTERVAL}"
            )));
        }
    }
    if unit.is_none() {
        return Ok(());
    }
    if has_direct_rule {
        return Err(ServiceError::InvalidCarrier(
            "intervalUnit carrier cannot be submitted at the same time as direct scheduleRule"
                .to_string(),
        ));
    }
    if !input.recurring.unwrap_or(true) {
        return Err(ServiceError::InvalidCarrier(
            "intervalUnit carrier must use recurring=true".to_string(),
        ));
    }
    if matches!(input.max_runs, Some(Some(_))) {
        return Err(ServiceError::InvalidCarrier(
            "intervalUnit carrier cannot be submitted simultaneously with limited maxRuns"
                .to_string(),
        ));
    }
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
