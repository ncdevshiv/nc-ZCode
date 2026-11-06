//! The shared surface of the dynamic-workflow tools (port of several pure projection helpers under `core/src/tool/handlers/`).
//!
//! The ten tools must say the same thing about the same facts, so this is where they are centralised:
//!
//! - **the three readings of a script source** (inline / a `path` file / a `saved` definition) and the file read they share
//!   (the port of `workflow-path-source.ts`);
//! - **the file coordinates of diagnostics** (`workflow-script-notes.ts`) and the two draft/saved NOTE wordings;
//! - **the normalisation of the concurrency ceiling and the subagent model** (the single-provider layout of
//!   `create-workflow-source.ts` / `model-reference.ts`) — this host's model catalogue comes from `ProviderConfig` (statically known), not TS's
//!   dynamic registry, so only the part of the three-tier match that can be honoured is implemented, and when nothing resolves it is **explicitly refused** the way TS does
//!   instead of silently passing through;
//! - **the model-facing spelling of a script path** (`workflow-script-path.ts`) and artifact serialization
//!   (`serializeWorkflowArtifact`).
//!
//! Business failures (the script cannot be read, the arguments are invalid, the model cannot be resolved) always return a [`ToolError`] with code
//! `tool.invalidInput`, the same tier as TS `ToolHandlerFailure(errorCode: 400)`: these are all discriminators the model can act on
//! to change what it does, not internal faults.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use nc_code_workflow::compile::CompileDiagnostic;
use serde_json::{json, Value};

use crate::provider_config::ProviderConfig;
use crate::tools::{ToolError, ToolErrorCode};
use crate::workflow_service::saved_workflows::{
    self, SavedWorkflowArgType, SavedWorkflowArgsDeclaration, SavedWorkflowMeta,
    SavedWorkflowResolveFailure, SavedWorkflowResolveResult, SavedWorkflowScope,
    WorkflowArgsValidation, SAVED_WORKFLOW_SENTINEL,
};

// ————————————————————————————————————————————————————————————————
// Wording constants (verbatim from TS contracts; the model is their only reader)
// ————————————————————————————————————————————————————————————————

pub const CREATE_WORKFLOW_SOURCE_ERROR: &str = "Provide exactly one workflow source: `script` for a one-off script written inline, `saved` to run a workflow saved in this project, or `path` for a script file on disk (the file a previous result named). Passing more than one, or none, is ambiguous.";

pub const CREATE_WORKFLOW_ARGS_WITHOUT_PATH_ERROR: &str = "`args` belongs to the `path` source: it carries values for the arguments a script file declares in its `/* nc-code-workflow` block. For a saved workflow pass `saved.args`; an inline `script` declares no arguments, so it takes none.";

pub const AMEND_WORKFLOW_SOURCE_ERROR: &str = "Provide at most one revised script: `path` for the script file you edited (the usual form), or `script` for the whole revised script inline. Passing both is ambiguous; omit both to keep the predecessor's script unchanged.";

pub const SAVE_WORKFLOW_SOURCE_ERROR: &str = "Provide exactly one script source: `script` for the body inline, or `script_path` for the file holding it (a draft, usually). Passing both, or neither, is ambiguous.";

pub const SAVE_WORKFLOW_SENTINEL_IN_SCRIPT_ERROR: &str = "The `script` must be the workflow body only — it already starts with a `/* nc-code-workflow` metadata block. Pass the metadata through the `description` / `whenToUse` / `args` fields instead; the block is written for you.";

pub const EVAL_WORKFLOW_SNIPPET_SOURCE_ERROR: &str = "Provide exactly one snippet source: `code` for the snippet inline, or `path` for a file holding it. Passing both, or neither, is ambiguous.";

pub const DIAGNOSTICS_NOT_EXECUTED_NOTE: &str =
    "NOTE: The workflow was NOT executed — fix the errors above and resubmit.";
