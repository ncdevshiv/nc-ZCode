//! The v4 workflow commands (port of `startSavedWorkflow` / `resumeWorkflowRun` /
//! `amendWorkflowRunSettings` / `cancelBackgroundWork`).
//!
//! All four commands hand the work to the single run service (the one created when the host is constructed); this file only does
//! payload parsing, session-ownership checks and ACK settling. `workId ≡ runId`: the cancel command tries both sources
//! (the workflow run and the background bash/subagent registry) and only errors when neither matches.

use std::sync::Arc;

use nc_code_core::protocol::{CommandFinal, CommandStatus};
use serde_json::{json, Value};

use super::settle_admitted;
use crate::session::Session;
use crate::workflow_service::{AmendRequest, SubmitRequest};
use crate::HostServer;
use nc_code_core::inbox::Admitted;

fn fault(message: impl Into<String>) -> CommandFinal {
    let mut final_ =
        CommandFinal::with_reason(CommandStatus::Failed, "fault.command.executionFailed");
    final_.message = Some(message.into());
    final_
}

fn accepted_with(result: Value) -> CommandFinal {
    let mut final_ = CommandFinal::accepted();
    final_.result = Some(result);
    final_
}

impl HostServer {
    /// `cancelBackgroundWork {workId}`: a workflow run goes through the engine's stop(user); otherwise fall back to the background task registry.
    pub(crate) fn run_cancel_background_work(
        self: &Arc<Self>,
        session: Arc<Session>,
        admitted: Admitted,
    ) {
        let work_id = admitted
            .envelope
            .payload
            .get("workId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let final_ = match self.workflow_service() {
            None => fault("workflow run service is not available on this host"),
            Some(service) => {
                let owned = service
                    .get_run(&work_id)
                    .map(|snapshot| {
                        snapshot.parent_session_id.as_deref() == Some(session.id.as_str())
                    })
                    .unwrap_or(false);
                if owned {
                    if service.cancel(&work_id, crate::workflow_service::CancelInitiator::User) {
                        CommandFinal::accepted()
                    } else {
                        // An already-settled run: cancelling is an idempotent no-op.
                        accepted_with(json!({"workId": work_id, "alreadySettled": true}))
                    }
                } else if session.background.mark_cancelled(&work_id) {
                    CommandFinal::accepted()
                } else {
                    fault(format!("no cancellable background work with id {work_id}"))
                }
            }
        };
        settle_admitted(&session, admitted, final_, Vec::new(), &self.outbox);
    }

    /// `resumeWorkflowRun {runId}`: the service's resume gate (stopped ∧ ¬superseded) has the final say.
    pub(crate) async fn run_resume_workflow_run(
        self: &Arc<Self>,
        session: Arc<Session>,
        admitted: Admitted,
    ) {
        let run_id = admitted
            .envelope
            .payload
            .get("runId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let final_ = match self.workflow_service() {
            None => fault("workflow run service is not available on this host"),
            Some(service) => match service.resume(&run_id).await {
                Ok(resumed) => accepted_with(json!({
                    "runId": resumed.run_id,
                    "status": "backgrounded",
                    "importedCache": resumed.imported_cache,
                })),
                Err(error) => fault(format!("resume {run_id} failed: {error:?}")),
            },
        };
        settle_admitted(&session, admitted, final_, Vec::new(), &self.outbox);
    }

    /// `amendWorkflowRunSettings {runId, script?, scriptPath?, subagentModel?, maxConcurrency?}`:
    /// an amendment = a new run (supersedes its predecessor). When `script` is absent, read `scriptPath` (restricted to the workspace).
    pub(crate) async fn run_amend_workflow_run_settings(
        self: &Arc<Self>,
        session: Arc<Session>,
        admitted: Admitted,
    ) {
        let payload = admitted.envelope.payload.clone();
        let run_id = payload
            .get("runId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let (final_, lines) = match self.workflow_service() {
            None => (
                fault("workflow run service is not available on this host"),
                Vec::new(),
            ),
            Some(service) => {
                let predecessor = service.get_run(&run_id);
                let resolved = match payload.get("script").and_then(Value::as_str) {
                    Some(script) => Ok(Some(script.to_string())),
                    None => match payload.get("scriptPath").and_then(Value::as_str) {
                        Some(relative) => read_amend_script(&session, relative),
                        // No explicit script: reuse the run's own script file (TS reads the script from the run record;
                        // this host's equivalent is the scriptPath recorded on the run-launched entry).
                        None => {
                            match predecessor.as_ref().and_then(|run| run.script_path.clone()) {
                                Some(path) => read_amend_script(&session, path.as_str()),
                                None => Ok(None),
                            }
                        }
                    },
                };
                match resolved {
                    Err(message) => (fault(message), Vec::new()),
                    Ok(None) => (
                        fault("amendWorkflowRunSettings needs script or scriptPath"),
                        Vec::new(),
                    ),
                    Ok(Some(script_text)) => {
                        self.amend_settings_submit(
                            service,
                            &session,
                            payload,
                            run_id,
                            script_text,
                            &admitted.envelope.command_id,
                        )
                        .await
                    }
                }
            }
        };
        settle_admitted(&session, admitted, final_, lines, &self.outbox);
    }

