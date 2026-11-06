//! Display construction for the observation-class workflow tools (port of the saved-workflow-list
//! and resume sections of `workflow-observation-display.ts`).
//!
//! display does not go through the tool-result budget, so every free-text field is length-bounded here independently; the
//! truncation semantics match TS `boundDisplayText` (UTF-8 bytes + code-point prefix + a `\n...[truncated]` tail).
//! The display of the remaining observation tools (get-run / eval-snippet / list-models) is not ported yet; they are
//! recorded in the difference table in `tools/workflow/mod.rs`.

use serde_json::{json, Map, Value};

/// The same value as `WORKFLOW_OBSERVATION_DISPLAY_MAX_META_CHARS`.
const META_MAX_BYTES: usize = 2_048;
/// The same value as `WORKFLOW_OBSERVATION_DISPLAY_MAX_RUNS` (the saved list reuses this bound).
const MAX_SAVED_WORKFLOWS: usize = 50;
const TRUNCATION_SUFFIX: &str = "\n...[truncated]";

/// The port of TS `boundDisplayText`: the bound is decided in UTF-8 bytes, and when truncating it keeps the longest valid code-point
/// prefix plus a fixed tail (the tail itself also spends budget). It must not be decided by char count or UTF-16 units — Chinese/emoji would drift from TS.
fn bound_display_text(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_string(), false);
    }
    let budget = max_bytes.saturating_sub(TRUNCATION_SUFFIX.len());
    let mut end = 0usize;
    for (index, ch) in value.char_indices() {
        let next = index + ch.len_utf8();
        if next > budget {
            break;
        }
        end = next;
    }
    (format!("{}{TRUNCATION_SUFFIX}", &value[..end]), true)
}

fn bound_meta(value: Option<&str>, truncated: &mut bool) -> Option<String> {
    let value = value?;
    let (bounded, was_truncated) = bound_display_text(value, META_MAX_BYTES);
    if was_truncated {
        *truncated = true;
    }
    Some(bounded)
}

/// The result card of `ListSavedWorkflows`: name/description/whenToUse/scope/path/argNames.
/// `args` keeps only the names (the declared types/defaults belong to the save confirmation window; the list card does not repeat them).
pub fn saved_workflow_list_display(output: &Value) -> Option<Value> {
    let workflows = output.get("workflows")?.as_array()?;
    let mut truncated = workflows.len() > MAX_SAVED_WORKFLOWS;
    let mut entries: Vec<Value> = Vec::new();
    for entry in workflows.iter().take(MAX_SAVED_WORKFLOWS) {
        let Some(object) = entry.as_object() else {
            continue;
        };
        let mut display = Map::new();
        if let Some(name) = object.get("name") {
            display.insert("name".into(), name.clone());
        }
        if let Some(description) = bound_meta(
            object.get("description").and_then(Value::as_str),
            &mut truncated,
        ) {
            display.insert("description".into(), json!(description));
        }
        if let Some(when_to_use) = bound_meta(
            object.get("whenToUse").and_then(Value::as_str),
            &mut truncated,
        ) {
            display.insert("whenToUse".into(), json!(when_to_use));
        }
        if let Some(scope) = object.get("scope") {
            display.insert("scope".into(), scope.clone());
        }
        if let Some(path) = object.get("path") {
            display.insert("path".into(), path.clone());
        }
        let arg_names: Vec<String> = object
            .get("args")
            .and_then(Value::as_object)
            .map(|args| args.keys().cloned().collect())
            .unwrap_or_default();
        display.insert("argNames".into(), json!(arg_names));
        entries.push(Value::Object(display));
    }
    let mut display = Map::new();
    display.insert("kind".into(), json!("saved_workflow_list"));
    display.insert("workflows".into(), Value::Array(entries));
    if let Some(invalid) = output.get("invalid").and_then(Value::as_array) {
        let mut rows = Vec::new();
        for entry in invalid {
            let mut row = Map::new();
            if let Some(path) = entry.get("path") {
                row.insert("path".into(), path.clone());
            }
            if let Some(reason) =
                bound_meta(entry.get("reason").and_then(Value::as_str), &mut truncated)
            {
                row.insert("reason".into(), json!(reason));
            }
            rows.push(Value::Object(row));
        }
        if !rows.is_empty() {
            display.insert("invalid".into(), Value::Array(rows));
        }
    }
    if truncated {
        display.insert("truncated".into(), json!(true));
    }
    Some(Value::Object(display))
}