pub const EXECUTION_UNAVAILABLE_NOTE: &str = "NOTE: The workflow was NOT executed — workflow execution is not available in this session, so the script was only typechecked.";
pub const NOT_EXECUTED_NOTE: &str =
    "NOTE: The snippet was NOT executed — fix the errors above and call the tool again.";
pub const UNAVAILABLE_NOTE: &str =
    "NOTE: The snippet was NOT executed — snippet evaluation is not available in this session.";
pub const SUBAGENT_MODEL_UNAVAILABLE: &str =
    "This host cannot choose a subagent model; omit subagent_model.";

/// A business failure (TS's `errorCode: 400` tier: the script/arguments/name are invalid and the model can fix them).
pub fn invalid_input(message: impl Into<String>) -> ToolError {
    ToolError::new(ToolErrorCode::InvalidInput, message)
}

/// The capability is absent (the port is not wired / the journal cannot be opened). The same tier as TS's `*_unavailable`: it **never** silently
/// degrades into an empty list or "already running in the background" — the model would wait for a notification that never arrives.
pub fn capability_unavailable(message: impl Into<String>) -> ToolError {
    ToolError::new(ToolErrorCode::ExecutionFailed, message)
}

pub fn workflow_introspection_unavailable() -> ToolError {
    capability_unavailable(
        "workflow_introspection_unavailable: this session cannot read workflow runs — workflow execution is not available here, so no run history is reachable. This is a capability gap, not an empty project.",
    )
}

pub fn run_not_found(run_id: &str) -> ToolError {
    invalid_input(format!(
        "run_not_found: no workflow run with ID {run_id} exists for this project. Use ListWorkflowRuns to see the runs that do."
    ))
}

pub fn workflow_execution_unavailable() -> ToolError {
    capability_unavailable(EXECUTION_UNAVAILABLE_NOTE)
}

// ————————————————————————————————————————————————————————————————
// Script sources
// ————————————————————————————————————————————————————————————————

/// A script file read from disk (the port of `readWorkflowScriptFile`: one read, read only once).
pub struct WorkflowScriptFile {
    /// The absolute path (the journal stores it; the model surface uses [`Self::described`]).
    pub path: String,
    /// The spelling the model surface should show (a relative path when under the workspace).
    pub described: String,
    /// The raw file text.
    pub source: String,
    /// The part to execute: the body when there is a metadata block, otherwise the whole file.
    pub script: String,
    /// The declarations parsed out of the metadata block; `None` when there is no block.
    pub meta: Option<SavedWorkflowMeta>,
    /// The number of lines before the body (0 when there is no block).
    pub body_line_offset: usize,
}

fn resolve_script_file_path(cwd: &Path, input_path: &str) -> PathBuf {
    let candidate = Path::new(input_path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    }
}

/// Whether the first non-empty line is the start marker (verbatim consistent with the parser's own entry: if the two places answer
/// differently about "is this a saved definition", a dead corner appears where "the tool says there is no block, the parser says the block is corrupt").
fn starts_with_sentinel(source: &str) -> bool {
    for line in source.split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        return line.trim() == SAVED_WORKFLOW_SENTINEL;
    }
    false
}

/// Reads a script file. An unreadable file or a corrupt metadata block both return a business failure **naming the file** (without the name the model would only
/// assume it just mistyped something and retry the same input).
pub fn read_workflow_script_file(
    cwd: &Path,
    input_path: &str,
    parse_frontmatter: bool,
) -> Result<WorkflowScriptFile, ToolError> {
    let absolute = resolve_script_file_path(cwd, input_path);
    let described = describe_script_path(&absolute, Some(cwd));
    let source = std::fs::read_to_string(&absolute).map_err(|error| {
        invalid_input(format!(
            "The workflow script file {described} could not be read: {error}. Pass `path` for a file that exists, or submit the script inline."
        ))
    })?;
    let path = absolute.to_string_lossy().to_string();
    if !parse_frontmatter || !starts_with_sentinel(&source) {
        return Ok(WorkflowScriptFile {
            path,
            described,
            script: source.clone(),
            source,
            meta: None,
            body_line_offset: 0,
        });
    }
    let parsed = saved_workflows::parse_saved_workflow(&source).map_err(|error| {
        invalid_input(format!(
            "The workflow script file {described} starts with a `{SAVED_WORKFLOW_SENTINEL}` metadata block that could not be read ({}): {}. Fix the block in that file, or remove it and pass the script alone.",
            error.reason, error.detail
        ))
    })?;
    Ok(WorkflowScriptFile {
        path,
        described,
        source,
        script: parsed.script,
        meta: Some(parsed.meta),
        body_line_offset: parsed.body_line_offset,
    })
}