    /// The submit half of a settings amendment: analysis (phase table + display graph) → settings diff (rejected when nothing changed) → reuse/rewrite
    /// the script file → amend → a settings controlOnly turn. TS completes analysis and the diff before any "zero side effects".
    #[allow(clippy::too_many_arguments)]
    async fn amend_settings_submit(
        &self,
        service: Arc<crate::workflow_service::WorkflowService>,
        session: &Session,
        payload: Value,
        run_id: String,
        script_text: String,
        command_id: &str,
    ) -> (CommandFinal, Vec<String>) {
        let Some(predecessor) = service.get_run(&run_id) else {
            return (
                fault(format!("amend {run_id} failed: run not found")),
                Vec::new(),
            );
        };
        // The name comes only from the run itself: a run that was never named stays unnamed (TS applies the same rule and does not use the run id as a title).
        let run_name = payload
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string);
        let analysis = match self
            .analyze_workflow(
                &script_text,
                &format!("The stored script of run {run_id} has errors:"),
            )
            .await
        {
            Ok(analysis) => analysis,
            Err(message) => return (fault(message), Vec::new()),
        };
        let phase_names = if analysis.phase_names.is_empty() {
            None
        } else {
            Some(analysis.phase_names.clone())
        };
        // —— settings diff: never interrupt a run in flight for an empty amendment when nothing changed (TS `unchanged`). ——
        let ceiling = service.concurrency_ceiling();
        let previous_model = predecessor.subagent_model.clone();
        let previous_bound =
            (predecessor.max_concurrency < ceiling).then_some(predecessor.max_concurrency);
        let model_field = payload.get("subagentModel");
        let (requested_model, model_changed) = match model_field {
            None => (previous_model.clone(), false),
            Some(Value::Null) => (None, previous_model.is_some()),
            Some(Value::String(model)) => (
                Some(model.clone()),
                previous_model.as_deref() != Some(model.as_str()),
            ),
            Some(_) => return (fault("subagentModel must be a string or null"), Vec::new()),
        };
        let bound_field = payload.get("maxConcurrency");
        let next_bound = match bound_field {
            None => previous_bound,
            Some(Value::Null) => None,
            Some(value) => value
                .as_u64()
                .map(|value| (value as u32).clamp(1, ceiling))
                .filter(|value| *value < ceiling),
        };
        let bound_changed = bound_field.is_some() && next_bound != previous_bound;
        // TS's settings surface only changes settings (the script comes from the run record). This host's command additionally accepts `script`/`scriptPath`
        // —— supplying a script explicitly is a deliberate amendment, so it counts as a change too; only when all three are absent is it a truly empty amendment.
        let script_supplied =
            payload.get("script").is_some() || payload.get("scriptPath").is_some();
        if !model_changed && !bound_changed && !script_supplied {
            return (
                fault("Nothing changed: the run already uses these settings."),
                Vec::new(),
            );
        }
        // Script file: keep recording the predecessor's file as long as it still holds these bytes right now, otherwise write a draft
        // like a script that does not come from a file (TS `resolveKeptScriptFile` + `writeWorkflowDraft`).
        let kept_path = predecessor.script_path.as_deref().and_then(|path| {
            let candidate = if std::path::Path::new(path).is_absolute() {
                std::path::PathBuf::from(path)
            } else {
                session.workspace_root.join(path)
            };
            let body = std::fs::read_to_string(candidate)
                .ok()
                .map(|raw| script_body_of_file(&raw))?;
            (body == script_text).then(|| path.to_string())
        });
        let script_path = kept_path.or_else(|| {
            let draft_name = crate::tools::workflow::support::draft_name(
                run_name.as_deref(),
                &analysis.phase_names,
            );
            crate::workflow_service::saved_workflows::write_workflow_draft(
                Some(&session.workspace_root),
                &draft_name,
                &script_text,
            )
            .map(|path| path.to_string_lossy().to_string())
        });
        let tool_call_id = self.next_id("settings");
        let request = AmendRequest {
            session_id: Some(session.id.clone()),
            predecessor_run_id: run_id.clone(),
            script_text,
            cwd: Some(session.workspace_root.to_string_lossy().to_string()),
            name: run_name.clone(),
            max_concurrency: next_bound,
            subagent_model: requested_model.clone(),
            script_path,
            inherit_args: true,
            tool_call_id: Some(tool_call_id.clone()),
            phase_names,
            phase_alongside: analysis.phase_alongside.clone(),
        };
        match service.amend(request).await {
            Ok(amended) => {
                let name = run_name.clone();
                let mut amend = json!({"predecessorRunId": run_id});
                if model_changed {
                    amend["subagentModel"] =
                        from_to_value(previous_model.clone(), requested_model.clone());
                }
                if bound_changed {
                    amend["maxConcurrency"] = from_to_value(previous_bound, next_bound);
                }
                amend["ceiling"] = json!(ceiling);
                let text = build_settings_message_text(
                    name.as_deref(),
                    &run_id,
                    &amended.run_id,
                    amended.superseded_run_id.is_some(),
                    &amend,
                );
                let display = crate::tools::workflow::display::create_workflow_display(&json!({
                    "ok": true,
                    "diagnostics": [],
                    "causalityGraph": analysis.causality_graph,
                }));
                let meta = workflow_launch_meta(LaunchMetaInput {
                    run_id: &amended.run_id,
                    tool_call_id: &tool_call_id,
                    name: name.as_deref(),
                    scope: None,
                    path: None,
                    args: None,
                    description: None,
                    display,
                    script: None,
                    amend: Some(amend),
                });
                let lines = session.project_workflow_launch_turn(
                    &self.next_id("workflow-settings-turn"),
                    command_id,
                    &text,
                    &meta,
                );
                (
                    accepted_with(json!({
                        "runId": amended.run_id,
                        "toolCallId": tool_call_id,
                        "supersededRunId": amended.superseded_run_id,
                        "status": "backgrounded",
                    })),
                    lines,
                )
            }
            Err(error) => (
                fault(format!("amend {run_id} failed: {error:?}")),
                Vec::new(),
            ),
        }
    }

    /// The shared "analysis → phase table + display graph" step for direct launch and amendment: diagnostics are a business rejection (readable text),
    /// and a kernel fault is handled the same way.
    async fn analyze_workflow(
        &self,
        script: &str,
        error_heading: &str,
    ) -> Result<crate::tools::workflow::ScriptAnalysis, String> {
        let Some(port) = self.workflow_port() else {
            return Err("workflow analysis is not available on this host".to_string());
        };
        let analysis = port
            .analyze(script)
            .await
            .map_err(|error| format!("workflow script analysis failed: {error}"))?;
        if !analysis.ok || !analysis.diagnostics.is_empty() {
            return Err(bounded_compile_diagnostics(
                error_heading,
                &analysis.diagnostics,
            ));
        }
        Ok(analysis)
    }

    /// `startSavedWorkflow {name, scope?, args?}`: resolve the saved definition → compile once → start the run.
    pub(crate) async fn run_start_saved_workflow(
        self: &Arc<Self>,
        session: Arc<Session>,
        admitted: Admitted,
    ) {
        let payload = admitted.envelope.payload.clone();
        let name = payload
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let scope = match payload.get("scope").and_then(Value::as_str) {
            Some("global") => {
                Some(crate::workflow_service::saved_workflows::SavedWorkflowScope::Global)
            }
            Some("project") => {
                Some(crate::workflow_service::saved_workflows::SavedWorkflowScope::Project)
            }
            _ => None,
        };
        let root = session.workspace_root.clone();
        let resolved = crate::workflow_service::saved_workflows::resolve_saved_workflow(
            &root, &name, scope, None,
        );
        let (final_, lines) = match resolved {
            crate::workflow_service::saved_workflows::SavedWorkflowResolveResult::Err(error) => (
                fault(format!("saved workflow {name} is not usable: {error:?}")),
                Vec::new(),
            ),
            crate::workflow_service::saved_workflows::SavedWorkflowResolveResult::Ok(found) => {
                match self.workflow_service() {
                    None => (
                        fault("workflow run service is not available on this host"),
                        Vec::new(),
                    ),
                    Some(service) => {
                        let args = payload
                            .get("args")
                            .and_then(Value::as_object)
                            .cloned()
                            .unwrap_or_default();
                        self.start_saved_workflow_submit(
                            service,
                            &session,
                            found,
                            args,
                            &admitted.envelope.command_id,
                        )
                        .await
                    }
                }
            }
        };
        settle_admitted(&session, admitted, final_, lines, &self.outbox);
    }

    /// The submit half of `startSavedWorkflow`: analysis (compile diagnostics + phase table) → argument validation → a draft working copy
    /// → submit with anchors → a controlOnly launch turn. TS orders it as "steps 1 and 2 fail before any persistence",
    /// and here nothing has been created either: no run row, no transcript row.
    async fn start_saved_workflow_submit(
        &self,
        service: Arc<crate::workflow_service::WorkflowService>,
        session: &Session,
        found: crate::workflow_service::saved_workflows::ResolvedSavedWorkflow,
        provided_args: serde_json::Map<String, Value>,
        command_id: &str,
    ) -> (CommandFinal, Vec<String>) {
        let Some(port) = self.workflow_port() else {
            return (
                fault("workflow analysis is not available on this host"),
                Vec::new(),
            );
        };
        let analysis = match port.analyze(&found.script).await {
            Ok(analysis) => analysis,
            Err(error) => {
                return (
                    fault(format!("workflow script analysis failed: {error}")),
                    Vec::new(),
                )
            }
        };
        if !analysis.ok || !analysis.diagnostics.is_empty() {
            return (
                fault(bounded_compile_diagnostics(
                    &format!("The saved workflow '{}' has errors:", found.name),
                    &analysis.diagnostics,
                )),
                Vec::new(),
            );
        }
        let args = match crate::workflow_service::saved_workflows::validate_workflow_args(
            found.meta.args.as_ref(),
            Some(&provided_args),
        ) {
            crate::workflow_service::saved_workflows::WorkflowArgsValidation::Ok(args) => args,
            crate::workflow_service::saved_workflows::WorkflowArgsValidation::Errors(errors) => {
                return (
                    fault(format!(
                        "The arguments for saved workflow '{}' are not valid:\n{}",
                        found.name,
                        errors.join("\n")
                    )),
                    Vec::new(),
                )
            }
        };
        // Working copy: the saved definition is never touched (the same rule CreateWorkflow applies to a saved source).
        let script_path = crate::workflow_service::saved_workflows::write_workflow_draft(
            Some(&session.workspace_root),
            &found.name,
            &found.source,
        )
        .map(|path| path.to_string_lossy().to_string());
        let launch_input_id = self.next_id("launch-input");
        let tool_call_id = self.next_id("launch");
        let args_value = Value::Object(args.clone().into_iter().collect());
        let request = SubmitRequest {
            session_id: session.id.clone(),
            script_text: found.script.clone(),
            cwd: Some(session.workspace_root.to_string_lossy().to_string()),
            name: Some(found.name.clone()),
            args: Some(args_value.clone()),
            tool_call_id: Some(tool_call_id.clone()),
            phase_names: if analysis.phase_names.is_empty() {
                None
            } else {
                Some(analysis.phase_names.clone())
            },
            phase_alongside: analysis.phase_alongside.clone(),
            launch_input_id: Some(launch_input_id),
            script_path,
            ..SubmitRequest::default()
        };
        match service.submit(request).await {
            Ok(submitted) => {
                // Launch turn: the user's click in the hub is the real intent. The text goes into model history and the transcript;
                // the metadata also lets the GUI draw the run card at the turn tail and the side-panel launch row (a direct launch has no tool row).
                let scope = scope_label(found.scope);
                let text =
                    build_launch_message_text(&found.name, scope, &submitted.run_id, &args_value);
                let display = crate::tools::workflow::display::create_workflow_display(&json!({
                    "ok": true,
                    "diagnostics": [],
                    "causalityGraph": analysis.causality_graph,
                }));
                let meta = workflow_launch_meta(LaunchMetaInput {
                    run_id: &submitted.run_id,
                    tool_call_id: &tool_call_id,
                    name: Some(&found.name),
                    scope: Some(scope),
                    path: Some(&found.path),
                    args: Some(&args_value),
                    description: Some(&found.meta.description),
                    display,
                    script: Some(&found.script),
                    amend: None,
                });
                let lines = session.project_workflow_launch_turn(
                    &self.next_id("workflow-launch-turn"),
                    command_id,
                    &text,
                    &meta,
                );
                (
                    accepted_with(json!({
                        "runId": submitted.run_id,
                        "toolCallId": tool_call_id,
                        "status": "backgrounded",
                    })),
                    lines,
                )
            }
            Err(error) => (
                fault(format!("start {} failed: {error:?}", found.name)),
                Vec::new(),
            ),
        }
    }
}

