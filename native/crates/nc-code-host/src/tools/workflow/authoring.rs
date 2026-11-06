//! The four authoring-surface tools: CreateWorkflow / AmendWorkflow / SaveWorkflow / EvalWorkflowSnippet.
//!
//! The shared discipline (identical to the TS handler entry by entry):
//! - **the script lands on disk first, then compiles, and only then does anything happen**. An inline submission always writes a draft
//!   (a script that does not compile needs it most — it is the only copy the model has to edit), a `path` source writes nothing (the file is already the working copy), and a `saved`
//!   source writes the exact bytes just read (the definition itself is never changed by a run).
//! - **if it does not compile there is no action**: no run submitted, no saved definition written; diagnostics are reported by **file line** (the line numbers must be pasteable straight
//!   into an `Edit`).
//! - `max_concurrency` and `subagent_model` are both top-level fields, handled identically across the three sources, and normalisation happens
//!   before any disk read (a call that cannot resolve should not scan the disk first).

use std::path::Path;

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use nc_code_workflow::compile::CompileDiagnostic;

use crate::tools::workflow::support::{
    self, amended_script_sentence, clamp_max_concurrency, describe_args_declaration,
    describe_concurrency_limit, describe_script_path, describe_subagent_model, draft_name,
    format_diagnostic_lines, launched_script_sentence, read_workflow_script_file, resolve_saved,
    saved_draft_note, script_file_note, validate_args, write_draft, ScriptLocation,
    AMEND_WORKFLOW_SOURCE_ERROR, CREATE_WORKFLOW_ARGS_WITHOUT_PATH_ERROR,
    CREATE_WORKFLOW_SOURCE_ERROR, DIAGNOSTICS_NOT_EXECUTED_NOTE,
    EVAL_WORKFLOW_SNIPPET_SOURCE_ERROR, NOT_EXECUTED_NOTE, SAVE_WORKFLOW_SENTINEL_IN_SCRIPT_ERROR,
    SAVE_WORKFLOW_SOURCE_ERROR, SUBAGENT_MODEL_UNAVAILABLE, UNAVAILABLE_NOTE,
};
use crate::workflow_service::saved_workflows::{
    self, SavedWorkflowMeta, SavedWorkflowScope, SAVED_WORKFLOW_SENTINEL,
};
use crate::workflow_service::{AmendRequest, AmendResult, SubmitRequest, WorkflowServiceError};

use super::support::invalid_input;
use super::{
    SnippetOutcome, SnippetRequest, AMEND_WORKFLOW_TOOL_NAME, CREATE_WORKFLOW_TOOL_NAME,
    EVAL_WORKFLOW_SNIPPET_TOOL_NAME, SAVE_WORKFLOW_TOOL_NAME,
};
use crate::tools::{
    require_string, AsyncTool, ToolContext, ToolContract, ToolError, SCOPE_NONE, SCOPE_WORKSPACE,
};

// Budgets and timeouts (taken verbatim from the TS entry).
const CREATE_WORKFLOW_TIMEOUT_MS: u64 = 15_000;
const AUTHORING_MODEL_BYTES: usize = 24_000;
const SAVE_WORKFLOW_TIMEOUT_MS: u64 = 15_000;
const EVAL_WORKFLOW_SNIPPET_TIMEOUT_MS: u64 = 660_000;
const EVAL_WORKFLOW_SNIPPET_DEFAULT_TIMEOUT_MS: u64 = 60_000;
const EVAL_WORKFLOW_SNIPPET_MIN_TIMEOUT_MS: u64 = 1_000;
const EVAL_WORKFLOW_SNIPPET_MAX_TIMEOUT_MS: u64 = 600_000;
const EVAL_WORKFLOW_SNIPPET_MAX_LOGS: usize = 100;
const EVAL_WORKFLOW_SNIPPET_MAX_LOG_CHARS: usize = 2_048;
const EVAL_WORKFLOW_SNIPPET_MAX_ARTIFACT_BYTES: usize = 256 * 1024;

fn authoring_contract(
    description: &str,
    input_schema: Value,
    scope: &'static str,
    timeout_ms: u64,
    concurrent_safe: bool,
) -> ToolContract {
    ToolContract {
        // Filled in by each `impl`'s `contract()` with struct-update syntax (the gate reads the tool name at that position).
        name: String::new(),
        description: description.to_string(),
        input_schema,
        read_only: false,
        destructive: false,
        concurrent_safe,
        side_effect_scope: scope,
        timeout_ms,
        max_output_bytes: AUTHORING_MODEL_BYTES,
        max_model_bytes: AUTHORING_MODEL_BYTES,
        preview_direction: "head",
    }
}

// ————————————————————————————————————————————————————————————————
// Reading the input
// ————————————————————————————————————————————————————————————————

fn opt_str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

fn opt_object<'a>(input: &'a Value, key: &str) -> Option<&'a Map<String, Value>> {
    input.get(key).and_then(Value::as_object)
}

fn opt_u32(input: &Value, key: &str) -> Result<Option<u32>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value >= 1)
            .map(Some)
            .ok_or_else(|| invalid_input(format!("{key}: must be a positive integer"))),
        Some(_) => Err(invalid_input(format!("{key}: must be a positive integer"))),
    }
}

/// The normalisation of `subagent_model`: when requested it is resolved into canonical form **before the disk read**; when the catalogue is absent it is refused explicitly (a string
/// no host can resolve would travel all the way down and blow up when the subagent first speaks, by which time it looks like the model's problem).
fn resolve_subagent_model(
    requested: Option<&str>,
    ctx: &ToolContext,
) -> Result<Option<String>, ToolError> {
    let Some(requested) = requested.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let Some(config) = ctx.provider_config.as_deref() else {
        return Err(invalid_input(SUBAGENT_MODEL_UNAVAILABLE));
    };
    let entries = support::catalog_from_provider(config);
    support::resolve_model_reference(requested, &entries).map(Some)
}

/// The diagnostics path's output. When `saved` is present the NOTE names "which definition this copy was transcribed from"; with a file present diagnostics are reported by file line.
fn diagnostics_output(
    diagnostics: Vec<CompileDiagnostic>,
    location: Option<&ScriptLocation>,
    saved: Option<(&str, &str)>,
    headline: &str,
    trailer: &str,
) -> Value {
    let mut lines = vec![headline.to_string()];
    lines.extend(format_diagnostic_lines(&diagnostics, location));
    lines.push(String::new());
    let note = match (location, saved) {
        (Some(location), Some((saved_name, saved_path))) => {
            saved_draft_note(saved_name, saved_path, &location.described)
        }
        (Some(location), None) => script_file_note(location),
        (None, Some(_)) => {
            "NOTE: The workflow was NOT executed — the saved file needs fixing (edit it, or save a corrected version).".to_string()
        }
        (None, None) => DIAGNOSTICS_NOT_EXECUTED_NOTE.to_string(),
    };
    lines.push(note);
    lines.push(trailer.to_string());
    json!({
        "diagnostics": diagnostics,
        "ok": false,
        "response": lines.join("\n").trim_end().to_string(),
    })
}

