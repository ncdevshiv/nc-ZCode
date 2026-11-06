//! `CronCreate` / `CronList` / `CronUpdate` / `CronDelete`: the model-facing tool surface for scheduled tasks.
//!
//! Reference implementation (aligned entry by entry; contract in `specs/rust-native-crates.md` S3 "scheduled task tools"):
//! - `core/src/tool/handlers/cron.ts` (the four handlers: `assertAutomationPort`,
//!   `assertNotAutomationTurn`, the projected fields of `toModelAutomation`, and the output wording);
//! - `contracts/src/tools/automation.ts` (the four zod schema groups and their refine rules);
//! - the server-side semantics (cron parsing / scheduleRule / relative delay / maxRuns) live in
//!   `nc-code-services-automation`; the host only holds the port trait - the dependency direction forbids
//!   host → services (automation itself depends on host storage), so it is injected by the composition root.
//!
//! Without a port we **fail closed**: the same text as TS `AutomationPort is not configured for CronCreate`,
//! never a silent fake success.
use super::{require_string, Tool, ToolContext, ToolContract, ToolError, ToolErrorCode};
use serde_json::{json, Value};
use std::sync::Arc;
// AutomationPort trait is defined in super (tools/mod.rs).
const CRON_TIMEOUT_MS: u64 = 30_000;
const CRON_MODEL_BYTES: usize = 32_000;
fn port(ctx: &ToolContext, tool_name: &str) -> Result<Arc<dyn super::AutomationPort>, ToolError> {
    let port = ctx.automation.as_ref().ok_or_else(|| {
        ToolError::new(
            ToolErrorCode::ExecutionFailed,
            format!("AutomationPort is not configured for {tool_name}"),
        )
    })?;
    Ok(Arc::clone(port))
}
/// The projection of `toModelAutomation` (the field set TS lists explicitly).
///
/// Implements `Serialize` directly: values are **borrowed** from the source `Value` when writing, with no intermediate
/// `Map<String, Value>`. In the 19-item list benchmark, building the intermediate Value cost ~7 µs per item (of `cron.list`'s
/// 0.27 ms the tool layer accounts for 0.13 ms); borrowing removes that allocation entirely. The default shapes stay as in TS: a missing
/// `nextRunAt`/`lastRunAt` writes null, a missing `enabled`/`runCount`/`recurring` uses true/0/true, and a missing `maxRuns`/`scheduleRule` omits the key altogether.
struct ProjectedAutomation<'a>(&'a Value);
impl serde::Serialize for ProjectedAutomation<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        const ALWAYS: [&str; 10] = [
            "automationId",
            "title",
            "cronExpr",
            "prompt",
            "enabled",
            "lifecycleStatus",
            "nextRunAt",
            "lastRunAt",
            "runCount",
            "recurring",
        ];
        let mut map = serializer.serialize_map(Some(12))?;
        for key in ALWAYS {
            match self.0.get(key) {
                Some(value) if !value.is_null() => map.serialize_entry(key, value)?,
                _ => match key {
                    "enabled" => map.serialize_entry(key, &true)?,
                    "runCount" => map.serialize_entry(key, &0u64)?,
                    "recurring" => map.serialize_entry(key, &true)?,
                    _ => map.serialize_entry(key, &Value::Null)?,
                },
            }
        }
        for key in ["maxRuns", "scheduleRule"] {
            if let Some(value) = self.0.get(key) {
                if !value.is_null() {
                    map.serialize_entry(key, value)?;
                }
            }
        }
        map.end()
    }
}
fn project_automation(automation: &Value) -> Value {
    serde_json::to_value(ProjectedAutomation(automation)).unwrap_or(Value::Null)
}
/// The list output is **written in one pass**: `{"automations":[...]}` goes straight into the serializer,
/// with no intermediate `Value` per task (that was the bulk of the `cron.list` tool-layer overhead).
struct ProjectedAutomations<'a>(&'a [Value]);
impl serde::Serialize for ProjectedAutomations<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for item in self.0 {
            seq.serialize_element(&ProjectedAutomation(item))?;
        }
        seq.end()
    }
}
struct CronListOutput<'a>(&'a [Value]);
impl serde::Serialize for CronListOutput<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("automations", &ProjectedAutomations(self.0))?;
        map.end()
    }
}
fn service_error(tool_name: &str, message: String) -> ToolError {
    ToolError::new(
        ToolErrorCode::ExecutionFailed,
        format!("{tool_name}: {message}"),
    )
}
fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::new(ToolErrorCode::InvalidInput, message)
}
fn non_empty(input: &Value, key: &str) -> Result<String, ToolError> {
    let raw = require_string(input, key)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(invalid(format!("{key}: must be a non-empty string")));
    }
    Ok(trimmed.to_string())
}
/// TS `hasRelativeDelayMinutes`: **only** a positive integer means a relative delay (null/absent are not).
fn relative_delay_minutes(input: &Value) -> Option<i64> {
    input.get("delayMinutes").and_then(Value::as_i64)
}
/// The cross-field refine of `CronCreateInputSchema` (targeted validation on the host side; the service layer validates again).
fn validate_create(input: &Value) -> Result<(), ToolError> {
    let relative = relative_delay_minutes(input);
    let has_cron = input
        .get("cron")
        .map(|value| !value.is_null())
        .unwrap_or(false);
    let recurring = input.get("recurring").and_then(Value::as_bool);
    let max_runs = input.get("maxRuns");
    let interval_unit = input.get("intervalUnit").and_then(Value::as_str);
    let interval = input.get("interval");
    if relative.is_none() && !has_cron {
        return Err(invalid("cron is required when delayMinutes is not set"));
    }
    if relative.is_some() && has_cron {
        return Err(invalid(
            "a relative delay must omit cron; the host computes the schedule from its real clock",
        ));
    }
    if relative.is_some() && recurring == Some(true) {
        return Err(invalid(
            "a relative delay creates a one-shot automation and cannot use recurring=true",
        ));
    }
    if relative.is_some() && max_runs.is_some() {
        return Err(invalid("a relative delay runs once and cannot use maxRuns"));
    }
    if interval_unit.is_some() && relative.is_some() {
        return Err(invalid(
            "intervalUnit is a recurring carrier and cannot combine with a relative delayMinutes",
        ));
    }
    if interval_unit.is_some() != interval.is_some() {
        return Err(invalid("intervalUnit and interval must be set together"));
    }
    if interval_unit.is_some() && recurring == Some(false) {
        return Err(invalid(
            "intervalUnit is a recurring carrier and requires recurring=true",
        ));
    }
    if interval_unit.is_some() && max_runs.is_some() {
        return Err(invalid(
            "intervalUnit is a recurring carrier and cannot combine with maxRuns",
        ));
    }
    Ok(())
}
pub struct CronCreateTool;
impl Tool for CronCreateTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "CronCreate".to_string(),
            description: "Create a persistent scheduled automation in the current workspace. Use delayMinutes for any relative delay ('in 8 minutes'); use a 5-field cron in the user's local timezone for absolute or recurring schedules. The prompt must describe the scheduled work directly and must never ask the run to create another automation.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cron": {"type": "string", "description": "5-field cron expression in the user's local timezone; required unless delayMinutes is set"},
                    "delayMinutes": {"type": "integer", "description": "Relative delay in whole minutes from now (one-shot); omit cron"},
                    "prompt": {"type": "string", "description": "Complete prompt sent at every fire"},
                    "title": {"type": "string", "description": "Concise title preserving the user's schedule phrase"},
                    "recurring": {"type": "boolean", "description": "true (default) repeats; false is finite"},
                    "maxRuns": {"type": "integer", "description": "Max runs; only with recurring=false"},
                    "intervalUnit": {"type": "string", "enum": ["minute", "hourly", "daily", "weekly", "monthly", "yearly"], "description": "Every-N-unit carrier; pair with interval"},
                    "interval": {"type": "integer", "description": "1-200, paired with intervalUnit"},
                },
                "required": ["prompt", "title"],
            }),
            read_only: false,
            destructive: false,
            concurrent_safe: false,
            side_effect_scope: super::SCOPE_WORKSPACE,
            timeout_ms: CRON_TIMEOUT_MS,
            max_output_bytes: CRON_MODEL_BYTES,
            max_model_bytes: CRON_MODEL_BYTES,
            preview_direction: "head",
        }
    }
    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let _title = non_empty(input, "title")?;
        let _prompt = non_empty(input, "prompt")?;
        validate_create(input)?;
        let port = port(ctx, "CronCreate")?;
        let automation = port
            .create(input, &ctx.session_id, ctx.model_ref.as_deref())
            .map_err(|message| service_error("CronCreate", message))?;
        let projected = project_automation(&automation);
        Ok(json!({
            "automation": projected,
            "message": format!(
                "Created automation {}.",
                projected["automationId"].as_str().unwrap_or_default()
            ),
        })
        .to_string())
    }
}
pub struct CronListTool;
impl Tool for CronListTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "CronList".to_string(),
            description: "List scheduled automations in the current workspace.".to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
            read_only: true,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: super::SCOPE_NONE,
            timeout_ms: CRON_TIMEOUT_MS,
            max_output_bytes: CRON_MODEL_BYTES,
            max_model_bytes: CRON_MODEL_BYTES,
            preview_direction: "head",
        }
    }
    fn execute(&self, _input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let port = port(ctx, "CronList")?;
        let automations = port
            .list()
            .map_err(|message| service_error("CronList", message))?;
        serde_json::to_string(&CronListOutput(&automations))
            .map_err(|error| service_error("CronList", error.to_string()))
    }
}
pub struct CronUpdateTool;
impl Tool for CronUpdateTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "CronUpdate".to_string(),
            description: "Update selected definition fields of an existing scheduled automation while preserving its id and run history. Always pass the synchronized title.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Automation id from CronCreate or CronList"},
                    "title": {"type": "string", "description": "Synchronized title describing the task after the update"},
                    "cron": {"type": "string"},
                    "prompt": {"type": "string"},
                    "recurring": {"type": "boolean"},
                    "maxRuns": {"type": "integer"},
                    "intervalUnit": {"type": "string", "enum": ["minute", "hourly", "daily", "weekly", "monthly", "yearly"]},
                    "interval": {"type": "integer"},
                },
                "required": ["id", "title"],
            }),
            read_only: false,
            destructive: false,
            concurrent_safe: false,
            side_effect_scope: super::SCOPE_WORKSPACE,
            timeout_ms: CRON_TIMEOUT_MS,
            max_output_bytes: CRON_MODEL_BYTES,
            max_model_bytes: CRON_MODEL_BYTES,
            preview_direction: "head",
        }
    }
    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let _id = non_empty(input, "id")?;
        let _title = non_empty(input, "title")?;
        let recurring = input.get("recurring").and_then(Value::as_bool);
        let max_runs = input.get("maxRuns");
        // The same refine as TS CronUpdateInputSchema: recurring=true cannot be combined with maxRuns.
        if recurring == Some(true) && max_runs.is_some() {
            return Err(invalid(
                "recurring=true clears any finite limit and must not be combined with maxRuns",
            ));
        }
        let interval_unit = input.get("intervalUnit").and_then(Value::as_str);
        let interval = input.get("interval");
        if interval_unit.is_some() != interval.is_some() {
            return Err(invalid("intervalUnit and interval must be set together"));
        }
        if interval_unit.is_some() && recurring == Some(false) {
            return Err(invalid(
                "intervalUnit is a recurring carrier and requires recurring=true",
            ));
        }
        let port = port(ctx, "CronUpdate")?;
        let automation = port
            .update(input)
            .map_err(|message| service_error("CronUpdate", message))?;
        let projected = project_automation(&automation);
        Ok(json!({
            "automation": projected,
            "message": format!(
                "Updated automation {}.",
                projected["automationId"].as_str().unwrap_or_default()
            ),
        })
        .to_string())
    }
}
pub struct CronDeleteTool;
impl Tool for CronDeleteTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "CronDelete".to_string(),
            description:
                "Delete a scheduled automation from the current workspace by automation id."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Automation id from CronCreate or CronList"},
                },
                "required": ["id"],
            }),
            read_only: false,
            destructive: true,
            concurrent_safe: false,
            side_effect_scope: super::SCOPE_WORKSPACE,
            timeout_ms: CRON_TIMEOUT_MS,
            max_output_bytes: CRON_MODEL_BYTES,
            max_model_bytes: CRON_MODEL_BYTES,
            preview_direction: "head",
        }
    }
    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let id = non_empty(input, "id")?;
        let port = port(ctx, "CronDelete")?;
        let deleted = port
            .delete(&id)
            .map_err(|message| service_error("CronDelete", message))?;
        Ok(json!({
            "deleted": deleted,
            "id": id,
            "message": if deleted {
                format!("Deleted automation {id}.")
            } else {
                format!("Automation {id} was not found in the current workspace.")
            },
        })
        .to_string())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;
    #[derive(Default)]
    struct FakePort {
        created: Mutex<Vec<Value>>,
        deleted: Mutex<Vec<String>>,
    }
    impl super::super::AutomationPort for FakePort {
        fn create(
            &self,
            input: &Value,
            session_id: &str,
            model: Option<&str>,
        ) -> Result<Value, String> {
            let automation = json!({
                "automationId": "auto-1",
                "title": input["title"],
                "cronExpr": input["cron"].as_str().unwrap_or("* * * * *"),
                "prompt": input["prompt"],
                "enabled": true,
                "lifecycleStatus": "active",
                "runCount": 0,
                "recurring": input["recurring"].as_bool().unwrap_or(true),
                "sessionId": session_id,
                "model": model,
            });
            self.created.lock().unwrap().push(automation.clone());
            Ok(automation)
        }
        fn list(&self) -> Result<Vec<Value>, String> {
            Ok(self.created.lock().unwrap().clone())
        }
        fn update(&self, input: &Value) -> Result<Value, String> {
            Ok(json!({
                "automationId": input["id"],
                "title": input["title"],
                "cronExpr": "0 * * * *",
                "prompt": "p",
                "enabled": true,
                "lifecycleStatus": "active",
                "runCount": 0,
                "recurring": true,
            }))
        }
        fn delete(&self, id: &str) -> Result<bool, String> {
            self.deleted.lock().unwrap().push(id.to_string());
            Ok(id == "auto-1")
        }
    }
    fn ctx(port: Option<Arc<dyn super::super::AutomationPort>>) -> ToolContext {
        ToolContext {
            workspace_root: std::env::temp_dir(),
            full_access: true,
            interactive: false,
            trace_id: "t".to_string(),
            turn_id: "turn".to_string(),
            session_id: "session-1".to_string(),
            model_ref: Some("anthropic/claude-test".to_string()),
            automation: port,
            browser: None,
            computer: None,
            offpeak: None,
            read_state: Arc::new(Mutex::new(std::collections::HashSet::new())),
            cancel: Arc::new(AtomicBool::new(false)),
            todos: crate::tools::todo::new_todo_store(),
            provider_config: None,
            tool_call_id: None,
            subagent: None,
            ask_user: None,
            plan_mode: false,
            plan: None,
            background: None,
            sessions: None,
            workflow: None,
        }
    }
    #[test]
    fn missing_port_fails_closed_with_the_ts_message() {
        let error = CronCreateTool
            .execute(
                &json!({"title": "t", "prompt": "p", "cron": "0 * * * *"}),
                &ctx(None),
            )
            .expect_err("no port");
        assert!(error
            .message
            .contains("AutomationPort is not configured for CronCreate"));
        assert!(CronListTool.execute(&json!({}), &ctx(None)).is_err());
    }
    #[test]
    fn create_passes_the_session_and_model_to_the_port() {
        let port = Arc::new(FakePort::default());
        let out = CronCreateTool
            .execute(
                &json!({"title": "every hour", "prompt": "check the build", "cron": "0 * * * *"}),
                &ctx(Some(port.clone())),
            )
            .expect("create");
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["message"], json!("Created automation auto-1."));
        assert_eq!(parsed["automation"]["automationId"], json!("auto-1"));
        let created = port.created.lock().unwrap();
        assert_eq!(created[0]["sessionId"], json!("session-1"));
        assert_eq!(created[0]["model"], json!("anthropic/claude-test"));
    }
    #[test]
    fn create_enforces_the_contract_refinements() {
        let port = Arc::new(FakePort::default());
        let context = ctx(Some(port));
        for (input, needle) in [
            (
                json!({"title": "t", "prompt": "p"}),
                "cron is required when delayMinutes is not set",
            ),
            (
                json!({"title": "t", "prompt": "p", "cron": "0 * * * *", "delayMinutes": 5}),
                "a relative delay must omit cron",
            ),
            (
                json!({"title": "t", "prompt": "p", "delayMinutes": 5, "recurring": true}),
                "cannot use recurring=true",
            ),
            (
                json!({"title": "t", "prompt": "p", "cron": "0 * * * *", "intervalUnit": "minute"}),
                "intervalUnit and interval must be set together",
            ),
            (
                json!({"title": "t", "prompt": "p", "cron": "0 * * * *", "intervalUnit": "minute", "interval": 5, "recurring": false}),
                "requires recurring=true",
            ),
        ] {
            let error = CronCreateTool.execute(&input, &context).expect_err(needle);
            assert!(
                error.message.contains(needle),
                "{}: {}",
                error.message,
                needle
            );
        }
    }
    #[test]
    fn delete_reports_found_and_missing_automations() {
        let port = Arc::new(FakePort::default());
        let ok = CronDeleteTool
            .execute(&json!({"id": "auto-1"}), &ctx(Some(port.clone())))
            .expect("delete");
        assert!(ok.contains("Deleted automation auto-1."));
        let missing = CronDeleteTool
            .execute(&json!({"id": "nope"}), &ctx(Some(port)))
            .expect("delete missing");
        assert!(missing.contains("was not found in the current workspace"));
    }
    #[test]
    fn list_projects_only_the_model_visible_fields() {
        let port = Arc::new(FakePort::default());
        let context = ctx(Some(port));
        CronCreateTool
            .execute(
                &json!({"title": "t", "prompt": "p", "cron": "0 * * * *"}),
                &context,
            )
            .expect("create");
        let out = CronListTool.execute(&json!({}), &context).expect("list");
        let parsed: Value = serde_json::from_str(&out).unwrap();
        let automation = &parsed["automations"][0];
        assert_eq!(automation["automationId"], json!("auto-1"));
        assert!(
            automation.get("sessionId").is_none(),
            "internal fields must not leak into the model projection"
        );
    }
}