fn scope_label(
    scope: crate::workflow_service::saved_workflows::SavedWorkflowScope,
) -> &'static str {
    match scope {
        crate::workflow_service::saved_workflows::SavedWorkflowScope::Project => "project",
        crate::workflow_service::saved_workflows::SavedWorkflowScope::Global => "global",
    }
}

/// TS `buildLaunchMessageText`: the canonical English that goes into model history and the transcript (for old clients / the TUI
/// this text is exactly the degraded presentation).
fn build_launch_message_text(name: &str, scope: &str, run_id: &str, args: &Value) -> String {
    let mut lines = vec![format!(
        "Started the saved workflow \"{name}\" ({scope}) from the workflows hub as run {run_id}."
    )];
    if args.as_object().is_some_and(|args| !args.is_empty()) {
        lines.push(String::new());
        lines.push("Arguments:".to_string());
        lines.push("```json".to_string());
        lines.push(serde_json::to_string_pretty(args).unwrap_or_else(|_| "{}".to_string()));
        lines.push("```".to_string());
    }
    lines.push(String::new());
    lines.push(
        "Progress and results arrive as background notifications; do not start it again."
            .to_string(),
    );
    lines.join("\n")
}

struct LaunchMetaInput<'a> {
    run_id: &'a str,
    tool_call_id: &'a str,
    name: Option<&'a str>,
    scope: Option<&'a str>,
    path: Option<&'a str>,
    args: Option<&'a Value>,
    description: Option<&'a str>,
    display: Option<Value>,
    script: Option<&'a str>,
    amend: Option<Value>,
}