/// Service-layer failures: compile diagnostics are business failures (already handled separately upstream), everything else is a wiring / storage fault.
fn service_failure(error: WorkflowServiceError) -> ToolError {
    if let WorkflowServiceError::CompileFailed { diagnostics } = &error {
        let lines: Vec<String> = diagnostics.iter().map(ToString::to_string).collect();
        return support::invalid_input(format!(
            "The workflow script does not compile:\n{}",
            lines.join("\n")
        ));
    }
    support::capability_unavailable(format!("workflow run service failure: {error}"))
}

// ————————————————————————————————————————————————————————————————
// CreateWorkflow
// ————————————————————————————————————————————————————————————————

/// The language sketch the model needs to write a correct script without reading a doc.
/// Kept next to the tool description on purpose: a description is the only authoring surface
/// text that reaches the model on every call, and the native runtime's language is not the one
/// the TypeScript facade taught.
const NWS_GUIDE: &str = "Write new scripts in NWS (nc-code Workflow Script), the native language: the host compiles and runs it in-process, so a run needs no Node and every facade call carries a journal identity that an amended re-run can reuse. A TypeScript script written against the dynamic-workflow facade is still accepted and runs through the compile kernel plus a Node sandbox; keep using it for existing drafts and saved workflows. Both languages feed the same preview card: NWS's graph is projected from the plan, the TypeScript one from the compiler's analysis. NWS shape — zero or more `schema Name { field: string, count: number, flag: boolean, items: string[], nested: Other, optional?: string }` declarations, then exactly one `workflow \"name\" { args { repo: string } ... }`; `args { ... }` right after the workflow opens declares the run's arguments, anywhere else `args` is the value they arrive in. Statements: `phase \"plan\";` opens a stage (a marker — it stamps what comes after it), `let x = ...;` or `let x: Schema = ...;` declares one value, `if/else`, `while`, `for (const item of seq)`, `parallel { { ... } { ... } }` for structured concurrency (each group is one strand, all run together), `try { ... } catch (err) { ... }` recovers from one rejected facade call with the engine's structured error as `err`, `report(item)` or `report(item, \"board\")` journals a progress finding, `return value;` is the run's output. Facades are the only other identifiers: `let a = agent(\"name\", \"persona\")` creates a subagent, `await a.ask(\"instruction\")` is answered by its final text and `await a.ask(\"instruction\", Schema)` — or an annotated `let x: Schema = await a.ask(...)`, the only form a `Promise.all` item can use — is answered as a closed JSON object matching the schema, so an unexpected key is a catchable violation instead of a silent misunderstanding. `await files.read(p)`, `await files.glob(pattern)`, `await files.grep(pattern, glob?)`, `await git.changedFiles(base?)`, `await git.diff(base?, path?)`, `await git.status()`, `await git.log(count?)` read the workspace; `await world.run(\"cargo test\", [\"-q\"], { timeoutMs: 120000 })` runs a command and resolves to `{ stdout, stderr, code }` — the command must be a string literal, because the set a run may execute is fixed and confirmed at submit time. `await artifact.file(id, path, opts?)` and `await artifact.markdown(id, text, opts?)` publish content, `artifact.board(id, spec)`, `artifact.table(id, spec)`, `artifact.metrics(id, spec)` and `artifact.chart(id, spec)` declare one card per id (declare them at the top, feed them with `report(item, \"id\")`). Unknown identifiers, a computed command, a duplicate actor name and a literal actor name inside a per-element loop body are compile errors returned as diagnostics before anything starts.";

const CREATE_WORKFLOW_OUTRO: &str = "Provide exactly one source: `script` inline, `saved` to run a saved workflow, or `path` to a script file (usually the draft a previous result named — edit it and pass the path instead of pasting the script again). The script is checked first; a clean check asks the user to confirm and then starts the run in the background and returns its ID, while a script with errors returns diagnostics and starts nothing. After launching, continue with other work: the final output arrives as a notification.";

/// The tool description the model actually sees: one language sketch shared by the three
/// authoring tools, so a script written from any of them compiles the same way.
fn nws_prefixed(outro: &str) -> String {
    format!("{NWS_GUIDE} {outro}")
}

fn create_workflow_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {"type": "string", "minLength": 1, "description": "Short display label for this run, in the user's language. Always pass it for an inline script: it labels the run everywhere and names its draft file under .zcode/workflow-drafts/."},
            "script": {"type": "string", "description": "The whole workflow script, inline, in NWS (or a TypeScript facade script for an existing draft). Provide exactly one of `script`, `saved` and `path`."},
            "saved": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "minLength": 1},
                    "args": {"type": "object", "additionalProperties": true},
                    "path": {"type": "string", "minLength": 1},
                    "scope": {"type": "string", "enum": ["project", "global"]}
                },
                "required": ["name"],
                "additionalProperties": false
            },
            "path": {"type": "string", "minLength": 1, "description": "A script file on disk, relative to the working directory or absolute — normally the file a previous CreateWorkflow/AmendWorkflow result named."},
            "args": {"type": "object", "additionalProperties": true, "description": "Values for the arguments a `path` file declares in its `/* nc-code-workflow` block. Only with `path`."},
            "max_concurrency": {"type": "integer", "minimum": 1, "description": "Upper bound on how many subagents work at the same time in this run. Set it ONLY when the user asks to limit parallelism. A value above what this machine allows is lowered to that maximum."},
            "subagent_model": {"type": "string", "description": "Model for the workflow's subagents, as `providerId/modelId` or a bare model id (optionally `$reasoningLevel`). Set it ONLY when the user asks for a specific model; the main agent keeps the session model."}
        },
        "additionalProperties": false
    })
}

/// The normalised script source (the three sources converge here into a single script).
struct ResolvedSource {
    script: String,
    /// The script-body path (absolute) used for logs / run identity; absent = the draft could not be written.
    script_path: Option<String>,
    location: Option<ScriptLocation>,
    saved: Option<(String, String)>,
    args: Option<Value>,
    /// The inline source's draft must wait until the analysis yields the first phase name before being written (same order as TS: analysis → draft → diagnostics).
    inline: bool,
}

