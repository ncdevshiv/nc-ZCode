//! Dispatch of the automation/* members of the nc-code-agent channel (the 10 TS zcodeAgentService automation methods).
//! The input is frontend camelCase JSON (the TS ZCodeAgent*AutomationParams), mapped onto the service inputs;
//! the output serializes the domain types with serde camelCase, the same shape as TS ZCodeAutomation / Run.

use nc_code_rpc::WireValue;
use nc_code_service_core::ServiceError;
use nc_code_services_automation::types::{ModelSelection, ScheduleRule};
use nc_code_services_automation::{CreateInput, Scope, UpdateInput};
use serde::Serialize;
use serde_json::{json, Value};

use crate::automation_dispatch::{AutomationDispatcher, RunNowStatus};
use crate::service::AgentService;

/// automation service error → channel error (the user-displayable message is preserved).
fn map_err(error: nc_code_services_automation::ServiceError) -> ServiceError {
    ServiceError::new(error.to_string())
}

fn to_value(value: &impl Serialize) -> Result<Value, ServiceError> {
    serde_json::to_value(value).map_err(|error| ServiceError::new(error.to_string()))
}

/// These commands belong to the automation surface (everything else goes back to the main match).
pub(crate) fn is_automation_command(command: &str) -> bool {
    matches!(
        command,
        "listAutomations"
            | "listAllAutomations"
            | "createAutomation"
            | "updateAutomation"
            | "deleteAutomation"
            | "setAutomationEnabled"
            | "restartAutomation"
            | "runAutomationNow"
            | "listAutomationRuns"
            | "deleteAutomationRun"
    )
}

fn scope_of(params: &Value) -> Scope {
    Scope {
        workspace_path: params
            .get("workspacePath")
            .and_then(Value::as_str)
            .map(str::to_string),
        workspace_identity: params
            .get("workspaceIdentity")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

fn required_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, ServiceError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ServiceError::invalid_params(format!("{key} is required")))
}

fn parse_model_selection(params: &Value) -> Result<Option<ModelSelection>, ServiceError> {
    match params.get("modelSelection") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value::<ModelSelection>(value.clone())
            .map(Some)
            .map_err(|error| {
                ServiceError::invalid_params(format!("invalid modelSelection: {error}"))
            }),
    }
}

fn parse_schedule_rule_field(params: &Value) -> Result<Option<Option<ScheduleRule>>, ServiceError> {
    match params.get("scheduleRule") {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(value) => serde_json::from_value::<ScheduleRule>(value.clone())
            .map(|rule| Some(Some(rule)))
            .map_err(|error| {
                ServiceError::invalid_params(format!("invalid scheduleRule: {error}"))
            }),
    }
}

fn create_input(params: &Value) -> Result<CreateInput, ServiceError> {
    Ok(CreateInput {
        title: required_str(params, "title")?.to_string(),
        cron_expr: required_str(params, "cronExpr")?.to_string(),
        relative_delay_minutes: params
            .get("relativeDelayMinutes")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        prompt: required_str(params, "prompt")?.to_string(),
        model_selection: parse_model_selection(params)?,
        mode: params
            .get("mode")
            .and_then(Value::as_str)
            .map(str::to_string),
        workspace_path: required_str(params, "workspacePath")?.to_string(),
        workspace_identity: params
            .get("workspaceIdentity")
            .and_then(Value::as_str)
            .map(str::to_string),
        target_task_id: params
            .get("targetTaskId")
            .and_then(Value::as_str)
            .map(str::to_string),
        recurring: params
            .get("recurring")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        max_runs: params
            .get("maxRuns")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        end_at: params.get("endAt").and_then(Value::as_i64),
        schedule_rule: parse_schedule_rule_field(params)?.flatten(),
        interval_unit: None,
        interval: None,
    })
}

fn update_input(params: &Value) -> Result<UpdateInput, ServiceError> {
    let model_selection = match params.get("modelSelection") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(value) => Some(Some(
            serde_json::from_value::<ModelSelection>(value.clone()).map_err(|error| {
                ServiceError::invalid_params(format!("invalid modelSelection: {error}"))
            })?,
        )),
    };
    let mode = match params.get("mode") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(value) => Some(value.as_str().map(str::to_string)),
    };
    let max_runs = match params.get("maxRuns") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(value) => Some(value.as_u64().map(|v| v as u32)),
    };
    let end_at = match params.get("endAt") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(value) => Some(value.as_i64()),
    };
    Ok(UpdateInput {
        title: params
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string),
        cron_expr: params
            .get("cronExpr")
            .and_then(Value::as_str)
            .map(str::to_string),
        prompt: params
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::to_string),
        model_selection,
        mode,
        recurring: params.get("recurring").and_then(Value::as_bool),
        max_runs,
        end_at,
        schedule_rule: parse_schedule_rule_field(params)?,
        interval_unit: None,
        interval: None,
        schedule_edited_by_user: params.get("scheduleEditedByUser").and_then(Value::as_bool),
    })
}

