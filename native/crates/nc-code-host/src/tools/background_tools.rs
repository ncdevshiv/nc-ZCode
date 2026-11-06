//! `TaskOutput` / `TaskStop`: the two tools the model uses to read and write background commands.
//!
//! Reference implementations (aligned entry by entry, contract in `specs/rust-native-crates.md` S3 "background Bash"):
//! - `core/src/tool/handlers/task-output.ts` (the `block`/`timeout` defaults, the three
//!   `retrieval_status` values, the `No task found with ID:` wording);
//! - `core/src/tool/handlers/task-output-bash.ts` (projection fields: `task_id`/`task_type`/
//!   `status`/`description`/`output`/`exitCode`/`outputFile`);
//! - `core/src/tool/handlers/task-stop.ts` (`Successfully stopped task: <id> (<cmd>)`,
//!   and `Task <id> is not running (status: <status>)` when it is not running);
//! - `contracts/src/tools/task-output.ts` (the wire shape of input/result) and
//!   `contracts/src/tools/tool-result-metadata.ts` (the boundaries of the two display
//!   variants `task_output` / `task_stop`).
//!
//! There is exactly one source of truth: the session's [`crate::background::BackgroundRegistry`]. This module reads no
//! second copy of state and never infers a task status backwards from the model-visible text.
use super::{
    optional_semantic_bool, optional_u64, require_string, Tool, ToolContext, ToolContract,
    ToolError, ToolErrorCode, SCOPE_NONE, SCOPE_SESSION,
};
use crate::background::{BackgroundRegistry, BackgroundWork, BG_RUNNING};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
/// TS `TASK_OUTPUT_DEFAULT_LENGTH` (the character budget for the output text inside the result).
const TASK_OUTPUT_DEFAULT_LENGTH: usize = 32_000;
/// TS `TASK_OUTPUT_RESULT_BUDGET_BYTES`.
const TASK_OUTPUT_RESULT_BUDGET_BYTES: usize = 400_000;
/// TS `TASK_OUTPUT_POLL_INTERVAL_MS`.
const TASK_OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const TASK_OUTPUT_DEFAULT_WAIT_MS: u64 = 30_000;
const TASK_OUTPUT_MAX_WAIT_MS: u64 = 600_000;
/// TS `TASK_OUTPUT_DISPLAY_MAX_OUTPUT_CHARS` / `…_STATUS_CHARS`.
const DISPLAY_MAX_OUTPUT_CHARS: usize = 2_000;
const DISPLAY_MAX_STATUS_CHARS: usize = 64;
/// TS `MAX_TASK_STOP_DISPLAY_FIELD_BYTES`.
const MAX_TASK_STOP_DISPLAY_FIELD_BYTES: usize = 16 * 1024;
/// Gets this session's background registry; when absent (internal fixtures/standalone tool calls) it fails closed.
/// The task's `task_type` is carried by the registry entry itself (`local_bash` / `local_agent`);
/// this layer does not guess the type from the tool name, and a task id that does not exist always yields `No task found with ID:`.
fn registry(ctx: &ToolContext) -> Result<Arc<BackgroundRegistry>, ToolError> {
    let missing = || {
        ToolError::new(
            ToolErrorCode::ExecutionFailed,
            "background tasks are not available for this invocation",
        )
    };
    Ok(Arc::clone(ctx.background.as_ref().ok_or_else(missing)?))
}
/// `TaskOutputTask` (the same group of fields as TS `projectBashTask`).
///
/// `exitCode` uses `null` instead of being omitted: TS's schema is `z.number().nullable().optional()`,
/// and while the command has not finished there genuinely **is** no exit code — an explicit null is more honest than letting the client guess.
fn project_task(work: &BackgroundWork, output: &str) -> Value {
    let mut task = json!({
        "task_id": work.work_id,
        "task_type": work.task_type,
        "status": work.status,
        "description": work.command,
        "output": output,
        "exitCode": work.exit_code,
    });
    if let Some(error) = &work.error {
        task["error"] = json!(error);
    }
    // TS only writes outputFile when there really is a readable writable path.
    task["outputFile"] = json!(work.output_path.to_string_lossy());
    task
}
pub struct TaskOutputTool;
impl Tool for TaskOutputTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "TaskOutput".to_string(),
            description: "Read the output of a background task started with Bash(run_in_background). Returns retrieval_status plus the task (status, output tail, exit code). Set block=false to read without waiting for completion.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": {"type": "string", "description": "The task ID to get output from"},
                    "block": {"type": "boolean", "description": "Whether to wait for completion"},
                    "timeout": {"type": "integer", "description": "Max wait time in ms (0-600000)"},
                },
                "required": ["task_id", "block", "timeout"],
            }),
            read_only: true,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: SCOPE_NONE,
            timeout_ms: TASK_OUTPUT_MAX_WAIT_MS,
            max_output_bytes: TASK_OUTPUT_RESULT_BUDGET_BYTES,
            max_model_bytes: TASK_OUTPUT_RESULT_BUDGET_BYTES,
            preview_direction: "head",
        }
    }
    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let task_id = require_string(input, "task_id")?.to_string();
        let registry = registry(ctx)?;
        // TS's `block` defaults to true (`semanticBoolean(z.boolean().default(true))`).
        let block = optional_semantic_bool(input, "block").unwrap_or(true);
        let wait_ms = optional_u64(input, "timeout")
            .unwrap_or(TASK_OUTPUT_DEFAULT_WAIT_MS)
            .min(TASK_OUTPUT_MAX_WAIT_MS);
        let initial = registry.get(&task_id).ok_or_else(|| {
            ToolError::new(
                ToolErrorCode::ExecutionFailed,
                format!("No task found with ID: {task_id}"),
            )
        })?;
        if !block {
            // Non-blocking: still running reports `not_ready` and does **not** pretend success.
            let (output, _) = registry.read_output(&task_id, TASK_OUTPUT_DEFAULT_LENGTH);
            if initial.status == BG_RUNNING {
                return Ok(json!({"retrieval_status": "not_ready", "task": project_task(&initial, &output)}).to_string());
            }
            return Ok(
                json!({"retrieval_status": "success", "task": project_task(&initial, &output)})
                    .to_string(),
            );
        }
        // Blocking wait: still unsettled at the deadline → `timeout` (with the current projection, same as TS: while the task exists it is returned).
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        let mut current = initial;
        while current.status == BG_RUNNING {
            if ctx.is_cancelled() {
                return Err(ToolError::new(ToolErrorCode::Cancelled, "turn cancelled"));
            }
            if Instant::now() >= deadline {
                let (output, _) = registry.read_output(&task_id, TASK_OUTPUT_DEFAULT_LENGTH);
                return Ok(
                    json!({"retrieval_status": "timeout", "task": project_task(&current, &output)})
                        .to_string(),
                );
            }
            std::thread::sleep(TASK_OUTPUT_POLL_INTERVAL);
            current = registry.get(&task_id).ok_or_else(|| {
                ToolError::new(
                    ToolErrorCode::ExecutionFailed,
                    format!("No task found with ID: {task_id}"),
                )
            })?;
        }
        let (output, _) = registry.read_output(&task_id, TASK_OUTPUT_DEFAULT_LENGTH);
        // The model has already read the terminal state: claim the completion notification so the same result is not notified again (same as TS:
        // `markTaskNotified` commits the claim after the terminal projection succeeds).
        let _ = registry.claim_notification(&task_id);
        Ok(
            json!({"retrieval_status": "success", "task": project_task(&current, &output)})
                .to_string(),
        )
    }

    /// The `task_output` display variant (the TaskOutput branch of TS `createToolResultDisplay`):

    /// taken from **this result**, status clipped to 64 characters and output to 2000 characters, empty-string fields absent entirely

    /// (the schema is `min(1)`).
    fn result_display_for(&self, _input: &Value, output_text: &str) -> Option<Value> {
        let parsed: Value = serde_json::from_str(output_text).ok()?;
        let status = parsed["retrieval_status"].as_str()?;
        if !matches!(status, "success" | "not_ready" | "timeout") {
            return None;
        }
        let mut display = json!({"kind": "task_output", "retrievalStatus": status});
        let task_status = parsed["task"]["status"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .chars()
            .take(DISPLAY_MAX_STATUS_CHARS)
            .collect::<String>();
        if !task_status.is_empty() {
            display["taskStatus"] = json!(task_status);
        }
        let full_output = parsed["task"]["output"]
            .as_str()
            .unwrap_or_default()
            .trim_end()
            .to_string();
        if !full_output.is_empty() {
            let bounded: String = full_output.chars().take(DISPLAY_MAX_OUTPUT_CHARS).collect();
            display["output"] = json!(bounded);
            if full_output.chars().count() > bounded.chars().count() {
                display["truncated"] = json!(true);
            }
        }
        Some(display)
    }
}
pub struct TaskStopTool;
impl TaskStopTool {
    fn stop(&self, task_id: &str, registry: &Arc<BackgroundRegistry>) -> Result<String, ToolError> {
        let work = registry.get(task_id).ok_or_else(|| {
            ToolError::new(
                ToolErrorCode::ExecutionFailed,
                format!("No task found with ID: {task_id}"),
            )
        })?;
        if work.status != BG_RUNNING {
            // TS's `background_task_not_running` (code 3): already terminal is not a reason to retry a failed call, and the
            // wording must hand the current status back to the model.
            return Err(ToolError::new(
                ToolErrorCode::ExecutionFailed,
                format!("Task {task_id} is not running (status: {})", work.status),
            ));
        }
        registry.mark_cancelled(task_id);
        Ok(json!({
            "message": format!("Successfully stopped task: {task_id} ({})", work.command),
            "task_id": task_id,
            "task_type": work.task_type,
            "command": work.command,
        })
        .to_string())
    }
}
impl Tool for TaskStopTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "TaskStop".to_string(),
            description: "Stop a running background task by its ID. Takes task_id, returns the stopped command. Use it to terminate a long-running command started with Bash(run_in_background).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": {"type": "string", "description": "The task ID to stop"},
                    "shell_id": {"type": "string", "description": "Deprecated: use task_id (the alias TS uses under the same name; this runtime supports it)"},
                },
                "required": ["task_id"],
            }),
            read_only: false,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: SCOPE_SESSION,
            timeout_ms: 10_000,
            max_output_bytes: 100_000,
            max_model_bytes: 100_000,
            preview_direction: "head",
        }
    }

    /// TS `permission.alwaysAllowPatternSources: ["toolName"]` + `needsApproval: false`:
    ///
    /// stopping a background command the model started **itself** does not ask the user a second time. Note this does not relax plan mode —
    ///
    /// TS gives TaskStop no `allowedInPlanMode`, the plan gate is ordered before it, and this host keeps the same order.
    fn always_allowed_by_name(&self) -> bool {
        true
    }
    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        // TS: `parsed.task_id ?? parsed.shell_id` (the old input names of the KillShell/KillBash aliases).
        let task_id = match require_string(input, "task_id") {
            Ok(value) => value.to_string(),
            Err(_) => match require_string(input, "shell_id") {
                Ok(value) => value.to_string(),
                Err(_) => {
                    return Err(ToolError::new(
                        ToolErrorCode::InvalidInput,
                        "Missing required parameter: task_id",
                    ));
                }
            },
        };
        self.stop(&task_id, &registry(ctx)?)
    }

    /// The `task_stop` display variant (the TaskStop branch of TS `createToolResultDisplay`):
    ///
    /// a standalone `command` field already exists, so repeating the command in the result-row wording is duplication, hence the parenthesised part is stripped
    ///
    /// by the same rule as `compactTaskStopDisplayMessage`; the two fields are length-bounded separately and
    ///
    /// if either was truncated then `truncated: true`.
    fn result_display_for(&self, _input: &Value, output_text: &str) -> Option<Value> {
        let parsed: Value = serde_json::from_str(output_text).ok()?;
        let task_id = parsed["task_id"].as_str()?;
        let task_type = parsed["task_type"].as_str()?;
        let message = parsed["message"].as_str()?;
        let command = parsed["command"].as_str();
        let standard = format!(
            "Successfully stopped task: {task_id} ({}) ",
            command.unwrap_or_default()
        );
        let compacted: String = if message == standard.trim_end() {
            format!("Successfully stopped task: {task_id}")
        } else {
            message.to_string()
        };
        let bounded_command =
            command.map(|value| bound_field(value, MAX_TASK_STOP_DISPLAY_FIELD_BYTES));
        let bounded_message = bound_field(&compacted, MAX_TASK_STOP_DISPLAY_FIELD_BYTES);
        let mut display = json!({
            "kind": "task_stop",
            "taskId": task_id,
            "taskType": task_type,
            "message": bounded_message.value,
        });
        if let Some(bounded) = &bounded_command {
            display["command"] = json!(bounded.value);
        }
        if bounded_command.as_ref().is_some_and(|b| b.truncated) || bounded_message.truncated {
            display["truncated"] = json!(true);
        }
        Some(display)
    }
}
struct BoundedField {
    value: String,
    truncated: bool,
}
fn bound_field(value: &str, max_bytes: usize) -> BoundedField {
    if value.len() <= max_bytes {
        return BoundedField {
            value: value.to_string(),
            truncated: false,
        };
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    BoundedField {
        value: value[..end].to_string(),
        truncated: true,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;
    fn registry(dir: &Path) -> Arc<BackgroundRegistry> {
        BackgroundRegistry::new("ses-1".to_string(), dir.join("background"))
    }
    fn ctx(root: &Path, background: Option<Arc<BackgroundRegistry>>) -> ToolContext {
        ToolContext {
            workspace_root: root.to_path_buf(),
            full_access: true,
            interactive: false,
            trace_id: "t".to_string(),
            turn_id: "turn".to_string(),
            session_id: "session-1".to_string(),
            model_ref: None,
            automation: None,
            browser: None,
            computer: None,
            offpeak: None,
            sessions: None,
            workflow: None,
            read_state: Arc::new(Mutex::new(std::collections::HashSet::new())),
            cancel: Arc::new(AtomicBool::new(false)),
            todos: crate::tools::todo::new_todo_store(),
            provider_config: None,
            tool_call_id: Some("toolu-1".to_string()),
            subagent: None,
            ask_user: None,
            plan_mode: false,
            plan: None,
            background,
        }
    }
    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zcode-bg-tools-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }
    fn start(registry: &Arc<BackgroundRegistry>, command: &str, dir: &Path) -> String {
        let (work_id, _) = registry.spawn_bash(crate::background::BashSpawn {
            shell: crate::shell::shell(),
            command,
            description: None,
            cwd: dir,
            tool_call_id: Some("toolu-1"),
            now_ms: 1,
        });
        work_id
    }
    #[test]
    fn unknown_task_id_is_a_failure_with_the_ts_text() {
        let dir = temp_dir("unknown");
        let registry = self::registry(&dir);
        let error = TaskOutputTool
            .execute(
                &json!({"task_id": "nope", "block": false, "timeout": 0}),
                &ctx(&dir, Some(registry)),
            )
            .expect_err("unknown task must fail");
        assert_eq!(error.code, ToolErrorCode::ExecutionFailed);
        assert_eq!(error.message, "No task found with ID: nope");
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn non_blocking_read_of_a_finished_task_is_success_with_the_output() {
        let dir = temp_dir("done");
        let registry = self::registry(&dir);
        let work_id = start(&registry, "echo task-output-probe", &dir);
        for _ in 0..200 {
            if registry.get(&work_id).unwrap().status != BG_RUNNING {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let out = TaskOutputTool
            .execute(
                &json!({"task_id": work_id, "block": false, "timeout": 0}),
                &ctx(&dir, Some(Arc::clone(&registry))),
            )
            .expect("read");
        let parsed: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(parsed["retrieval_status"], json!("success"));
        assert_eq!(parsed["task"]["task_type"], json!("local_bash"));
        assert_eq!(parsed["task"]["task_id"], json!(work_id));
        assert!(
            parsed["task"]["output"]
                .as_str()
                .unwrap_or_default()
                .contains("task-output-probe"),
            "{parsed}"
        );
        let display = TaskOutputTool
            .result_display_for(&json!({}), &out)
            .expect("task_output display");
        assert_eq!(display["kind"], json!("task_output"));
        assert_eq!(display["retrievalStatus"], json!("success"));
        assert_eq!(display["taskStatus"], json!("completed"));
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn blocking_read_of_a_still_running_task_reports_timeout_not_success() {
        let dir = temp_dir("timeout");
        let registry = self::registry(&dir);
        let long = if cfg!(windows) {
            "ping -n 20 127.0.0.1"
        } else {
            "sleep 20"
        };
        let work_id = start(&registry, long, &dir);
        let out = TaskOutputTool
            .execute(
                &json!({"task_id": work_id, "block": "true", "timeout": 200}),
                &ctx(&dir, Some(Arc::clone(&registry))),
            )
            .expect("blocking read returns at the deadline");
        let parsed: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(parsed["retrieval_status"], json!("timeout"));
        assert_eq!(parsed["task"]["status"], json!("running"));
        // A blocking read leaves the process running where it is: clean up via stop, otherwise the test would leave a live process behind.
        assert!(registry.mark_cancelled(&work_id));
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn stop_kills_the_task_then_a_second_stop_says_it_is_not_running() {
        let dir = temp_dir("stop");
        let registry = self::registry(&dir);
        let long = if cfg!(windows) {
            "ping -n 20 127.0.0.1"
        } else {
            "sleep 20"
        };
        let work_id = start(&registry, long, &dir);
        let out = TaskStopTool
            .execute(
                &json!({"task_id": work_id}),
                &ctx(&dir, Some(Arc::clone(&registry))),
            )
            .expect("stop");
        let parsed: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(parsed["task_id"], json!(work_id));
        assert_eq!(parsed["task_type"], json!("local_bash"));
        assert!(
            parsed["message"]
                .as_str()
                .unwrap_or_default()
                .starts_with("Successfully stopped task:"),
            "{parsed}"
        );
        assert_eq!(registry.get(&work_id).unwrap().status, "cancelled");
        let again = TaskStopTool
            .execute(
                &json!({"task_id": work_id}),
                &ctx(&dir, Some(Arc::clone(&registry))),
            )
            .expect_err("a stopped task is not running");
        assert!(
            again.message.contains("is not running (status: cancelled)"),
            "{again:?}"
        );
        let display = TaskStopTool
            .result_display_for(&json!({}), &out)
            .expect("task_stop display");
        assert_eq!(display["kind"], json!("task_stop"));
        assert_eq!(display["taskId"], json!(work_id));
        assert_eq!(
            display["message"],
            json!(format!("Successfully stopped task: {work_id}")),
            "the compact message must not repeat the command that already has its own field"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn stop_accepts_the_legacy_shell_id_and_rejects_no_id_at_all() {
        // The declaration surface must pin this too: TS advertises shell_id to the model; if the runtime accepts it but the schema does not declare it,
        // the model will never use it (this is exactly the kind of inconsistency check-tool-schemas reconciles).
        let contract = TaskStopTool.contract();
        assert!(
            contract
                .input_schema
                .get("properties")
                .and_then(|properties| properties.get("shell_id"))
                .is_some(),
            "TaskStop must advertise shell_id: {}",
            contract.input_schema
        );
        let dir = temp_dir("aliases");
        let registry = self::registry(&dir);
        let work_id = start(&registry, "echo x", &dir);
        let out = TaskStopTool
            .execute(
                &json!({"shell_id": work_id}),
                &ctx(&dir, Some(Arc::clone(&registry))),
            )
            .expect("shell_id is the KillShell alias");
        assert!(out.contains("Successfully stopped task"), "{out}");
        let missing = TaskStopTool
            .execute(&json!({}), &ctx(&dir, Some(registry)))
            .expect_err("no id at all");
        assert_eq!(missing.code, ToolErrorCode::InvalidInput);
        assert_eq!(missing.message, "Missing required parameter: task_id");
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn task_stop_needs_no_approval_and_no_full_access_yet_plan_mode_still_denies_it() {
        // What this assertion pins is the gate's ordering, not the tool logic: TS gives TaskStop
        // `alwaysAllowPatternSources:["toolName"]` + `needsApproval:false`, but **no**
        // `allowedInPlanMode`, so plan mode still blocks it.
        let dir = temp_dir("gate");
        let registry = crate::tools::ToolRegistry::with_defaults();
        let tool = registry.get("TaskStop").expect("TaskStop registered");
        let contract = tool.contract();
        let mut headless = ctx(&dir, None);
        headless.full_access = false;
        headless.interactive = false;
        assert_eq!(
            registry.authorize_contract(
                &contract,
                false,
                &headless,
                false,
                tool.always_allowed_by_name(),
                tool.always_requires_approval(),
            ),
            crate::tools::Authorization::Allow,
            "stopping a task the model started must not open a second approval"
        );
        // The same gate without the ask-exemption (Write is read_only:false and not exempt) → fail-closed.
        let write = registry.get("Write").expect("Write registered");
        let write_contract = write.contract();
        assert!(
            matches!(
                registry.authorize_contract(
                    &write_contract,
                    false,
                    &headless,
                    false,
                    write.always_allowed_by_name(),
                    write.always_requires_approval(),
                ),
                crate::tools::Authorization::Deny(_)
            ),
            "the exemption is per-tool, not a blanket bypass"
        );
        // plan mode: TaskStop goes back to being blocked (the gate is ordered before the ask-exemption).
        let mut plan = ctx(&dir, None);
        plan.full_access = false;
        plan.plan_mode = true;
        assert!(matches!(
            registry.authorize_contract(
                &contract,
                false,
                &plan,
                false,
                tool.always_allowed_by_name(),
                tool.always_requires_approval(),
            ),
            crate::tools::Authorization::Deny(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn display_is_absent_when_the_result_is_not_the_tools_own_shape() {
        assert!(TaskOutputTool
            .result_display_for(&json!({}), "not json")
            .is_none());
        assert!(TaskStopTool
            .result_display_for(&json!({}), &json!({"message": "x"}).to_string())
            .is_none());
    }
    #[test]
    fn task_output_declares_itself_read_only_so_plan_mode_allows_it() {
        let registry = crate::tools::ToolRegistry::with_defaults();
        let tool = registry.get("TaskOutput").expect("TaskOutput registered");
        assert!(
            tool.contract().read_only,
            "reading a background task is not a write"
        );
        assert!(
            !tool.always_allowed_by_name(),
            "TaskOutput needs no exemption: read-only already passes the gate"
        );
    }
}