enum SourceKind {
    Inline,
    Path,
    Saved,
}

/// Exactly one of the three sources + `args` only travels with `path` (true only for the inputs **the model emits**).
fn create_source_kind(input: &Value) -> Result<SourceKind, ToolError> {
    let has_script = input.get("script").is_some_and(|value| !value.is_null());
    let has_saved = input.get("saved").is_some_and(|value| !value.is_null());
    let has_path = input.get("path").is_some_and(|value| !value.is_null());
    let count = [has_script, has_saved, has_path]
        .into_iter()
        .filter(|present| *present)
        .count();
    if count != 1 {
        return Err(invalid_input(CREATE_WORKFLOW_SOURCE_ERROR));
    }
    if input.get("args").is_some_and(|value| !value.is_null()) && !has_path {
        return Err(invalid_input(CREATE_WORKFLOW_ARGS_WITHOUT_PATH_ERROR));
    }
    Ok(if has_saved {
        SourceKind::Saved
    } else if has_path {
        SourceKind::Path
    } else {
        SourceKind::Inline
    })
}

fn resolve_create_source(
    input: &Value,
    kind: &SourceKind,
    cwd: &Path,
) -> Result<ResolvedSource, ToolError> {
    match kind {
        SourceKind::Inline => {
            let script = opt_str(input, "script")
                .ok_or_else(|| invalid_input("script: required string"))?
                .to_string();
            Ok(ResolvedSource {
                script,
                script_path: None,
                location: None,
                saved: None,
                args: None,
                inline: true,
            })
        }
        SourceKind::Path => {
            let input_path =
                opt_str(input, "path").ok_or_else(|| invalid_input("path: required string"))?;
            let file = read_workflow_script_file(cwd, input_path, true)?;
            if file.meta.is_none() && opt_object(input, "args").is_some() {
                return Err(invalid_input(format!(
                    "The workflow script file {} declares no arguments (it has no `/* nc-code-workflow` metadata block), so it takes none. Drop `args`, or add a block declaring them.",
                    file.described
                )));
            }
            let provided = opt_object(input, "args");
            let validated = validate_args(
                file.meta.as_ref().and_then(|meta| meta.args.as_ref()),
                provided,
            )
            .map_err(|error| {
                invalid_input(format!(
                    "The arguments for the workflow script file {} are not valid:\n{}\n\n{}",
                    file.described,
                    strip_error_prefix(&error),
                    describe_args_declaration(
                        &file.described,
                        file.meta.as_ref().and_then(|meta| meta.args.as_ref())
                    )
                ))
            })?;
            let location = ScriptLocation {
                kind: "path",
                described: file.described.clone(),
                line_offset: file.body_line_offset,
            };
            Ok(ResolvedSource {
                script: file.script,
                script_path: Some(file.path),
                location: Some(location),
                saved: None,
                args: if validated.is_empty() {
                    None
                } else {
                    Some(Value::Object(validated))
                },
                inline: false,
            })
        }
        SourceKind::Saved => {
            let saved = input
                .get("saved")
                .and_then(Value::as_object)
                .ok_or_else(|| invalid_input("saved: required object"))?;
            let saved_name = saved
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_input("saved.name: required string"))?;
            let scope = parse_scope(saved.get("scope").and_then(Value::as_str))?;
            let found = resolve_saved(cwd, saved_name, scope)?;
            let provided = saved.get("args").and_then(Value::as_object);
            let validated = validate_args(found.meta.args.as_ref(), provided).map_err(|error| {
                invalid_input(format!(
                    "The arguments for saved workflow '{saved_name}' are not valid:\n{}\n\n{}",
                    strip_error_prefix(&error),
                    describe_args_declaration(
                        &format!("'{}'", found.name),
                        found.meta.args.as_ref()
                    )
                ))
            })?;
            // The working copy is transcribed byte for byte from the definition (metadata block included): the definition itself is never changed by a run.
            let draft = write_draft(
                cwd,
                opt_str(input, "name").unwrap_or(&found.name),
                &found.source,
            );
            let location = draft.as_ref().map(|draft| ScriptLocation {
                kind: "draft",
                described: describe_script_path(Path::new(draft), Some(cwd)),
                line_offset: found.body_line_offset,
            });
            Ok(ResolvedSource {
                script: found.script,
                script_path: draft,
                location,
                saved: Some((found.name, found.path)),
                // The saved source's args are always an object (possibly empty), same shape as TS's `parsed.args ?? saved.args`:
                // when nothing is declared the sandbox still reads `{}`.
                args: Some(Value::Object(validated)),
                inline: false,
            })
        }
    }
}

fn strip_error_prefix(error: &ToolError) -> String {
    error
        .message
        .strip_prefix("[tool.invalidInput] ")
        .unwrap_or(&error.message)
        .to_string()
}

fn parse_scope(raw: Option<&str>) -> Result<Option<SavedWorkflowScope>, ToolError> {
    match raw {
        None => Ok(None),
        Some("project") => Ok(Some(SavedWorkflowScope::Project)),
        Some("global") => Ok(Some(SavedWorkflowScope::Global)),
        Some(other) => Err(invalid_input(format!(
            "scope: must be \"project\" or \"global\", got \"{other}\""
        ))),
    }
}

pub struct CreateWorkflowTool;

#[async_trait]
impl AsyncTool for CreateWorkflowTool {
    fn contract(&self) -> ToolContract {
        // Writing the tool name at **this position** is load-bearing: `native/tools/check-tool-parity.mjs` reads
        // this line out of the `impl ... { ... }` body to prove the registry matches the TS tool surface.
        ToolContract {
            name: CREATE_WORKFLOW_TOOL_NAME.to_string(),
            ..authoring_contract(
                &nws_prefixed(CREATE_WORKFLOW_OUTRO),
                create_workflow_schema(),
                SCOPE_NONE,
                CREATE_WORKFLOW_TIMEOUT_MS,
                // TS's concurrentSafe: true (each run owns its runId and cwd, so they never overwrite one another).
                true,
            )
        }
    }