/// The script file's identity on the model surface (`WorkflowScriptLocation`).
#[derive(Clone, Debug)]
pub struct ScriptLocation {
    /// `draft` = the copy the tool just wrote ("saved at"); `path` = the file the model supplied itself.
    pub kind: &'static str,
    pub described: String,
    pub line_offset: usize,
}

/// `workflow-script-path.ts`: a relative path when under the session's working directory, otherwise the absolute path as is.
pub fn describe_script_path(absolute: &Path, cwd: Option<&Path>) -> String {
    let Some(cwd) = cwd.filter(|cwd| !cwd.as_os_str().is_empty()) else {
        return absolute.to_string_lossy().to_string();
    };
    match absolute.strip_prefix(cwd) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative.to_string_lossy().to_string(),
        // Both an empty result (the path equals the working directory exactly) and a cross-drive / outside-directory path fall back to the absolute path: an empty string is not a
        // filename you can `Edit`, and a relative spelling starting with `..` reads as if it were inside the workspace.
        _ => absolute.to_string_lossy().to_string(),
    }
}

/// The model-facing diagnostic line: with a file present it is reported by **file line** (the line number must be pasteable straight into an `Edit`).
pub fn format_diagnostic_lines(
    diagnostics: &[CompileDiagnostic],
    location: Option<&ScriptLocation>,
) -> Vec<String> {
    diagnostics
        .iter()
        .map(|diagnostic| match location {
            Some(location) => format!(
                "{}:L{}:C{} {}",
                location.described,
                diagnostic.line as usize + location.line_offset,
                diagnostic.column,
                diagnostic.message
            ),
            None => format!(
                "L{}:C{} {}",
                diagnostic.line, diagnostic.column, diagnostic.message
            ),
        })
        .collect()
}

pub fn script_file_note(location: &ScriptLocation) -> String {
    let where_ = if location.kind == "draft" {
        format!("The script is saved at {}.", location.described)
    } else {
        format!("The script file is {}.", location.described)
    };
    format!(
        "NOTE: The workflow was NOT executed. {where_} Edit that file in place and resubmit with `path: \"{}\"` — do not paste the script inline again.",
        location.described
    )
}

pub fn saved_draft_note(saved_name: &str, saved_path: &str, draft: &str) -> String {
    format!(
        "NOTE: The workflow was NOT executed. A working copy of the saved workflow '{saved_name}' ({saved_path}) was written to {draft}. Edit that copy in place and resubmit with `path: \"{draft}\"` (pass its `args` again); to change the saved definition itself, use SaveWorkflow."
    )
}

pub fn launched_script_sentence(location: &ScriptLocation) -> String {
    let where_ = if location.kind == "draft" {
        format!("The script is saved at {}", location.described)
    } else {
        format!("The script file is {}", location.described)
    };
    format!(" {where_}; to revise it later, edit that file and pass `path` to AmendWorkflow.")
}

pub fn amended_script_sentence(location: &ScriptLocation) -> String {
    format!(
        " The revision's script is at {}; edit it there for a further revision.",
        location.described
    )
}