/// Dispatch one automation/* command. A non-automation command returns None (back to the main match).
///
/// The input is AgentService instead of a separate AutomationService: `runAutomationNow` needs the
/// dispatcher (create session / send prompt / observe the terminal state), and the dispatcher and the channel must
/// share the same automation service instance (the same single sqlite database) and the same workspace runtime table.
pub(crate) async fn dispatch(
    agent: &AgentService,
    command: &str,
    args: &[WireValue],
) -> Option<Result<WireValue, ServiceError>> {
    if !is_automation_command(command) {
        return None;
    }
    let automation = agent.automation();
    let params = match args.first() {
        Some(WireValue::Json(value)) => value.clone(),
        _ => Value::Object(serde_json::Map::new()),
    };
    // The inner async block lets `?` act on Result<ServiceError>; automation service errors are converted by map_err.
    let run = async {
        let value = match command {
            "listAutomations" => to_value(&automation.list(&scope_of(&params)).map_err(map_err)?)?,
            "listAllAutomations" => {
                to_value(&automation.list(&Scope::default()).map_err(map_err)?)?
            }
            "createAutomation" => {
                let created = automation
                    .create(&create_input(&params)?, &scope_of(&params))
                    .map_err(map_err)?;
                to_value(&created)?
            }
            "updateAutomation" => {
                let id = required_str(&params, "automationId")?;
                match automation
                    .update(id, &update_input(&params)?, &scope_of(&params))
                    .map_err(map_err)
                {
                    Ok(Some(updated)) => to_value(&updated)?,
                    Ok(None) => Value::Null,
                    Err(error) => return Err(error),
                }
            }
            "deleteAutomation" => {
                let id = required_str(&params, "automationId")?;
                automation.delete(id, &scope_of(&params)).map_err(map_err)?;
                Value::Null
            }
            "setAutomationEnabled" => {
                let id = required_str(&params, "automationId")?;
                let enabled = params
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| ServiceError::invalid_params("enabled is required"))?;
                automation
                    .set_enabled(id, enabled, &scope_of(&params))
                    .map_err(map_err)?;
                Value::Null
            }
            "restartAutomation" => {
                let id = required_str(&params, "automationId")?;
                automation
                    .restart(id, &scope_of(&params))
                    .map_err(map_err)?;
                Value::Null
            }
            "runAutomationNow" => {
                let id = required_str(&params, "automationId")?;
                // Running now must stay scoped to the caller's workspace (ZCodeAgentAutomationIdParams
                // extends ZCodeAgentWorkspaceTarget, workspacePath required) — a cross-workspace
                // claim would bypass the ownership check.
                required_str(&params, "workspacePath")?;
                let status = AutomationDispatcher::new(agent)
                    .run_now(id, &scope_of(&params))
                    .await?;
                match status {
                    // Same shape as TS ZCodeAgentRunAutomationNowResult: reports only the claim result,
                    // the run's real terminal state is written in the automation_runs ledger.
                    RunNowStatus::Queued { .. } => json!({"status": "queued"}),
                    RunNowStatus::Duplicate => json!({"status": "duplicate"}),
                }
            }
            "listAutomationRuns" => {
                let id = required_str(&params, "automationId")?;
                to_value(
                    &automation
                        .list_runs(id, &scope_of(&params))
                        .map_err(map_err)?,
                )?
            }
            "deleteAutomationRun" => {
                let run_id = required_str(&params, "runId")?;
                automation
                    .delete_run(run_id, &scope_of(&params))
                    .map_err(map_err)?;
                Value::Null
            }
            // The guard (is_automation_command) already narrowed the range to here; landing in this arm means the
            // inventory and the dispatch disagree — report a parameter error honestly (fail-closed), no panic and no pretending success.
            _ => {
                return Err(ServiceError::invalid_params(format!(
                    "automation command is declared but not dispatched: {command}"
                )))
            }
        };
        Ok(WireValue::Json(value))
    };
    Some(run.await)
}