    /// TS `permission.alwaysAsk: true`: the confirmation window overrides every "allow" branch (including full-access / plan's
    /// read-only fast path), because one call really spends money and has real side effects.
    fn always_requires_approval(&self) -> bool {
        true
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let Some(port) = ctx.workflow.as_deref() else {
            return Err(support::workflow_execution_unavailable());
        };
        let kind = create_source_kind(input)?;
        // Model and concurrency ceiling are resolved before the disk read (the same code path for all three sources).
        let subagent_model = resolve_subagent_model(opt_str(input, "subagent_model"), ctx)?;
        let max_concurrency = clamp_max_concurrency(
            opt_u32(input, "max_concurrency")?,
            port.concurrency_ceiling(),
        );
        let cwd = ctx.workspace_root.as_path();
        let name = opt_str(input, "name").map(str::to_string);

        let mut source = resolve_create_source(input, &kind, cwd)?;
        let analysis = port.analyze(&source.script).await.map_err(|error| {
            support::capability_unavailable(format!("workflow script analysis failed: {error}"))
        })?;

        // An inline script's working copy lands on disk **regardless of the compile result**: a script that does not compile never reaches the confirmation window, and the handler
        // is the only place it must pass through. The analysis runs first only to name it (when there is no `name`, the first phase name is used).
        if source.inline {
            let draft = write_draft(
                cwd,
                &draft_name(name.as_deref(), &analysis.phase_names),
                &source.script,
            );
            source.location = draft.as_ref().map(|draft| ScriptLocation {
                kind: "draft",
                described: describe_script_path(Path::new(draft), Some(cwd)),
                line_offset: 0,
            });
            source.script_path = draft;
        }
        let location = source.location.clone();

        if !analysis.ok {
            let saved = source
                .saved
                .as_ref()
                .map(|(name, path)| (name.as_str(), path.as_str()));
            let headline = match &source.saved {
                None => "The workflow script has errors:".to_string(),
                Some((name, path)) => {
                    format!("The saved workflow '{name}' ({path}) has errors:")
                }
            };
            return Ok(diagnostics_output(
                analysis.diagnostics,
                location.as_ref(),
                saved,
                &headline,
                "",
            )
            .to_string());
        }

        let submitted = port
            .submit(SubmitRequest {
                session_id: ctx.session_id.clone(),
                script_text: source.script.clone(),
                cwd: Some(cwd.to_string_lossy().to_string()),
                name: name.clone(),
                args: source.args.clone(),
                tool_call_id: ctx.tool_call_id.clone(),
                phase_names: if analysis.phase_names.is_empty() {
                    None
                } else {
                    Some(analysis.phase_names.clone())
                },
                phase_alongside: analysis.phase_alongside.clone(),
                launch_input_id: None,
                max_concurrency,
                subagent_model: subagent_model.clone(),
                script_path: source.script_path.clone(),
            })
            .await;
        let submitted = match submitted {
            Ok(submitted) => submitted,
            Err(WorkflowServiceError::CompileFailed { diagnostics }) => {
                let saved = source
                    .saved
                    .as_ref()
                    .map(|(name, path)| (name.as_str(), path.as_str()));
                return Ok(diagnostics_output(
                    diagnostics,
                    location.as_ref(),
                    saved,
                    "The workflow script has errors:",
                    "",
                )
                .to_string());
            }
            Err(error) => return Err(service_failure(error)),
        };

        let response = format!(
            "The workflow script compiled cleanly and the run started in the background with ID: {}. It is still running — you will be notified with the final output when it completes. Do not wait for it or poll it with TaskOutput; continue with other work unless the user asked you to wait.{}{}{}",
            submitted.run_id,
            describe_concurrency_limit(max_concurrency, port.concurrency_ceiling()),
            describe_subagent_model(subagent_model.as_deref()),
            location
                .as_ref()
                .map(launched_script_sentence)
                .unwrap_or_default(),
        );
        let mut output = json!({
            "diagnostics": analysis.diagnostics,
            "ok": true,
            "response": response,
            "status": "backgrounded",
            "backgroundTaskId": submitted.run_id,
        });
        if let Some(graph) = analysis.causality_graph {
            output["causalityGraph"] = graph;
        }
        Ok(output.to_string())
    }

    fn result_display_for(&self, _input: &Value, output_text: &str) -> Option<Value> {
        let output: Value = serde_json::from_str(output_text).ok()?;
        crate::tools::workflow::display::create_workflow_display(&output)
    }
}

// ————————————————————————————————————————————————————————————————
// AmendWorkflow
// ————————————————————————————————————————————————————————————————

const AMEND_WORKFLOW_OUTRO: &str = "Revise an existing dynamic-workflow run: stop it if it is still running, import its finished work as a warm cache, and start the revision in the background. Pass the corrected script as `path` (the usual form: the file GetWorkflowRun named) or inline `script`. Any run of this project qualifies, settled or still running. After it starts, continue with other work: the final output arrives as a notification.";

fn amend_workflow_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "run_id": {"type": "string", "minLength": 1, "description": "ID of the run to amend — from CreateWorkflow's or AmendWorkflow's result, a notification, GetWorkflowRun or ListWorkflowRuns. A running run is stopped and superseded by the new one."},
            "script": {"type": "string", "minLength": 1, "description": "The WHOLE revised workflow script, inline, in the same language as the predecessor or NWS for a new revision. Provide this OR `path`, never both; `path` is the usual form."},
            "path": {"type": "string", "minLength": 1, "description": "The revised script's file, relative to the working directory or absolute — usually the predecessor's own script file. Provide this OR `script`, never both."},
            "name": {"type": "string", "minLength": 1, "description": "Optional display label for the new run. Defaults to the predecessor's name."},
            "max_concurrency": {"type": ["integer", "null"], "minimum": 1, "description": "Upper bound on how many subagents work at the same time in the new run. OMIT to keep the predecessor's limit; pass null to remove it; pass a number to set one."},
            "subagent_model": {"type": ["string", "null"], "description": "Model for the new run's subagents. OMIT to keep the predecessor's choice; null to return to the session model; a string to set one."}
        },
        "required": ["run_id"],
        "additionalProperties": false
    })
}

/// The predecessor does not exist: reuses the introspection tool's `run_not_found` and only adds a sentence naming `run_id` (same shape as TS).
fn predecessor_not_found(run_id: &str) -> ToolError {
    let base = support::run_not_found(run_id);
    invalid_input(format!(
        "{} Nothing was stopped or created: `run_id` pointed at a run that does not exist — pass an existing run's ID (see ListWorkflowRuns), or start a fresh run with CreateWorkflow.",
        strip_error_prefix(&base)
    ))
}