/// The concurrency ceiling's one sentence inside the result wording. When `limit == ceiling` it names this machine's maximum — the number the model
/// asked for was lowered, and without saying so it would treat "at most 32" as already in effect and repeat a false number when the user asks.
pub fn describe_concurrency_limit(limit: Option<u32>, ceiling: Option<u32>) -> String {
    let Some(limit) = limit else {
        return String::new();
    };
    let subject = if limit == 1 {
        "1 subagent runs".to_string()
    } else {
        format!("{limit} subagents run")
    };
    let capped = ceiling == Some(limit);
    format!(
        " At most {subject} at once{}.",
        if capped {
            " (this machine's maximum)"
        } else {
            ""
        }
    )
}

pub fn describe_subagent_model(canonical: Option<&str>) -> String {
    match canonical {
        None => String::new(),
        Some(canonical) => {
            format!(" Subagents run on {canonical} (the main agent stays on the session model).")
        }
    }
}

/// A compact restatement of the argument declarations, appended after an argument validation failure so the model fixes it in one pass instead of guessing another round.
pub fn describe_args_declaration(
    label: &str,
    args: Option<&SavedWorkflowArgsDeclaration>,
) -> String {
    let Some(args) = args.filter(|args| !args.is_empty()) else {
        return format!("{label} declares no arguments.");
    };
    let mut lines = vec![format!("{label} declares:")];
    for (key, spec) in args {
        let mut notes = vec![arg_type_str(spec.arg_type).to_string()];
        notes.push(if spec.required == Some(true) {
            "required".to_string()
        } else {
            "optional".to_string()
        });
        if let Some(default) = &spec.default {
            notes.push(format!("default {default}"));
        }
        let description = spec
            .description
            .as_deref()
            .map(|description| format!(": {description}"))
            .unwrap_or_default();
        lines.push(format!("- {key} ({}){description}", notes.join(", ")));
    }
    lines.join("\n")
}

fn arg_type_str(arg_type: SavedWorkflowArgType) -> &'static str {
    match arg_type {
        SavedWorkflowArgType::String => "string",
        SavedWorkflowArgType::Number => "number",
        SavedWorkflowArgType::Boolean => "boolean",
        SavedWorkflowArgType::Json => "json",
    }
}

/// A saved definition failing to resolve → a business failure. When it is not found the **actually available names are listed** (with scope labels): after the model
/// guesses a name wrong, the most useful next-step information is exactly the correct set.
pub fn saved_resolve_failure(
    cwd: &Path,
    name: &str,
    scope: Option<SavedWorkflowScope>,
    failure: SavedWorkflowResolveFailure,
) -> ToolError {
    match failure {
        SavedWorkflowResolveFailure::InvalidName(detail) => {
            invalid_input(format!("'{name}' is not a usable workflow name: {detail}"))
        }
        SavedWorkflowResolveFailure::ParseError { path, reason, detail } => invalid_input(format!(
            "The saved workflow '{name}' at {path} could not be read: {reason}: {detail}\n\nIts metadata block is malformed — most likely hand-edited. Fix the file, or save the workflow again."
        )),
        SavedWorkflowResolveFailure::ReadError { path, detail } => invalid_input(format!(
            "The saved workflow '{name}' at {path} could not be read: {detail}"
        )),
        SavedWorkflowResolveFailure::NotFound => {
            let headline = match scope {
                None => format!("No saved workflow named '{name}' in this project or globally."),
                Some(scope) => format!("No {} workflow named '{name}'.", scope.as_str()),
            };
            invalid_input(format!("{headline}\n\n{}", describe_available_workflows(cwd)))
        }
    }
}