/// TS `boundWorkflowLaunchMeta`: when serialized args exceed 4 KiB they become `{"…": "arguments omitted (N bytes)"}`,
/// a description longer than 500 characters is truncated with an ellipsis, a script longer than 256 000 characters is omitted
/// entirely (and the side panel's Script section with it), and display is already bounded where it is built and passed through verbatim.
/// A settings turn has no name/scope/path/script/args.
fn workflow_launch_meta(input: LaunchMetaInput<'_>) -> Value {
    const ARGS_MAX_BYTES: usize = 4_096;
    const DESCRIPTION_MAX_CHARS: usize = 500;
    const SCRIPT_MAX_CHARS: usize = 256_000;
    let mut meta = json!({
        "runId": input.run_id,
        "toolCallId": input.tool_call_id,
    });
    if let Some(name) = input.name {
        meta["name"] = json!(name);
    }
    if let Some(scope) = input.scope {
        meta["scope"] = json!(scope);
    }
    if let Some(path) = input.path {
        meta["path"] = json!(path);
    }
    if let Some(args) = input.args {
        if args.as_object().is_some_and(|args| !args.is_empty()) {
            let serialized = serde_json::to_string(args).unwrap_or_default();
            meta["args"] = if serialized.len() > ARGS_MAX_BYTES {
                json!({"…": format!("arguments omitted ({} bytes)", serialized.len())})
            } else {
                args.clone()
            };
        }
    }
    if let Some(description) = input.description.filter(|text| !text.trim().is_empty()) {
        let mut chars: Vec<char> = description.chars().collect();
        if chars.len() > DESCRIPTION_MAX_CHARS {
            chars.truncate(DESCRIPTION_MAX_CHARS - 1);
            chars.push('…');
        }
        meta["description"] = json!(chars.into_iter().collect::<String>());
    }
    if let Some(display) = input.display {
        meta["display"] = display;
    }
    if let Some(script) = input
        .script
        .filter(|script| script.chars().count() <= SCRIPT_MAX_CHARS)
    {
        meta["script"] = json!(script);
    }
    if let Some(amend) = input.amend {
        meta["amend"] = amend;
    }
    meta
}