fn amend_refusal(reason: &str, run_id: &str) -> ToolError {
    match reason {
        "run_not_found" => predecessor_not_found(run_id),
        "missing_boundaries" => invalid_input(format!(
            "workflow_amend_missing_boundaries: run {run_id}'s journal predates transcript-boundary bookkeeping (or its boundaries were never recorded), so its finished asks cannot seed an amended run — there is no fallback. Submit this script as a fresh CreateWorkflow instead. Nothing was stopped or created."
        )),
        other => invalid_input(format!(
            "workflow_amend_refused: the workflow runtime refused to amend run {run_id} (reason: {other}). Nothing was stopped or created."
        )),
    }
}

pub struct AmendWorkflowTool;

#[async_trait]
impl AsyncTool for AmendWorkflowTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: AMEND_WORKFLOW_TOOL_NAME.to_string(),
            ..authoring_contract(
                &nws_prefixed(AMEND_WORKFLOW_OUTRO),
                amend_workflow_schema(),
                SCOPE_NONE,
                CREATE_WORKFLOW_TIMEOUT_MS,
                true,
            )
        }
    }

    fn always_requires_approval(&self) -> bool {
        true
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let Some(port) = ctx.workflow.as_deref() else {
            return Err(support::workflow_execution_unavailable());
        };
        let run_id = require_string(input, "run_id")?.to_string();
        let has_script = input.get("script").is_some_and(|value| !value.is_null());
        let has_path = input.get("path").is_some_and(|value| !value.is_null());
        if has_script && has_path {
            return Err(invalid_input(AMEND_WORKFLOW_SOURCE_ERROR));
        }
        // The predecessor lookup comes before the script is settled: a call pointing at a run that does not exist should not read a file first.
        let snapshot = port.get_run(&run_id).map_err(|reason| {
            support::capability_unavailable(format!("workflow_introspection_unavailable: {reason}"))
        })?;
        let Some(snapshot) = snapshot else {
            return Err(predecessor_not_found(&run_id));
        };
        let row = port.run_row(&run_id).ok().flatten();
        let predecessor_name = row
            .as_ref()
            .filter(|row| row.label_source == crate::workflow_service::LabelSource::Name)
            .map(|row| row.label.clone());
        let subagent_model = resolve_subagent_model(opt_str(input, "subagent_model"), ctx)?;
        // Three states: omitted inherits the predecessor (the snapshot only carries this field when it "was set"); null clears it; a number sets it.
        let requested_model_is_null = input.get("subagent_model").is_some_and(Value::is_null);
        let inherited_model = if requested_model_is_null {
            None
        } else {
            subagent_model
                .clone()
                .or_else(|| snapshot.subagent_model.clone())
        };
        let max_concurrency = match input.get("max_concurrency") {
            // null = clear (back to the ceiling).
            Some(Value::Null) => None,
            // a number = set (clamped below the ceiling).
            Some(_) => clamp_max_concurrency(
                opt_u32(input, "max_concurrency")?,
                port.concurrency_ceiling(),
            ),
            // omitted = inherit the predecessor's effective ceiling (the number on the snapshot was already clamped when the run was created).
            None => Some(snapshot.max_concurrency),
        };

        let cwd = ctx.workspace_root.as_path();
        let script_path_field = opt_str(input, "path").map(str::to_string);
        let is_path_source = script_path_field.is_some();
        let (script, file_path, line_offset) = match script_path_field {
            Some(input_path) => {
                let file = read_workflow_script_file(cwd, &input_path, true)?;
                (file.script, file.path, file.body_line_offset)
            }
            None => {
                let Some(script) = opt_str(input, "script") else {
                    // Neither given = "inherit the predecessor's script". TS backfills it from the predecessor journal's scriptText;
                    // this host's run service observation surface does not expose that column (RunSnapshot has no script_text),
                    // so a named business failure is returned here instead of quietly running different bytes.
                    return Err(invalid_input(format!(
                        "workflow_amend_script_unavailable: this host cannot read run {run_id}'s stored script back through its run service, so a revision that omits both `script` and `path` has nothing to run. Pass the revised `path` (edit the file named in the run's scriptPath) or the whole `script` inline. Nothing was stopped or created."
                    )));
                };
                let draft = write_draft(
                    cwd,
                    &draft_name(opt_str(input, "name").or(predecessor_name.as_deref()), &[]),
                    script,
                );
                match draft {
                    Some(draft) => (script.to_string(), draft, 0),
                    None => (script.to_string(), String::new(), 0),
                }
            }
        };
        let has_script_file = !file_path.is_empty();
        let location = has_script_file.then(|| ScriptLocation {
            kind: if is_path_source { "path" } else { "draft" },
            described: describe_script_path(Path::new(&file_path), Some(cwd)),
            line_offset,
        });

        let analysis = port.analyze(&script).await.map_err(|error| {
            support::capability_unavailable(format!("workflow script analysis failed: {error}"))
        })?;
        if !analysis.ok {
            let response = diagnostics_output(
                analysis.diagnostics,
                location.as_ref(),
                None,
                "The revised workflow script has errors:",
                &format!("Run {run_id} was not touched."),
            );
            return Ok(response.to_string());
        }

        let amended: Result<AmendResult, WorkflowServiceError> = port
            .amend(AmendRequest {
                session_id: Some(ctx.session_id.clone()),
                predecessor_run_id: run_id.clone(),
                script_text: script.clone(),
                cwd: Some(cwd.to_string_lossy().to_string()),
                name: opt_str(input, "name")
                    .map(str::to_string)
                    .or(predecessor_name),
                max_concurrency,
                subagent_model: inherited_model.clone(),
                script_path: has_script_file.then(|| file_path.clone()),
                // The tool path does not inherit args (only the GUI's "configure" rerun inherits them).
                inherit_args: false,
                tool_call_id: ctx.tool_call_id.clone(),
                phase_names: if analysis.phase_names.is_empty() {
                    None
                } else {
                    Some(analysis.phase_names.clone())
                },
                phase_alongside: analysis.phase_alongside.clone(),
            })
            .await;
        let amended = match amended {
            Ok(amended) => amended,
            Err(WorkflowServiceError::AmendRejected { reason, .. }) => {
                return Err(amend_refusal(reason.as_str(), &run_id))
            }
            Err(WorkflowServiceError::CompileFailed { diagnostics }) => {
                return Ok(diagnostics_output(
                    diagnostics,
                    location.as_ref(),
                    None,
                    "The revised workflow script has errors:",
                    &format!("Run {run_id} was not touched."),
                )
                .to_string())
            }
            Err(error) => return Err(service_failure(error)),
        };

        let superseded = match &amended.superseded_run_id {
            None => format!(
                "Run {run_id} had already settled; its finished work is imported as cache."
            ),
            Some(superseded) => format!(
                "Run {superseded} was still running: it has been stopped and superseded, and everything it finished before the stop is imported as cache. It will not send a notification of its own."
            ),
        };
        let response = format!(
            "{superseded} The revised script started in the background as run {}. It is still running — you will be notified with the final output when it completes. Do not wait for it or poll it with TaskOutput; continue with other work unless the user asked you to wait.{}{}{}",
            amended.run_id,
            describe_concurrency_limit(max_concurrency, port.concurrency_ceiling()),
            describe_subagent_model(inherited_model.as_deref()),
            location
                .as_ref()
                .map(amended_script_sentence)
                .unwrap_or_default(),
        );
        let mut output = json!({
            "diagnostics": analysis.diagnostics,
            "ok": true,
            "response": response,
            "status": "backgrounded",
            "backgroundTaskId": amended.run_id,
        });
        if let Some(graph) = analysis.causality_graph {
            output["causalityGraph"] = graph;
        }
        Ok(output.to_string())
    }

    fn result_display_for(&self, _input: &Value, output_text: &str) -> Option<Value> {
        let output: Value = serde_json::from_str(output_text).ok()?;
        crate::tools::workflow::display::create_workflow_display(&output)
    }
}