/// Each of the two archives is scanned on its own (targeted scan, no shadowing applied), so a global definition hidden by the project archive still appears in the list.
fn describe_available_workflows(cwd: &Path) -> String {
    let project =
        saved_workflows::list_saved_workflows(cwd, Some(SavedWorkflowScope::Project), None);
    let global = saved_workflows::list_saved_workflows(cwd, Some(SavedWorkflowScope::Global), None);
    let mut tagged: Vec<String> = project
        .entries
        .iter()
        .map(|entry| format!("{} (project)", entry.name))
        .collect();
    tagged.extend(
        global
            .entries
            .iter()
            .map(|entry| format!("{} (global)", entry.name)),
    );
    if tagged.is_empty() {
        return "There are no saved workflows yet, in this project or globally. Use SaveWorkflow to create one, or pass an inline `script` instead.".to_string();
    }
    format!(
        "Available saved workflows: {}. Use ListSavedWorkflows for their descriptions.",
        tagged.join(", ")
    )
}

/// Validates against the declarations and fills in defaults; on failure it lists **all** violations at once.
pub fn validate_args(
    declaration: Option<&SavedWorkflowArgsDeclaration>,
    provided: Option<&serde_json::Map<String, Value>>,
) -> Result<serde_json::Map<String, Value>, ToolError> {
    match saved_workflows::validate_workflow_args(declaration, provided) {
        WorkflowArgsValidation::Ok(args) => Ok(args.into_iter().collect()),
        WorkflowArgsValidation::Errors(errors) => Err(invalid_input(errors.join("\n"))),
    }
}

/// Writes a draft (the port of `writeWorkflowDraft`: mints a new file every time, exclusive create, returns None when the write fails).
pub fn write_draft(cwd: &Path, name: &str, source: &str) -> Option<String> {
    saved_workflows::write_workflow_draft(Some(cwd), name, source)
        .map(|path| path.to_string_lossy().to_string())
}

/// Without a name the script's first `phase("…")` is taken — the draft name aligns with the model surface's "edit this file".
pub fn draft_name(name: Option<&str>, phase_names: &[String]) -> String {
    match name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => name.to_string(),
        None => phase_names
            .first()
            .cloned()
            .unwrap_or_else(|| "workflow".to_string()),
    }
}