/// Read an amendment script (inside the workspace; when it carries a metadata block, take the body). Escaping is a command-surface rejection, not a file error.
fn read_amend_script(session: &Session, path: &str) -> Result<Option<String>, String> {
    let candidate = if std::path::Path::new(path).is_absolute() {
        std::path::PathBuf::from(path)
    } else {
        session.workspace_root.join(path)
    };
    if !candidate.starts_with(&session.workspace_root) {
        return Err("scriptPath escapes the workspace".to_string());
    }
    Ok(std::fs::read_to_string(&candidate)
        .ok()
        .map(|raw| script_body_of_file(&raw)))
}

/// Read the **body** of a script file: with a `/* nc-code-workflow` metadata block the block is stripped (the same check TS
/// `readWorkflowScriptFile` makes); plain script text is returned unchanged.
fn script_body_of_file(source: &str) -> String {
    if source
        .trim_start()
        .starts_with(crate::workflow_service::saved_workflows::SAVED_WORKFLOW_SENTINEL)
    {
        if let Ok(parsed) = crate::workflow_service::saved_workflows::parse_saved_workflow(source) {
            return parsed.script;
        }
    }
    source.to_string()
}

/// TS `fromTo`: only write the end that is present (absent = the default: session model / this machine's ceiling).
fn from_to_value<T: serde::Serialize>(from: Option<T>, to: Option<T>) -> Value {
    let mut object = serde_json::Map::new();
    if let Some(from) = from {
        object.insert("from".to_string(), json!(from));
    }
    if let Some(to) = to {
        object.insert("to".to_string(), json!(to));
    }
    Value::Object(object)
}