// ————————————————————————————————————————————————————————————————
// SaveWorkflow
// ————————————————————————————————————————————————————————————————

const SAVE_WORKFLOW_OUTRO: &str = "Check a workflow script and, once confirmed, save it into the project as a reusable definition (`.zcode/workflows/<name>.dwf.ts`) or into the global archive (`~/.zcode/workflows`). NEVER call this tool unsolicited: saving writes a file into the user's repository; suggest it in prose and wait for agreement. The required `scope` field decides where it lives. Run the result later with CreateWorkflow's `saved` source.";

fn save_workflow_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {"type": "string", "minLength": 1, "maxLength": 64, "description": "File-safe identifier the workflow will be recalled by. Letters, digits, dot, dash and underscore only."},
            "description": {"type": "string", "minLength": 1, "description": "One line saying what the workflow does. Shown wherever the workflow is listed."},
            "whenToUse": {"type": "string", "minLength": 1, "description": "Optional guidance on the situations this workflow is the right answer to."},
            "args": {"type": "object", "additionalProperties": true, "description": "Optional argument declarations. The script reads validated values off the `args` global."},
            "script": {"type": "string", "description": "The workflow script body, NWS for a new definition (a TypeScript facade script also saves). Body only — do not include a metadata block. Provide this OR `script_path`, never both."},
            "script_path": {"type": "string", "minLength": 1, "description": "The file holding the script body, relative to the working directory or absolute — usually a draft a CreateWorkflow/AmendWorkflow result named. Provide this OR `script`, never both."},
            "scope": {"type": "string", "enum": ["project", "global"], "description": "Where the workflow is saved. \"project\" when the script references this repository's files or conventions; \"global\" when it depends on nothing in this project. Decide every time; there is no default."},
            "path": {"type": "string", "minLength": 1, "description": "Resolved before the confirmation window: where the file lands."},
            "overwrite": {"type": "boolean", "description": "Resolved before the confirmation window: the target already exists, so approving replaces it."},
            "shadowing": {"type": "string", "enum": ["hides_global", "hidden_by_project"], "description": "Resolved before the confirmation window: a same-named definition exists in the other archive."}
        },
        "required": ["name", "description", "scope"],
        "additionalProperties": false
    })
}

fn validate_save_input(input: &Value) -> Result<(), ToolError> {
    let name = require_string(input, "name")?;
    if !saved_workflows::is_valid_saved_workflow_name(name) {
        return Err(invalid_input(format!(
            "'{name}' is not a usable workflow name: names may only contain letters, digits, '.', '-' and '_', and must be 1-{} characters.",
            saved_workflows::SAVED_WORKFLOW_MAX_NAME_CHARS
        )));
    }
    require_string(input, "description")?;
    let has_script = input.get("script").is_some_and(|value| !value.is_null());
    let has_path = input
        .get("script_path")
        .is_some_and(|value| !value.is_null());
    if has_script == has_path {
        return Err(invalid_input(SAVE_WORKFLOW_SOURCE_ERROR));
    }
    if let Some(script) = opt_str(input, "script") {
        if script.trim_start().starts_with(SAVED_WORKFLOW_SENTINEL) {
            return Err(invalid_input(SAVE_WORKFLOW_SENTINEL_IN_SCRIPT_ERROR));
        }
    }
    Ok(())
}

fn meta_from_input(input: &Value) -> Result<SavedWorkflowMeta, ToolError> {
    let description = require_string(input, "description")?.to_string();
    let when_to_use = opt_str(input, "whenToUse").map(str::to_string);
    let args = match input.get("args") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            serde_json::from_value(value.clone())
                .map_err(|error| invalid_input(format!("args: {error}")))?,
        ),
    };
    Ok(SavedWorkflowMeta {
        description,
        when_to_use,
        args,
    })
}

pub struct SaveWorkflowTool;