/// The result card of `ResumeWorkflowRun`: the payload is deliberately minimal `{kind, runId}`; the resume card only expresses "which run is in the background".
pub fn resume_workflow_run_display(output: &Value) -> Option<Value> {
    let run_id = output.get("runId")?.as_str()?;
    Some(json!({"kind": "resume_workflow_run", "runId": run_id}))
}

/// The same values as `CREATE_WORKFLOW_DISPLAY_MAX_DIAGNOSTICS` / `..._MAX_MESSAGE_CHARS`.
const MAX_DISPLAY_DIAGNOSTICS: usize = 100;
const MAX_DIAGNOSTIC_MESSAGE_UNITS: usize = 2_048;

/// The UTF-16 code-unit truncation of TS `String.prototype.slice`. When it cuts inside a surrogate pair it steps back one code unit (Rust/serde_json
/// cannot carry an unpaired surrogate; TS's JSON serialization escapes it as `\uD8xx`, and the rendering side never draws half a character).
fn truncate_utf16(value: &str, max_units: usize) -> String {
    let units: Vec<u16> = value.encode_utf16().collect();
    if units.len() <= max_units {
        return value.to_string();
    }
    let mut end = max_units;
    if end > 0 && (0xD800..=0xDBFF).contains(&units[end - 1]) {
        end -= 1;
    }
    String::from_utf16_lossy(&units[..end])
}

/// The result card of `CreateWorkflow` / `AmendWorkflow` (the two launch tools share one kind): the diagnostic count and a
/// single message are length-bounded independently (display does not pass the tool-result budget), and `causalityGraph` is already bounded at the output boundary,
/// so it is passed straight through. `truncated` only means the diagnostic **count** was cut — truncating a message does not raise this flag (same as TS).
pub fn create_workflow_display(output: &Value) -> Option<Value> {
    let ok = output.get("ok")?.as_bool()?;
    let diagnostics = output.get("diagnostics")?.as_array()?;
    let error_count = diagnostics.len();
    let bounded: Vec<Value> = diagnostics
        .iter()
        .take(MAX_DISPLAY_DIAGNOSTICS)
        .map(|diagnostic| {
            let mut row = Map::new();
            if let Some(line) = diagnostic.get("line") {
                row.insert("line".into(), line.clone());
            }
            if let Some(column) = diagnostic.get("column") {
                row.insert("column".into(), column.clone());
            }
            if let Some(code) = diagnostic.get("code") {
                row.insert("code".into(), code.clone());
            }
            let message = diagnostic
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            row.insert(
                "message".into(),
                json!(truncate_utf16(message, MAX_DIAGNOSTIC_MESSAGE_UNITS)),
            );
            Value::Object(row)
        })
        .collect();
    let mut display = json!({
        "kind": "create_workflow",
        "ok": ok,
        "errorCount": error_count,
        "diagnostics": bounded,
    });
    if let Some(graph) = output
        .get("causalityGraph")
        .filter(|value| !value.is_null())
    {
        display["causalityGraph"] = graph.clone();
    }
    if error_count > MAX_DISPLAY_DIAGNOSTICS {
        display["truncated"] = json!(true);
    }
    Some(display)
}

/// The same values as `WORKFLOW_OBSERVATION_DISPLAY_MAX_LOG_ENTRIES` / `..._MAX_LOG_CHARS` /
/// `..._MAX_RESULT_CHARS`.
const MAX_DISPLAY_LOGS: usize = 40;
const MAX_LOG_BYTES: usize = 1_024;
const MAX_RESULT_BYTES: usize = 4_000;

fn bounded_diagnostic_rows(diagnostics: &[Value], truncated: &mut bool) -> Vec<Value> {
    if diagnostics.len() > MAX_DISPLAY_DIAGNOSTICS {
        *truncated = true;
    }
    diagnostics
        .iter()
        .take(MAX_DISPLAY_DIAGNOSTICS)
        .map(|diagnostic| {
            let mut row = Map::new();
            if let Some(line) = diagnostic.get("line") {
                row.insert("line".into(), line.clone());
            }
            if let Some(column) = diagnostic.get("column") {
                row.insert("column".into(), column.clone());
            }
            if let Some(code) = diagnostic.get("code") {
                row.insert("code".into(), code.clone());
            }
            let message = diagnostic
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            row.insert(
                "message".into(),
                json!(truncate_utf16(message, MAX_DIAGNOSTIC_MESSAGE_UNITS)),
            );
            Value::Object(row)
        })
        .collect()
}