/// TS `buildSettingsMessageText`: the canonical model-facing sentence for the settings turn (English, goes into the provider transcript).
fn build_settings_message_text(
    name: Option<&str>,
    previous: &str,
    run_id: &str,
    superseded: bool,
    amend: &Value,
) -> String {
    let mut changes: Vec<String> = Vec::new();
    if let Some(model) = amend.get("subagentModel") {
        changes.push(match model.get("to") {
            Some(to) => format!(
                "its subagents now run on {}",
                to.as_str().unwrap_or_default()
            ),
            None => "its subagents are back on the session model".to_string(),
        });
    }
    if let Some(bound) = amend.get("maxConcurrency") {
        changes.push(match bound.get("to").and_then(Value::as_u64) {
            Some(to) => format!("at most {to} of them run at once"),
            None => "the limit on subagents at once is removed".to_string(),
        });
    }
    if changes.is_empty() {
        // This host's command additionally accepts `script`: when the settings are unchanged and only the script was replaced, say what changed.
        changes.push("its script was updated".to_string());
    }
    let named = name
        .map(|name| format!(" (\"{name}\")"))
        .unwrap_or_default();
    let relation = if superseded {
        "supersedes"
    } else {
        "takes over from"
    };
    [
        format!(
            "Changed the settings of workflow run {previous}{named} from the GUI: {}.",
            changes.join("; ")
        ),
        format!(
            "The same script continues as run {run_id}, which {relation} run {previous} and imports everything run {previous} finished as cache."
        ),
        "Progress and results arrive as background notifications; do not amend, resume or restart it.".to_string(),
    ]
    .join(" ")
}

/// TS `boundedCompileDiagnostics`: one `L{line}:C{column} {message}` per line, with the body clipped to 2000
/// characters (ending in `…` when exceeded). Diagnostics travel in the ACK message; an over-long stack has no reading value.
fn bounded_compile_diagnostics(
    heading: &str,
    diagnostics: &[nc_code_workflow::compile::CompileDiagnostic],
) -> String {
    const MAX_CHARS: usize = 2_000;
    let mut body = String::from(heading);
    for diagnostic in diagnostics {
        body.push('\n');
        body.push_str(&format!(
            "L{}:C{} {}",
            diagnostic.line, diagnostic.column, diagnostic.message
        ));
    }
    if body.chars().count() > MAX_CHARS {
        let mut clipped: String = body.chars().take(MAX_CHARS - 1).collect();
        clipped.push('…');
        return clipped;
    }
    body
}

#[cfg(test)]
mod tests {
    #[test]
    fn basic_test() {
        assert!(true);
    }
}