/// The declaration table of named phases inside the causality graph (the table shared by TS `createWorkflowPhaseNames` and
/// `createWorkflowPhaseAlongside`): names in declaration order cut to 32; the alongside table is aligned with it
/// by index and only keeps references pointing at other phases **inside this table** (self-references, unknown ids and duplicates are dropped). When both groups
/// are empty alongside is absent entirely — "no parallelism" is a single straight track.
pub fn phase_table(graph: Option<&Value>) -> (Vec<String>, Option<Vec<Vec<u32>>>) {
    let Some(phases) = graph
        .and_then(|graph| graph.get("phases"))
        .and_then(Value::as_array)
    else {
        return (Vec::new(), None);
    };
    let named: Vec<&Value> = phases
        .iter()
        .filter(|phase| {
            phase
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| !name.is_empty())
        })
        .take(32)
        .collect();
    if named.is_empty() {
        return (Vec::new(), None);
    }
    let index_of: HashMap<&str, u32> = named
        .iter()
        .enumerate()
        .map(|(index, phase)| {
            (
                phase.get("id").and_then(Value::as_str).unwrap_or_default(),
                index as u32,
            )
        })
        .collect();
    let mut any = false;
    let alongside: Vec<Vec<u32>> = named
        .iter()
        .enumerate()
        .map(|(self_index, phase)| {
            let mut indexes: Vec<u32> = Vec::new();
            for id in phase
                .get("alongside")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[])
            {
                let Some(index) = id.as_str().and_then(|id| index_of.get(id).copied()) else {
                    continue;
                };
                if index as usize == self_index || indexes.contains(&index) {
                    continue;
                }
                indexes.push(index);
            }
            any |= !indexes.is_empty();
            indexes
        })
        .collect();
    let names: Vec<String> = named
        .iter()
        .map(|phase| {
            phase
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    (names, any.then_some(alongside))
}

// ————————————————————————————————————————————————————————————————
// The normalisation of the subagent model (the single-provider layout of `model-reference.ts`)
// ————————————————————————————————————————————————————————————————

/// One model catalogue row (this host's subset of TS `ModelCatalogEntry`).
pub struct CatalogEntry {
    pub provider_id: Option<String>,
    pub model_id: String,
    pub current: bool,
    pub reasoning_levels: Vec<String>,
    pub default_reasoning_level: Option<String>,
    pub disabled_reason: Option<String>,
}

/// `ProviderConfig` → the model catalogue. TS's catalogue includes account / gateway dynamic delivery; this host only has the statically known
/// copy inside the configuration (the same source as ListModels), so that is the optional surface of a resolved result.
pub fn catalog_from_provider(config: &ProviderConfig) -> Vec<CatalogEntry> {
    config
        .model_catalog()
        .into_iter()
        .map(|model_id| CatalogEntry {
            // A single-provider config has no registry id: kind is used as providerId so that ListModels' output
            // canonical form `providerId/modelId` and the resolver accept the same key (a bare model id still matches).
            provider_id: config
                .provider_id
                .clone()
                .or_else(|| (!config.kind.is_empty()).then(|| config.kind.clone())),
            current: model_id == config.model,
            model_id,
            reasoning_levels: config.reasoning_levels.clone(),
            // ProviderConfig has no "default tier" column; absent means the canonical carries no `$level`.
            default_reasoning_level: None,
            disabled_reason: None,
        })
        .collect()
}

fn same_token(left: &str, right: &str) -> bool {
    left.trim().to_lowercase() == right.trim().to_lowercase()
}

fn format_catalog_id(entry: &CatalogEntry) -> String {
    match &entry.provider_id {
        Some(provider) if !provider.is_empty() => format!("{provider}/{}", entry.model_id),
        _ => entry.model_id.clone(),
    }
}

/// `providerId/modelId[$reasoning]` → the canonical form, or a business failure carrying the candidate list.
///
/// The three-tier match goes from highest specificity downwards (the order is part of the specification): exact full name → exactly one bare-id hit → when a bare id
/// hits several, take the current session's entry, otherwise ambiguous. All case-insensitive; a disabled entry is never silently matched.
pub fn resolve_model_reference(text: &str, entries: &[CatalogEntry]) -> Result<String, ToolError> {
    let (reference, level) = split_reasoning_level(text.trim());
    let provider_separator = reference.find('/');
    let matches: Vec<&CatalogEntry> = match provider_separator {
        Some(index) if index > 0 => {
            let provider = &reference[..index];
            let model = &reference[index + 1..];
            entries
                .iter()
                .filter(|entry| {
                    entry
                        .provider_id
                        .as_deref()
                        .map(|candidate| same_token(candidate, provider))
                        .unwrap_or(false)
                        && same_token(&entry.model_id, model)
                })
                .collect()
        }
        _ => entries
            .iter()
            .filter(|entry| same_token(&entry.model_id, reference))
            .collect(),
    };
    if matches.is_empty() {
        return Err(not_found_model(text, entries));
    }
    let enabled: Vec<&CatalogEntry> = matches
        .iter()
        .copied()
        .filter(|entry| entry.disabled_reason.is_none())
        .collect();
    if enabled.is_empty() {
        let lines: Vec<String> = matches
            .iter()
            .map(|entry| {
                format!(
                    "{} — {}",
                    format_catalog_id(entry),
                    entry.disabled_reason.as_deref().unwrap_or("unavailable")
                )
            })
            .collect();
        return Err(invalid_input(format!(
            "`{text}` matches a model that cannot be used on this host:\n{}\n\nResolve that with the user, or call ListModels and pick another id.",
            lines.join("\n")
        )));
    }
    let entry = if enabled.len() == 1 {
        enabled[0]
    } else {
        enabled
            .iter()
            .find(|entry| entry.current)
            .copied()
            .ok_or_else(|| {
                let lines: Vec<String> = enabled.iter().map(|entry| format_catalog_id(entry)).collect();
                invalid_input(format!(
                    "`{text}` is configured under more than one provider:\n{}\n\nPass the full `providerId/modelId` of the one you want.",
                    lines.join("\n")
                ))
            })?
    };
    let level = match level {
        Some(level) => {
            let matched = entry
                .reasoning_levels
                .iter()
                .find(|candidate| same_token(candidate, level));
            match matched {
                Some(matched) => Some(matched.clone()),
                None => {
                    let id = format_catalog_id(entry);
                    let levels = if entry.reasoning_levels.is_empty() {
                        format!("{id} has no reasoning levels — drop the `$` suffix.")
                    } else {
                        format!(
                            "Its levels are: {}.{}",
                            entry.reasoning_levels.join(", "),
                            entry
                                .default_reasoning_level
                                .as_deref()
                                .map(|default| format!(" Omit the suffix to use {default}."))
                                .unwrap_or_default()
                        )
                    };
                    return Err(invalid_input(format!(
                        "`{level}` is not a reasoning level of {id}. {levels}"
                    )));
                }
            }
        }
        None => entry.default_reasoning_level.clone(),
    };
    let base = format_catalog_id(entry);
    Ok(match level {
        Some(level) => format!("{base}${level}"),
        None => base,
    })
}

/// Splits out `$level`: the search start follows the provider separator; an empty side (`$high`, `glm$`) does not count as a level.
fn split_reasoning_level(text: &str) -> (&str, Option<&str>) {
    let search_from = text.find('/').map(|index| index + 1).unwrap_or(0);
    match text[search_from..].find('$') {
        Some(offset) => {
            let index = search_from + offset;
            if index <= search_from || index >= text.len() - 1 {
                (text, None)
            } else {
                (&text[..index], Some(&text[index + 1..]))
            }
        }
        None => (text, None),
    }
}

fn not_found_model(text: &str, entries: &[CatalogEntry]) -> ToolError {
    const MAX_CANDIDATE_LINES: usize = 40;
    let candidates: Vec<&CatalogEntry> = entries
        .iter()
        .filter(|entry| entry.disabled_reason.is_none())
        .collect();
    let shown = &candidates[..candidates.len().min(MAX_CANDIDATE_LINES)];
    let mut lines: Vec<String> = shown
        .iter()
        .map(|entry| {
            format!(
                "{}{}",
                format_catalog_id(entry),
                if entry.current { " [current]" } else { "" }
            )
        })
        .collect();
    if candidates.len() > shown.len() {
        lines.push(format!("… and {} more.", candidates.len() - shown.len()));
    }
    let body = if candidates.is_empty() {
        "No models are configured on this host.".to_string()
    } else {
        format!("Available models:\n{}", lines.join("\n"))
    };
    invalid_input(format!(
        "No configured model matches `{text}`. {body}\n\nPass one of these ids, or call ListModels."
    ))
}

/// The clamping of the concurrency ceiling: `[1, ceiling]`. A value above the ceiling is **lowered rather than refused** (when the model says
/// "at most 32" the user wants an upper bound, not an error).
pub fn clamp_max_concurrency(value: Option<u32>, ceiling: Option<u32>) -> Option<u32> {
    value.map(|value| {
        let at_least_one = value.max(1);
        match ceiling {
            Some(ceiling) => at_least_one.min(ceiling.max(1)),
            None => at_least_one,
        }
    })
}

// ————————————————————————————————————————————————————————————————
// Relative age (`formatRelativeAge`)
// ————————————————————————————————————————————————————————————————

/// "How long ago". When `at` is absent or not a finite number it returns `None` — the read side **omits the whole age clause** accordingly instead of
/// rendering a 0 or "unknown": an old journal without a timestamp cannot state an age, and that is a fact.
pub fn format_relative_age(now: f64, at: Option<f64>) -> Option<String> {
    let at = at.filter(|at| at.is_finite())?;
    if !now.is_finite() {
        return None;
    }
    let total = (now - at).max(0.0);
    let second = 1_000.0;
    let minute = 60_000.0;
    let hour = 3_600_000.0;
    let day = 86_400_000.0;
    let text = if total < minute {
        format!("{}s", (total / second).floor() as u64)
    } else if total < hour {
        format!(
            "{}m {:02}s",
            (total / minute).floor() as u64,
            ((total % minute) / second).floor() as u64
        )
    } else if total < day {
        format!(
            "{}h {:02}m",
            (total / hour).floor() as u64,
            ((total % hour) / minute).floor() as u64
        )
    } else {
        format!(
            "{}d {}h",
            (total / day).floor() as u64,
            ((total % day) / hour).floor() as u64
        )
    };
    Some(format!("{text} ago"))
}

// ————————————————————————————————————————————————————————————————
// Artifact serialization / saved scope
// ————————————————————————————————————————————————————————————————

/// `serializeWorkflowArtifact`: a string verbatim; everything else pretty JSON; `undefined` → None.
pub fn serialize_artifact(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Null => Some("null".to_string()),
        other => Some(serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string())),
    }
}