#[async_trait]
impl AsyncTool for SaveWorkflowTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: SAVE_WORKFLOW_TOOL_NAME.to_string(),
            ..authoring_contract(
                &nws_prefixed(SAVE_WORKFLOW_OUTRO),
                save_workflow_schema(),
                // What is written is a file inside the project, the same scope as Write/Edit.
                SCOPE_WORKSPACE,
                SAVE_WORKFLOW_TIMEOUT_MS,
                // TS concurrentSafe: false (saves under the same name overwrite each other).
                false,
            )
        }
    }

    fn always_requires_approval(&self) -> bool {
        true
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let Some(port) = ctx.workflow.as_deref() else {
            return Err(support::capability_unavailable(
                "workflow_analysis_unavailable: this session cannot typecheck a workflow script, so nothing was saved. This is a capability gap, not a script error.",
            ));
        };
        validate_save_input(input)?;
        let name = require_string(input, "name")?.to_string();
        let scope = parse_scope(opt_str(input, "scope"))?
            .ok_or_else(|| invalid_input("scope: required; pass \"project\" or \"global\""))?;
        let meta = meta_from_input(input)?;
        let cwd = ctx.workspace_root.as_path();

        // `script_path` is read as the body first (the block is dropped — the metadata is decided by this call's fields).
        let (script, location) = match opt_str(input, "script_path") {
            Some(input_path) => {
                let file = read_workflow_script_file(cwd, input_path, true)?;
                let location = ScriptLocation {
                    kind: "path",
                    described: file.described.clone(),
                    line_offset: 0,
                };
                (file.script, Some(location))
            }
            None => (
                opt_str(input, "script")
                    .ok_or_else(|| invalid_input("script: required string"))?
                    .to_string(),
                None,
            ),
        };

        let analysis = port.analyze(&script).await.map_err(|error| {
            support::capability_unavailable(format!("workflow script analysis failed: {error}"))
        })?;
        // The landing path is filled by normalisation: the confirmation window reads the same facts (path / overwrite / shadowing).
        let root = saved_workflows::saved_workflow_roots(cwd, None)
            .into_iter()
            .find(|root| root.scope == scope)
            .expect("roots cover both scopes");
        let path = saved_workflows::saved_workflow_path(&root, &name)
            .to_string_lossy()
            .to_string();
        if !analysis.ok {
            let mut lines = vec!["The workflow script has errors:".to_string()];
            lines.extend(format_diagnostic_lines(
                &analysis.diagnostics,
                location.as_ref(),
            ));
            lines.push(String::new());
            lines.push(
                "NOTE: Nothing was saved — fix the errors above and call the tool again."
                    .to_string(),
            );
            return Ok(json!({
                "name": name,
                "scope": scope.as_str(),
                "path": path,
                "diagnostics": analysis.diagnostics,
                "ok": false,
                "response": lines.join("\n"),
            })
            .to_string());
        }

        // A write failure bubbles up as a tool-call failure: it is never swallowed into a success output that reported a path (that would let the model
        // tell the user "saved"). `overwritten` is re-decided **at the moment of the write**, not read from the input.
        let (written_path, written_scope, overwritten) =
            saved_workflows::save_saved_workflow(cwd, &name, &meta, &script, Some(scope), None)
                .map_err(|error| {
                    support::capability_unavailable(format!(
                        "the workflow could not be saved to {path}: {error}"
                    ))
                })?;
        let is_global = written_scope == SavedWorkflowScope::Global;
        let response = [
            if overwritten {
                if is_global {
                    format!("Replaced the saved global workflow '{name}' at {written_path}.")
                } else {
                    format!("Replaced the saved workflow '{name}' at {written_path}.")
                }
            } else if is_global {
                format!("Saved global workflow '{name}' to {written_path}.")
            } else {
                format!("Saved the workflow '{name}' to {written_path}.")
            },
            format!("Run it with CreateWorkflow using `saved: {{ name: \"{name}\" }}`."),
        ]
        .join(" ");
        Ok(json!({
            "diagnostics": analysis.diagnostics,
            "ok": true,
            "response": response,
            "name": name,
            "scope": written_scope.as_str(),
            "path": written_path,
            "overwritten": overwritten,
        })
        .to_string())
    }
}

// ————————————————————————————————————————————————————————————————
// EvalWorkflowSnippet
// ————————————————————————————————————————————————————————————————

const EVAL_WORKFLOW_SNIPPET_DESCRIPTION: &str = "Compile and synchronously run a small dynamic-workflow TypeScript snippet (world reads + pure logic) against the real workflow execution path, fully ephemerally. Snippets use files.*, git.*, log, top-level await and return — no agent()/report(). Provide exactly one source: `code` inline or `path` for a file holding it (the whole file is the snippet). Nothing is persisted and no background run is created.";

fn eval_snippet_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "code": {"type": "string", "minLength": 1, "description": "TypeScript snippet written against the snippet facade (files.*, git.*, log, top-level await and return). No agent()/report(). Provide this OR `path`, never both."},
            "path": {"type": "string", "minLength": 1, "description": "A file holding the snippet, relative to the working directory or absolute. Provide this OR `code`, never both. The whole file is the snippet."},
            "timeoutMs": {"type": "integer", "minimum": EVAL_WORKFLOW_SNIPPET_MIN_TIMEOUT_MS, "maximum": EVAL_WORKFLOW_SNIPPET_MAX_TIMEOUT_MS, "description": "Wall-clock timeout for the whole snippet in milliseconds. Default 60000."}
        },
        "additionalProperties": false
    })
}

pub struct EvalWorkflowSnippetTool;

fn snippet_logs_section(logs: &[String], truncated: bool) -> Vec<String> {
    if logs.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![String::new(), "Logs:".to_string()];
    lines.extend(logs.iter().map(|message| format!("- {message}")));
    if truncated {
        lines.push("- … (logs truncated)".to_string());
    }
    lines
}