/// The result card of `EvalWorkflowSnippet`: the diagnostics area shares Create's bound; logs take the **tail** (newest at the tail),
/// each entry length-bounded in UTF-8 bytes; response is length-bounded independently; any cut anywhere raises `truncated`.
pub fn eval_workflow_snippet_display(output: &Value) -> Option<Value> {
    let ok = output.get("ok")?.as_bool()?;
    let diagnostics = output.get("diagnostics")?.as_array()?;
    let mut truncated = false;
    let rows = bounded_diagnostic_rows(diagnostics, &mut truncated);

    let raw_logs = output.get("logs")?.as_array()?;
    let dropped = raw_logs.len().saturating_sub(MAX_DISPLAY_LOGS);
    if dropped > 0 {
        truncated = true;
    }
    let logs: Vec<Value> = raw_logs
        .iter()
        .skip(dropped)
        .map(|entry| {
            let text = entry.as_str().unwrap_or_default();
            let (bounded, was_truncated) = bound_display_text(text, MAX_LOG_BYTES);
            if was_truncated {
                truncated = true;
            }
            json!(bounded)
        })
        .collect();

    let (response, response_truncated) = bound_display_text(
        output
            .get("response")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        MAX_RESULT_BYTES,
    );
    if response_truncated {
        truncated = true;
    }

    let mut display = json!({
        "kind": "eval_workflow_snippet",
        "ok": ok,
        "diagnostics": rows,
        "logs": logs,
        "response": response,
        "durationMs": output.get("durationMs").cloned().unwrap_or(Value::Null),
    });
    if truncated {
        display["truncated"] = json!(true);
    }
    Some(display)
}

/// The same values as `WORKFLOW_OBSERVATION_DISPLAY_MAX_ACTORS` / `..._MAX_SUBAGENTS` / `..._MAX_PHASES`.
const MAX_DISPLAY_ACTORS: usize = 32;
const MAX_DISPLAY_SUBAGENTS: usize = 64;
const MAX_DISPLAY_PHASES: usize = 32;