/// Looks up a saved definition directly (the `saved` source). A resolution failure carries "which ones are available".
pub fn resolve_saved(
    cwd: &Path,
    name: &str,
    scope: Option<SavedWorkflowScope>,
) -> Result<saved_workflows::ResolvedSavedWorkflow, ToolError> {
    match saved_workflows::resolve_saved_workflow(cwd, name, scope, None) {
        SavedWorkflowResolveResult::Ok(resolved) => Ok(resolved),
        SavedWorkflowResolveResult::Err(failure) => {
            Err(saved_resolve_failure(cwd, name, scope, failure))
        }
    }
}

/// The `SavedWorkflowMeta` fields used inside `json!(...)` (kept in camelCase).
pub fn meta_to_json(meta: &SavedWorkflowMeta) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("description".to_string(), json!(meta.description));
    if let Some(when_to_use) = &meta.when_to_use {
        object.insert("whenToUse".to_string(), json!(when_to_use));
    }
    if let Some(args) = &meta.args {
        object.insert(
            "args".to_string(),
            serde_json::to_value(args).unwrap_or(Value::Null),
        );
    }
    Value::Object(object)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_table_missing_graph_or_phases_is_absent() {
        assert_eq!(phase_table(None), (Vec::new(), None));
        let (names, alongside) = phase_table(Some(&json!({"steps": []})));
        assert!(names.is_empty());
        assert_eq!(alongside, None);
    }

    #[test]
    fn phase_table_skips_unnamed_phases_and_keeps_declaration_order() {
        let graph = json!({"phases": [
            {"id": "unphased"},
            {"id": "phase#1", "name": "plan"},
            {"id": "phase#2", "name": "execute"},
            {"id": "phase#3", "name": ""}
        ]});
        let (names, alongside) = phase_table(Some(&graph));
        assert_eq!(names, vec!["plan", "execute"]);
        // Without alongside references the whole field is absent, not a run of empty arrays.
        assert_eq!(alongside, None);
    }

    #[test]
    fn phase_table_alongside_uses_named_index_space() {
        let graph = json!({"phases": [
            {"id": "unphased"},
            {"id": "phase#1", "name": "plan"},
            {"id": "phase#2", "name": "fan-a", "alongside": ["phase#3", "phase#2", "phase#9", "phase#3"]},
            {"id": "phase#3", "name": "fan-b", "alongside": ["phase#2"]}
        ]});
        let (names, alongside) = phase_table(Some(&graph));
        assert_eq!(names, vec!["plan", "fan-a", "fan-b"]);
        // Self-references, unnamed phases, ids outside the table and duplicates are all dropped; the indices land inside the named table.
        assert_eq!(alongside, Some(vec![vec![], vec![2], vec![1]]));
    }
}