#[async_trait]
impl AsyncTool for EvalWorkflowSnippetTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: EVAL_WORKFLOW_SNIPPET_TOOL_NAME.to_string(),
            ..authoring_contract(
                EVAL_WORKFLOW_SNIPPET_DESCRIPTION,
                eval_snippet_schema(),
                SCOPE_WORKSPACE,
                EVAL_WORKFLOW_SNIPPET_TIMEOUT_MS,
                true,
            )
        }
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let has_code = input.get("code").is_some_and(|value| !value.is_null());
        let has_path = input.get("path").is_some_and(|value| !value.is_null());
        if has_code == has_path {
            return Err(invalid_input(EVAL_WORKFLOW_SNIPPET_SOURCE_ERROR));
        }
        let cwd = ctx.workspace_root.as_path();
        let (code, location) = match opt_str(input, "path") {
            Some(input_path) => {
                // The whole file is the snippet: no metadata block is parsed.
                let file = read_workflow_script_file(cwd, input_path, false)?;
                let location = if input_path.is_empty() {
                    None
                } else {
                    Some(ScriptLocation {
                        kind: "path",
                        described: describe_script_path(Path::new(&file.path), Some(cwd)),
                        line_offset: 0,
                    })
                };
                (file.source, location)
            }
            None => (
                opt_str(input, "code")
                    .ok_or_else(|| invalid_input("code: required string"))?
                    .to_string(),
                None,
            ),
        };
        let Some(port) = ctx.workflow.as_deref() else {
            return Ok(json!({
                "ok": false,
                "diagnostics": [],
                "logs": [],
                "response": UNAVAILABLE_NOTE,
                "durationMs": 0,
            })
            .to_string());
        };
        let timeout_ms = opt_u32(input, "timeoutMs")?
            .map(u64::from)
            .unwrap_or(EVAL_WORKFLOW_SNIPPET_DEFAULT_TIMEOUT_MS)
            .clamp(
                EVAL_WORKFLOW_SNIPPET_MIN_TIMEOUT_MS,
                EVAL_WORKFLOW_SNIPPET_MAX_TIMEOUT_MS,
            );
        let started = std::time::Instant::now();
        let outcome = port
            .eval_snippet(SnippetRequest {
                code,
                cwd: cwd.to_string_lossy().to_string(),
                session_id: ctx.session_id.clone(),
                timeout_ms,
            })
            .await
            .map_err(|error| {
                support::capability_unavailable(format!("snippet evaluation failed: {error}"))
            })?;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match outcome {
            SnippetOutcome::Diagnostics(diagnostics) => {
                let bounded: Vec<CompileDiagnostic> = diagnostics;
                let mut lines = vec!["The snippet has errors:".to_string()];
                lines.extend(format_diagnostic_lines(&bounded, location.as_ref()));
                lines.push(String::new());
                lines.push(NOT_EXECUTED_NOTE.to_string());
                Ok(json!({
                    "ok": false,
                    "diagnostics": bounded,
                    "logs": [],
                    "response": lines.join("\n"),
                    "durationMs": duration_ms,
                })
                .to_string())
            }
            SnippetOutcome::Failed {
                code,
                message,
                logs,
                logs_truncated,
            } => {
                let (logs, logs_truncated) = bound_snippet_logs(logs, logs_truncated);
                let mut lines = vec![format!("The snippet failed ({code}): {message}")];
                lines.extend(snippet_logs_section(&logs, logs_truncated));
                Ok(json!({
                    "ok": false,
                    "diagnostics": [],
                    "logs": logs,
                    "response": lines.join("\n"),
                    "durationMs": duration_ms,
                })
                .to_string())
            }
            SnippetOutcome::Completed {
                artifact,
                logs,
                logs_truncated,
            } => {
                let (logs, logs_truncated) = bound_snippet_logs(logs, logs_truncated);
                let serialized = artifact
                    .as_ref()
                    .and_then(|value| support::serialize_artifact(Some(value)));
                let oversize = serialized
                    .as_ref()
                    .map(|text| text.len())
                    .filter(|bytes| *bytes > EVAL_WORKFLOW_SNIPPET_MAX_ARTIFACT_BYTES);
                if let Some(bytes) = oversize {
                    let mut lines = vec![format!(
                        "The snippet failed (ArtifactTooLarge): The snippet's return value serializes to {bytes} bytes, over the cap of {EVAL_WORKFLOW_SNIPPET_MAX_ARTIFACT_BYTES} bytes. Return a summary (a count, the first few items) and leave the full data to log() or to the real run."
                    )];
                    lines.extend(snippet_logs_section(&logs, logs_truncated));
                    return Ok(json!({
                        "ok": false,
                        "diagnostics": [],
                        "logs": logs,
                        "response": lines.join("\n"),
                        "durationMs": duration_ms,
                    })
                    .to_string());
                }
                let mut lines = vec![format!("The snippet completed in {duration_ms}ms.")];
                lines.push(match &serialized {
                    None => "It returned no value.".to_string(),
                    Some(text) => format!("Return value:\n{text}"),
                });
                lines.extend(snippet_logs_section(&logs, logs_truncated));
                Ok(json!({
                    "ok": true,
                    "diagnostics": [],
                    "logs": logs,
                    "response": lines.join("\n"),
                    "durationMs": duration_ms,
                })
                .to_string())
            }
        }
    }

    fn result_display_for(&self, _input: &Value, output_text: &str) -> Option<Value> {
        let output: Value = serde_json::from_str(output_text).ok()?;
        crate::tools::workflow::display::eval_workflow_snippet_display(&output)
    }
}

/// The bounds for logs: count × per-entry length (every payload at a protocol boundary is bounded); when cut, the truncation flag is set.
fn bound_snippet_logs(logs: Vec<String>, truncated: bool) -> (Vec<String>, bool) {
    let mut truncated = truncated || logs.len() > EVAL_WORKFLOW_SNIPPET_MAX_LOGS;
    let bounded = logs
        .into_iter()
        .take(EVAL_WORKFLOW_SNIPPET_MAX_LOGS)
        .map(|message| {
            if message.chars().count() > EVAL_WORKFLOW_SNIPPET_MAX_LOG_CHARS {
                truncated = true;
                message
                    .chars()
                    .take(EVAL_WORKFLOW_SNIPPET_MAX_LOG_CHARS)
                    .collect()
            } else {
                message
            }
        })
        .collect();
    (bounded, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_xor_is_enforced_on_model_input() {
        assert!(create_source_kind(&json!({})).is_err());
        assert!(create_source_kind(&json!({"script": "x", "path": "y"})).is_err());
        assert!(create_source_kind(&json!({"script": "x", "saved": {"name": "a"}})).is_err());
        // `args` only travels with `path`.
        assert!(create_source_kind(&json!({"script": "x", "args": {}})).is_err());
        assert!(create_source_kind(&json!({"script": "x"})).is_ok());
        assert!(create_source_kind(&json!({"path": "x", "args": {}})).is_ok());
    }

    #[test]
    fn snippet_logs_are_bounded_in_both_dimensions() {
        let long = "Word".repeat(EVAL_WORKFLOW_SNIPPET_MAX_LOG_CHARS + 10);
        let (bounded, truncated) = bound_snippet_logs(
            (0..(EVAL_WORKFLOW_SNIPPET_MAX_LOGS + 5))
                .map(|index| {
                    if index == 0 {
                        long.clone()
                    } else {
                        "ok".to_string()
                    }
                })
                .collect(),
            false,
        );
        assert!(truncated);
        assert_eq!(bounded.len(), EVAL_WORKFLOW_SNIPPET_MAX_LOGS);
        assert_eq!(
            bounded[0].chars().count(),
            EVAL_WORKFLOW_SNIPPET_MAX_LOG_CHARS
        );
    }

    #[test]
    fn save_input_rejects_the_ambiguous_and_the_prewrapped() {
        assert!(
            validate_save_input(&json!({"name": "a", "description": "d", "scope": "project"}))
                .is_err()
        );
        assert!(validate_save_input(
            &json!({"name": "../x", "description": "d", "scope": "project", "script": "x"})
        )
        .is_err());
        assert!(validate_save_input(&json!({"name": "a", "description": "d", "scope": "project", "script": "/* nc-code-workflow\ndescription: d\n*/\nx"})).is_err());
        assert!(validate_save_input(
            &json!({"name": "a", "description": "d", "scope": "project", "script": "return 1;"})
        )
        .is_ok());
        assert!(validate_save_input(
            &json!({"name": "a", "description": "d", "scope": "project", "script_path": "x"})
        )
        .is_ok());
    }
}