/// The tool-surface nested subagents → flat card rows (TS `toDisplaySubagent`). The wait reason's free text does not go on the card,
/// only cause/retryAfterMs/since are kept; every other field stays absent when absent.
fn display_subagent(row: &Value) -> Value {
    let mut out = Map::new();
    for key in ["siteId", "ordinal", "name", "state", "phaseName"] {
        if let Some(value) = row.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
    if let Some(ask) = row.get("currentAsk").and_then(Value::as_object) {
        for key in [
            "instructionsHead",
            "startedAt",
            "turn",
            "toolCalls",
            "lastTool",
        ] {
            if let Some(value) = ask.get(key) {
                out.insert(key.into(), value.clone());
            }
        }
    }
    if let Some(wait) = row.get("wait").and_then(Value::as_object) {
        if let Some(value) = wait.get("cause") {
            out.insert("waitCause".into(), value.clone());
        }
        if let Some(value) = wait.get("retryAfterMs") {
            out.insert("retryAfterMs".into(), value.clone());
        }
        if let Some(value) = wait.get("since") {
            out.insert("waitSince".into(), value.clone());
        }
    }
    for key in [
        "parkedOn",
        "stepsSettled",
        "stepsFailed",
        "tokens",
        "lastProgressAt",
    ] {
        if let Some(value) = row.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
    Value::Object(out)
}

/// The result card of `GetWorkflowRun`. Each situation slice has its own bound (display does not pass the tool-result budget): actors 32,
/// logTail takes the tail 40 entries (1024 UTF-8 bytes each), subagents 64, result 4000 bytes; any of them being cut
/// is folded into a single `truncated`. Errors carry only code/message (providerStop and similar details in the output do not go on the card).
pub fn get_workflow_run_display(output: &Value) -> Option<Value> {
    let object = output.as_object()?;
    let run_id = object.get("runId")?.as_str()?;
    let label = object.get("label")?.as_str()?;
    let status = object.get("status")?.as_str()?;
    let summary = object.get("summary")?.as_str()?;
    let generated_at = object.get("generatedAt")?.clone();
    let usage = object.get("usage")?.clone();
    let health = object.get("health")?.clone();

    let mut truncated = false;
    let raw_actors = object.get("actors")?.as_array()?;
    if raw_actors.len() > MAX_DISPLAY_ACTORS {
        truncated = true;
    }
    let actors: Vec<Value> = raw_actors
        .iter()
        .take(MAX_DISPLAY_ACTORS)
        .cloned()
        .collect();

    let raw_logs = object.get("logTail")?.as_array()?;
    let dropped = raw_logs.len().saturating_sub(MAX_DISPLAY_LOGS);
    if dropped > 0 {
        truncated = true;
    }
    let log_tail: Vec<Value> = raw_logs
        .iter()
        .skip(dropped)
        .map(|entry| {
            let mut row = Map::new();
            if let Some(sequence) = entry.get("sequence") {
                row.insert("sequence".into(), sequence.clone());
            }
            let (message, was_truncated) = bound_display_text(
                entry
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                MAX_LOG_BYTES,
            );
            if was_truncated {
                truncated = true;
            }
            row.insert("message".into(), json!(message));
            if let Some(at) = entry.get("at") {
                row.insert("at".into(), at.clone());
            }
            Value::Object(row)
        })
        .collect();

    let raw_subagents = object.get("subagents")?.as_array()?;
    if raw_subagents.len() > MAX_DISPLAY_SUBAGENTS
        || object.get("subagentsTruncated") == Some(&json!(true))
    {
        truncated = true;
    }
    let subagents: Vec<Value> = raw_subagents
        .iter()
        .take(MAX_DISPLAY_SUBAGENTS)
        .map(display_subagent)
        .collect();

    let phases = object
        .get("phases")
        .and_then(Value::as_array)
        .map(|phases| {
            if phases.len() > MAX_DISPLAY_PHASES {
                truncated = true;
            }
            phases
                .iter()
                .take(MAX_DISPLAY_PHASES)
                .cloned()
                .collect::<Vec<_>>()
        });

    let result = object.get("result").and_then(Value::as_str).map(|result| {
        let (bounded, was_truncated) = bound_display_text(result, MAX_RESULT_BYTES);
        if was_truncated {
            truncated = true;
        }
        bounded
    });

    let error = object.get("error").and_then(Value::as_object).map(|error| {
        json!({
            "code": error.get("code").cloned().unwrap_or(Value::Null),
            "message": error.get("message").cloned().unwrap_or(Value::Null),
        })
    });

    let mut display = json!({
        "kind": "get_workflow_run",
        "runId": run_id,
        "label": label,
        "status": status,
        "summary": summary,
        "generatedAt": generated_at,
        "usage": usage,
        "subagents": subagents,
        "health": health,
        "actors": actors,
        "logTail": log_tail,
    });
    if let Some(stop_reason) = object.get("stopReason") {
        display["stopReason"] = stop_reason.clone();
    }
    if object.get("possiblyInterrupted") == Some(&json!(true)) {
        display["possiblyInterrupted"] = json!(true);
    }
    if let Some(phases) = phases.filter(|phases| !phases.is_empty()) {
        display["phases"] = Value::Array(phases);
    }
    if let Some(result) = result {
        display["result"] = json!(result);
    }
    if let Some(error) = error {
        display["error"] = error;
    }
    if truncated {
        display["truncated"] = json!(true);
    }
    Some(display)
}

/// The same value as `WORKFLOW_OBSERVATION_DISPLAY_MAX_MODELS`.
const MAX_DISPLAY_MODELS: usize = 100;

/// The result card of `ListModels`: catalogue rows are passed through as they are; providerLabel/disabledReason are free text and go
/// through the meta byte bound. The row count is capped at 100, and cutting only raises `truncated` — no numbers are invented on the card.
pub fn list_models_display(output: &Value) -> Option<Value> {
    let raw_models = output.get("models")?.as_array()?;
    let mut truncated = raw_models.len() > MAX_DISPLAY_MODELS;
    let models: Vec<Value> = raw_models
        .iter()
        .take(MAX_DISPLAY_MODELS)
        .map(|model| {
            let mut row = Map::new();
            for key in ["id", "providerId", "modelId"] {
                if let Some(value) = model.get(key) {
                    row.insert(key.into(), value.clone());
                }
            }
            if let Some(label) = bound_meta(
                model.get("providerLabel").and_then(Value::as_str),
                &mut truncated,
            ) {
                row.insert("providerLabel".into(), json!(label));
            }
            if let Some(levels) = model.get("reasoningLevels") {
                row.insert("reasoningLevels".into(), levels.clone());
            }
            if let Some(level) = model.get("defaultReasoningLevel") {
                row.insert("defaultReasoningLevel".into(), level.clone());
            }
            if let Some(window) = model.get("contextWindow") {
                row.insert("contextWindow".into(), window.clone());
            }
            if let Some(reason) = bound_meta(
                model.get("disabledReason").and_then(Value::as_str),
                &mut truncated,
            ) {
                row.insert("disabledReason".into(), json!(reason));
            }
            Value::Object(row)
        })
        .collect();
    let mut display = json!({"kind": "list_models", "models": models});
    if let Some(current) = output.get("current").filter(|value| !value.is_null()) {
        display["current"] = current.clone();
    }
    if truncated {
        display["truncated"] = json!(true);
    }
    Some(display)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_display_text_matches_the_ts_byte_and_suffix_semantics() {
        assert_eq!(
            bound_display_text("short", 32),
            ("short".to_string(), false)
        );
        let (value, truncated) = bound_display_text("abcdefghijklmnopqrstuvwxyz", 20);
        assert!(truncated);
        assert!(value.ends_with("\n...[truncated]"));
        assert!(value.len() <= 20);
        // Multibyte characters are cut on a code-point prefix: only the complete code points that fit are kept (never half a UTF-8 sequence).
        let (value, _) = bound_display_text("中中中中", 20);
        assert!(value.starts_with('中'));
        assert!(std::str::from_utf8(value.as_bytes()).is_ok());
    }

    #[test]
    fn saved_list_display_maps_metas_args_and_invalid_rows() {
        let output = json!({
            "workflows": [{
                "name": "linear",
                "description": "Runs the linear fixture",
                "whenToUse": "when testing",
                "scope": "project",
                "path": ".zcode/workflows/linear.md",
                "args": {"goal": {"type": "string"}, "count": {"type": "number"}}
            }],
            "invalid": [{"path": "broken.md", "reason": "bad frontmatter"}]
        });
        let display = saved_workflow_list_display(&output).expect("display");
        assert_eq!(display["kind"], json!("saved_workflow_list"));
        assert_eq!(display["workflows"][0]["name"], json!("linear")); // TS `Object.keys` keeps declaration order; display does not reorder (the schema does not require an order).
        assert_eq!(
            display["workflows"][0]["argNames"],
            json!(["goal", "count"])
        );
        assert_eq!(display["invalid"][0]["path"], json!("broken.md"));
        assert!(display.get("truncated").is_none());

        let long = "x".repeat(4_000);
        let truncated = saved_workflow_list_display(&json!({
            "workflows": [{
                "name": "long", "description": long, "scope": "user", "path": "p"
            }]
        }))
        .expect("display");
        assert_eq!(truncated["truncated"], json!(true));
        assert!(truncated["workflows"][0]["description"]
            .as_str()
            .is_some_and(|text| text.ends_with("\n...[truncated]")));
    }

    #[test]
    fn create_display_bounds_diagnostics_and_passes_the_graph() {
        let graph = json!({
            "steps": [{"id": "s1", "lane": "workspace"}],
            "lanes": [{"id": "workspace"}],
            "participants": [],
            "handoffs": []
        });
        let output = json!({
            "ok": true,
            "diagnostics": [{"line": 1, "column": 2, "code": "X", "message": "m"}],
            "causalityGraph": graph
        });
        let display = create_workflow_display(&output).expect("display");
        assert_eq!(display["kind"], json!("create_workflow"));
        assert_eq!(display["ok"], json!(true));
        assert_eq!(display["errorCount"], json!(1));
        assert_eq!(display["diagnostics"][0]["code"], json!("X"));
        assert_eq!(display["causalityGraph"], graph);
        assert!(display.get("truncated").is_none());

        let diagnostics: Vec<Value> = (0..101)
            .map(|index| {
                json!({"line": 1, "column": 1, "code": "X", "message": format!("m{index}")})
            })
            .collect();
        let display = create_workflow_display(&json!({"ok": false, "diagnostics": diagnostics}))
            .expect("display");
        assert_eq!(display["errorCount"], json!(101));
        assert_eq!(display["diagnostics"].as_array().expect("kept").len(), 100);
        assert_eq!(display["truncated"], json!(true));
        assert!(display.get("causalityGraph").is_none());

        let long = "x".repeat(3_000);
        let display = create_workflow_display(&json!({
            "ok": true,
            "diagnostics": [{"line": 1, "column": 1, "code": "X", "message": long}]
        }))
        .expect("display");
        assert_eq!(
            display["diagnostics"][0]["message"]
                .as_str()
                .expect("message")
                .chars()
                .count(),
            2_048
        );
        // Truncating a single message does not raise `truncated` (only a cut count does), same as TS.
        assert!(display.get("truncated").is_none());
    }

    #[test]
    fn eval_display_bounds_logs_response_and_diagnostics() {
        let logs: Vec<Value> = (0..45).map(|index| json!(format!("log{index}"))).collect();
        let output = json!({
            "ok": true,
            "diagnostics": [],
            "logs": logs,
            "response": "done",
            "durationMs": 12
        });
        let display = eval_workflow_snippet_display(&output).expect("display");
        assert_eq!(display["kind"], json!("eval_workflow_snippet"));
        assert_eq!(display["logs"].as_array().expect("logs").len(), 40);
        assert_eq!(display["logs"][0], json!("log5"));
        assert_eq!(display["truncated"], json!(true));

        let display = eval_workflow_snippet_display(&json!({
            "ok": false,
            "diagnostics": [{"line": 1, "column": 1, "code": "X", "message": "bad"}],
            "logs": [],
            "response": "The snippet has errors.",
            "durationMs": 3
        }))
        .expect("display");
        assert_eq!(display["ok"], json!(false));
        assert_eq!(display["diagnostics"][0]["code"], json!("X"));
        assert!(display.get("truncated").is_none());
    }

    #[test]
    fn get_run_display_keeps_the_card_fields_and_flattens_subagents() {
        let output = json!({
            "runId": "dwfrun-1",
            "label": "linear",
            "status": "completed",
            "summary": "one ask and a report",
            "generatedAt": 1_700_000_000_000.0,
            "usage": {"spentTokens": 35, "nodesObserved": 2},
            "health": {"ok": true},
            "actors": [{"siteId": "actor#1", "ordinal": 1, "status": "completed"}],
            "logTail": [{"sequence": 4, "message": "hello", "at": 1}],
            "subagents": [{
                "siteId": "actor#1", "ordinal": 1, "state": "done",
                "currentAsk": {"instructionsHead": "do it", "turn": 1, "toolCalls": 2},
                "stepsSettled": 1, "stepsFailed": 0, "tokens": 12,
                "lastProgressAt": 2
            }],
            "result": "done",
            "error": {"code": "X", "message": "m", "providerStop": {"secret": true}}
        });
        let display = get_workflow_run_display(&output).expect("display");
        assert_eq!(display["kind"], json!("get_workflow_run"));
        assert_eq!(display["runId"], json!("dwfrun-1"));
        assert_eq!(display["subagents"][0]["instructionsHead"], json!("do it"));
        assert_eq!(display["subagents"][0]["stepsSettled"], json!(1));
        assert_eq!(display["logTail"][0]["message"], json!("hello"));
        assert_eq!(display["result"], json!("done"));
        assert_eq!(display["error"], json!({"code": "X", "message": "m"}));
        assert!(display["error"].get("providerStop").is_none());
        assert!(display.get("truncated").is_none());
    }

    #[test]
    fn list_models_display_passes_rows_and_bounds_meta() {
        let output = json!({
            "current": "anthropic/main",
            "models": [{
                "id": "anthropic/main",
                "providerId": "anthropic",
                "modelId": "main",
                "providerLabel": "Anthropic",
                "reasoningLevels": ["low", "high"],
                "defaultReasoningLevel": "low",
                "contextWindow": 200_000
            }]
        });
        let display = list_models_display(&output).expect("display");
        assert_eq!(display["kind"], json!("list_models"));
        assert_eq!(display["current"], json!("anthropic/main"));
        assert_eq!(
            display["models"][0]["reasoningLevels"],
            json!(["low", "high"])
        );
        assert_eq!(display["models"][0]["providerLabel"], json!("Anthropic"));
        assert!(display.get("truncated").is_none());
        assert!(list_models_display(&json!({})).is_none());
    }

    #[test]
    fn resume_display_is_a_minimal_run_id_card() {
        assert_eq!(
            resume_workflow_run_display(&json!({"ok": true, "runId": "dwfrun-1"}))
                .expect("display"),
            json!({"kind": "resume_workflow_run", "runId": "dwfrun-1"})
        );
        assert!(resume_workflow_run_display(&json!({"ok": true})).is_none());
    }
}
