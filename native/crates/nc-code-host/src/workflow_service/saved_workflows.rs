//! Saved workflow store (port of `core/src/tool/handlers/saved-workflows/{store,frontmatter,args}.ts`
//! and `workflow-drafts.ts`).
//!
//! File shape (structurally identical to TS, field for field): a block-comment frontmatter + the
//! script source.
//!
//! ```text
//! /* nc-code-workflow
//! description: ...
//! args:
//!   pr: { type: string, required: true }
//! */
//! <plain dwf script>
//! ```
//!
//! The `.dwf.ts` extension keeps editors highlighting it as TypeScript; a `---` fence would turn the
//! first line into a syntax error.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SAVED_WORKFLOW_FILE_EXTENSION: &str = ".dwf.ts";
pub const SAVED_WORKFLOW_PROJECT_DIR: &str = ".zcode/workflows";
pub const SAVED_WORKFLOW_GLOBAL_DIR: &str = ".zcode/workflows";
pub const WORKFLOW_DRAFTS_DIR: &str = ".zcode/workflow-drafts";
pub const SAVED_WORKFLOW_SENTINEL: &str = "/* nc-code-workflow";
pub const SAVED_WORKFLOW_MAX_NAME_CHARS: usize = 64;
const WORKFLOW_DRAFT_MAX_SLUG_CHARS: usize = 64;
const WORKFLOW_DRAFT_FALLBACK_SLUG: &str = "workflow";
const WORKFLOW_DRAFT_MAX_ATTEMPTS: u32 = 1_000;

/// Whether a name is usable as a file name (this check is itself the path-traversal defense).
pub fn is_valid_saved_workflow_name(name: &str) -> bool {
    if name.is_empty() || name.chars().count() > SAVED_WORKFLOW_MAX_NAME_CHARS {
        return false;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return false;
    }
    // `.` and `..` pass the charset check but they are directory entries, not names.
    name.chars().any(|c| c != '.')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SavedWorkflowScope {
    Project,
    Global,
}

impl SavedWorkflowScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Global => "global",
        }
    }
}

/// The shadowing fact (shown in the confirmation window).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SavedWorkflowShadowing {
    HidesGlobal,
    HiddenByProject,
}

impl SavedWorkflowShadowing {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::HidesGlobal => "hides_global",
            Self::HiddenByProject => "hidden_by_project",
        }
    }
}

/// Argument type vocabulary: three primitives plus the `json` fallback (`json` = explicitly not validated).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SavedWorkflowArgType {
    String,
    Number,
    Boolean,
    Json,
}

/// A single argument declaration. `default` is arbitrary JSON (after applying it, the value goes through the same validation).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedWorkflowArgDeclaration {
    #[serde(rename = "type")]
    pub arg_type: SavedWorkflowArgType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

pub type SavedWorkflowArgsDeclaration = BTreeMap<String, SavedWorkflowArgDeclaration>;

/// Frontmatter metadata (strict: a misspelled key becomes a visible invalid row).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedWorkflowMeta {
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "whenToUse")]
    pub when_to_use: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<SavedWorkflowArgsDeclaration>,
}

/// One row of the listing (without the script body).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedWorkflowEntry {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<SavedWorkflowArgsDeclaration>,
    pub scope: SavedWorkflowScope,
    pub path: String,
}

/// A file that exists but cannot be read (one broken file must not fail the whole listing).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SavedWorkflowInvalidEntry {
    pub path: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SavedWorkflowListResult {
    pub entries: Vec<SavedWorkflowEntry>,
    pub invalid: Vec<SavedWorkflowInvalidEntry>,
}

/// One lookup root: a scope label plus an absolute directory.
#[derive(Clone, Debug, PartialEq)]
pub struct SavedWorkflowRoot {
    pub scope: SavedWorkflowScope,
    pub dir: PathBuf,
}

/// This session's lookup roots, in priority order: `[project, global]` (a project file always beats a global one of the same name).
pub fn saved_workflow_roots(cwd: &Path, home_dir: Option<&Path>) -> Vec<SavedWorkflowRoot> {
    let home = home_dir.map(Path::to_path_buf).unwrap_or_else(|| {
        std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
    });
    vec![
        SavedWorkflowRoot {
            scope: SavedWorkflowScope::Project,
            dir: cwd.join(SAVED_WORKFLOW_PROJECT_DIR),
        },
        SavedWorkflowRoot {
            scope: SavedWorkflowScope::Global,
            dir: home.join(SAVED_WORKFLOW_GLOBAL_DIR),
        },
    ]
}

fn saved_workflow_root(
    cwd: &Path,
    scope: SavedWorkflowScope,
    home_dir: Option<&Path>,
) -> SavedWorkflowRoot {
    saved_workflow_roots(cwd, home_dir)
        .into_iter()
        .find(|root| root.scope == scope)
        .expect("roots cover both scopes")
}

pub fn saved_workflow_file_name(name: &str) -> String {
    format!("{name}{SAVED_WORKFLOW_FILE_EXTENSION}")
}

pub fn saved_workflow_path(root: &SavedWorkflowRoot, name: &str) -> PathBuf {
    root.dir.join(saved_workflow_file_name(name))
}

/// The four reasons a parse failed (each carries its own advice; they are never merged into "parse error").
#[derive(Clone, Debug, PartialEq)]
pub enum SavedWorkflowResolveFailure {
    InvalidName(String),
    NotFound,
    ParseError {
        path: String,
        reason: &'static str,
        detail: String,
    },
    ReadError {
        path: String,
        detail: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedSavedWorkflow {
    pub name: String,
    pub path: String,
    pub scope: SavedWorkflowScope,
    pub meta: SavedWorkflowMeta,
    pub script: String,
    /// The file's exact bytes (the draft copy uses them, avoiding drift from re-serializing).
    pub source: String,
    /// How many lines precede the body (`fileLine = bodyLine + bodyLineOffset`).
    pub body_line_offset: usize,
}

pub enum SavedWorkflowResolveResult {
    Ok(ResolvedSavedWorkflow),
    Err(SavedWorkflowResolveFailure),
}

fn read_text(path: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

fn is_not_found(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Resolve a saved workflow by name (no scope given: the two roots are tried in order, first wins).
pub fn resolve_saved_workflow(
    cwd: &Path,
    name: &str,
    scope: Option<SavedWorkflowScope>,
    home_dir: Option<&Path>,
) -> SavedWorkflowResolveResult {
    if !is_valid_saved_workflow_name(name) {
        return SavedWorkflowResolveResult::Err(SavedWorkflowResolveFailure::InvalidName(format!(
            "workflow names may only contain letters, digits, '.', '-' and '_', and must be 1-{SAVED_WORKFLOW_MAX_NAME_CHARS} characters"
        )));
    }
    let roots = match scope {
        Some(scope) => vec![saved_workflow_root(cwd, scope, home_dir)],
        None => saved_workflow_roots(cwd, home_dir),
    };
    for root in roots {
        let path = saved_workflow_path(&root, name);
        let source = match read_text(&path) {
            Ok(source) => source,
            Err(error) if is_not_found(&error) => continue,
            Err(error) => {
                return SavedWorkflowResolveResult::Err(SavedWorkflowResolveFailure::ReadError {
                    path: path.to_string_lossy().to_string(),
                    detail: error.to_string(),
                })
            }
        };
        match parse_saved_workflow(&source) {
            Ok(parsed) => {
                return SavedWorkflowResolveResult::Ok(ResolvedSavedWorkflow {
                    name: name.to_string(),
                    path: path.to_string_lossy().to_string(),
                    scope: root.scope,
                    meta: parsed.meta,
                    script: parsed.script,
                    source,
                    body_line_offset: parsed.body_line_offset,
                })
            }
            Err(parse_error) => {
                return SavedWorkflowResolveResult::Err(SavedWorkflowResolveFailure::ParseError {
                    path: path.to_string_lossy().to_string(),
                    reason: parse_error.reason,
                    detail: parse_error.detail,
                })
            }
        }
    }
    SavedWorkflowResolveResult::Err(SavedWorkflowResolveFailure::NotFound)
}

/// Enumerate saved definitions: depth-1 flat, sorted by file name, first-wins on a name clash (a shadowed one is not listed).
pub fn list_saved_workflows(
    cwd: &Path,
    scope: Option<SavedWorkflowScope>,
    home_dir: Option<&Path>,
) -> SavedWorkflowListResult {
    let mut result = SavedWorkflowListResult::default();
    let mut claimed = std::collections::HashSet::new();
    let roots = match scope {
        Some(scope) => vec![saved_workflow_root(cwd, scope, home_dir)],
        None => saved_workflow_roots(cwd, home_dir),
    };
    for root in roots {
        let entries = match std::fs::read_dir(&root.dir) {
            Ok(entries) => entries,
            Err(error) if is_not_found(&error) => continue,
            Err(error) => {
                result.invalid.push(SavedWorkflowInvalidEntry {
                    path: root.dir.to_string_lossy().to_string(),
                    reason: error.to_string(),
                });
                continue;
            }
        };
        let mut file_names: Vec<String> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        // readdir order depends on the filesystem; sorting keeps the listing identical on two machines.
        file_names.sort();
        for file_name in file_names {
            let Some(name) = file_name.strip_suffix(SAVED_WORKFLOW_FILE_EXTENSION) else {
                continue;
            };
            let path = root.dir.join(&file_name);
            if !is_valid_saved_workflow_name(name) {
                result.invalid.push(SavedWorkflowInvalidEntry {
                    path: path.to_string_lossy().to_string(),
                    reason: "file name is not a usable workflow name".to_string(),
                });
                continue;
            }
            if claimed.contains(name) {
                continue;
            }
            let source = match read_text(&path) {
                Ok(source) => source,
                Err(error) => {
                    result.invalid.push(SavedWorkflowInvalidEntry {
                        path: path.to_string_lossy().to_string(),
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            match parse_saved_workflow(&source) {
                Ok(parsed) => {
                    claimed.insert(name.to_string());
                    result.entries.push(SavedWorkflowEntry {
                        name: name.to_string(),
                        description: parsed.meta.description.clone(),
                        when_to_use: parsed.meta.when_to_use.clone(),
                        args: parsed.meta.args.clone(),
                        scope: root.scope,
                        path: path.to_string_lossy().to_string(),
                    });
                }
                Err(parse_error) => result.invalid.push(SavedWorkflowInvalidEntry {
                    path: path.to_string_lossy().to_string(),
                    reason: format!("{}: {}", parse_error.reason, parse_error.detail),
                }),
            }
        }
    }
    result
}

/// Write one saved definition, returning (destination, scope, wasOverwrite).
pub fn save_saved_workflow(
    cwd: &Path,
    name: &str,
    meta: &SavedWorkflowMeta,
    script: &str,
    scope: Option<SavedWorkflowScope>,
    home_dir: Option<&Path>,
) -> std::io::Result<(String, SavedWorkflowScope, bool)> {
    let root = saved_workflow_root(cwd, scope.unwrap_or(SavedWorkflowScope::Project), home_dir);
    let path = saved_workflow_path(&root, name);
    std::fs::create_dir_all(&root.dir)?;
    let overwritten = path.is_file();
    std::fs::write(&path, serialize_saved_workflow(meta, script))?;
    Ok((path.to_string_lossy().to_string(), root.scope, overwritten))
}

/// Whether the target already exists (the confirmation window must say "overwrite" and "create" as two different things).
pub fn saved_workflow_exists(
    cwd: &Path,
    name: &str,
    scope: Option<SavedWorkflowScope>,
    home_dir: Option<&Path>,
) -> bool {
    if !is_valid_saved_workflow_name(name) {
        return false;
    }
    let root = saved_workflow_root(cwd, scope.unwrap_or(SavedWorkflowScope::Project), home_dir);
    saved_workflow_path(&root, name).is_file()
}

/// Whether the other scope already holds a definition of this name when saving into `scope` (the shadowing fact).
pub fn find_saved_workflow_shadowing(
    cwd: &Path,
    name: &str,
    scope: SavedWorkflowScope,
    home_dir: Option<&Path>,
) -> Option<SavedWorkflowShadowing> {
    if !is_valid_saved_workflow_name(name) {
        return None;
    }
    let other_scope = match scope {
        SavedWorkflowScope::Project => SavedWorkflowScope::Global,
        SavedWorkflowScope::Global => SavedWorkflowScope::Project,
    };
    let other_root = saved_workflow_root(cwd, other_scope, home_dir);
    if !saved_workflow_path(&other_root, name).is_file() {
        return None;
    }
    Some(match scope {
        SavedWorkflowScope::Project => SavedWorkflowShadowing::HidesGlobal,
        SavedWorkflowScope::Global => SavedWorkflowShadowing::HiddenByProject,
    })
}

/// The result of a byte-for-byte global→project move.
#[derive(Clone, Debug, PartialEq)]
pub enum SavedWorkflowMoveResult {
    Ok { from: String, to: String },
    InvalidName(String),
    NotFound,
    TargetExists(String),
    ReadError { path: String, detail: String },
    WriteError { path: String, detail: String },
}

/// Move `name` from the global root to the project root (that direction only; byte for byte, no parse and no re-ordering).
pub fn move_saved_workflow(
    cwd: &Path,
    name: &str,
    home_dir: Option<&Path>,
) -> SavedWorkflowMoveResult {
    if !is_valid_saved_workflow_name(name) {
        return SavedWorkflowMoveResult::InvalidName(format!(
            "workflow names may only contain letters, digits, '.', '-' and '_', and must be 1-{SAVED_WORKFLOW_MAX_NAME_CHARS} characters"
        ));
    }
    let from_root = saved_workflow_root(cwd, SavedWorkflowScope::Global, home_dir);
    let to_root = saved_workflow_root(cwd, SavedWorkflowScope::Project, home_dir);
    let from_path = saved_workflow_path(&from_root, name);
    let to_path = saved_workflow_path(&to_root, name);
    if !from_path.is_file() {
        return SavedWorkflowMoveResult::NotFound;
    }
    if to_path.is_file() {
        return SavedWorkflowMoveResult::TargetExists(to_path.to_string_lossy().to_string());
    }
    if let Err(error) = std::fs::create_dir_all(&to_root.dir) {
        return SavedWorkflowMoveResult::WriteError {
            path: to_path.to_string_lossy().to_string(),
            detail: error.to_string(),
        };
    }
    match std::fs::rename(&from_path, &to_path) {
        Ok(()) => {
            return SavedWorkflowMoveResult::Ok {
                from: from_path.to_string_lossy().to_string(),
                to: to_path.to_string_lossy().to_string(),
            }
        }
        Err(error) if error.raw_os_error() != Some(18 /* EXDEV */) => {
            return SavedWorkflowMoveResult::WriteError {
                path: to_path.to_string_lossy().to_string(),
                detail: error.to_string(),
            }
        }
        Err(_) => {}
    }
    // Across devices: a read → write → delete fallback move.
    let bytes = match std::fs::read(&from_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return SavedWorkflowMoveResult::ReadError {
                path: from_path.to_string_lossy().to_string(),
                detail: error.to_string(),
            }
        }
    };
    if let Err(error) =
        std::fs::write(&to_path, &bytes).and_then(|_| std::fs::remove_file(&from_path))
    {
        return SavedWorkflowMoveResult::WriteError {
            path: to_path.to_string_lossy().to_string(),
            detail: error.to_string(),
        };
    }
    SavedWorkflowMoveResult::Ok {
        from: from_path.to_string_lossy().to_string(),
        to: to_path.to_string_lossy().to_string(),
    }
}

// ——————————————————————— frontmatter codec ———————————————————————

#[derive(Clone, Debug, PartialEq)]
pub struct ParsedSavedWorkflow {
    pub meta: SavedWorkflowMeta,
    pub script: String,
    pub body_line_offset: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SavedWorkflowParseError {
    pub reason: &'static str,
    pub detail: String,
}

/// Metadata + script → file body (key order fixed: description → whenToUse → args; the script follows the terminating line byte for byte).
pub fn serialize_saved_workflow(meta: &SavedWorkflowMeta, script: &str) -> String {
    let yaml = serde_yaml::to_string(meta).unwrap_or_default();
    format!("{SAVED_WORKFLOW_SENTINEL}\n{yaml}*/\n{script}")
}

/// File body → metadata + script. Each of the four failures has its own name.
pub fn parse_saved_workflow(source: &str) -> Result<ParsedSavedWorkflow, SavedWorkflowParseError> {
    let lines: Vec<&str> = source.split('\n').collect();
    let mut start = 0usize;
    while start < lines.len() && lines[start].trim().is_empty() {
        start += 1;
    }
    if start >= lines.len() || lines[start].trim() != SAVED_WORKFLOW_SENTINEL {
        return Err(SavedWorkflowParseError {
            reason: "missing_frontmatter",
            detail: format!(
                "file does not start with the `{SAVED_WORKFLOW_SENTINEL}` metadata block"
            ),
        });
    }
    let mut end = start + 1;
    while end < lines.len() && lines[end].trim() != "*/" {
        end += 1;
    }
    if end >= lines.len() {
        return Err(SavedWorkflowParseError {
            reason: "unterminated_frontmatter",
            detail: "metadata block is never closed with `*/`".to_string(),
        });
    }
    let body_text = lines[start + 1..end].join("\n");
    let script = lines[end + 1..].join("\n");
    let parsed: SavedWorkflowMeta =
        serde_yaml::from_str(&body_text).map_err(|error| SavedWorkflowParseError {
            reason: "invalid_yaml",
            detail: error.to_string(),
        })?;
    // Strict semantics come from deny_unknown_fields plus a non-empty description; the non-empty check is added here.
    if parsed.description.is_empty() {
        return Err(SavedWorkflowParseError {
            reason: "invalid_metadata",
            detail: "description: must be a non-empty string".to_string(),
        });
    }
    if let Some(when_to_use) = &parsed.when_to_use {
        if when_to_use.is_empty() {
            return Err(SavedWorkflowParseError {
                reason: "invalid_metadata",
                detail: "whenToUse: must be a non-empty string".to_string(),
            });
        }
    }
    Ok(ParsedSavedWorkflow {
        meta: parsed,
        script,
        body_line_offset: end + 1,
    })
}

// ——————————————————————— Drafts ———————————————————————

/// Write a draft and return its absolute path; if the write fails, return `None` (never fail the call).
///
/// Every inline submission mints a new file (the one the user is editing is never overwritten behind
/// the model's back); a name clash continues with `-2`, `-3`… and exclusive creation guarantees that
/// concurrent submissions do not land in the same file.
pub fn write_workflow_draft(
    cwd: Option<&Path>,
    name: &str,
    source: &str,
) -> Option<std::path::PathBuf> {
    let cwd = cwd?;
    if cwd.as_os_str().is_empty() {
        return None;
    }
    let dir = cwd.join(WORKFLOW_DRAFTS_DIR);
    std::fs::create_dir_all(&dir).ok()?;
    // A .gitignore inside the directory, written once only when absent (once the user edits it, leave it alone).
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(".gitignore"))
        .and_then(|mut file| std::io::Write::write_all(&mut file, b"*\n"));
    let slug = workflow_draft_slug(name);
    for attempt in 1..=WORKFLOW_DRAFT_MAX_ATTEMPTS {
        let file_name = if attempt == 1 {
            format!("{slug}{SAVED_WORKFLOW_FILE_EXTENSION}")
        } else {
            format!("{slug}-{attempt}{SAVED_WORKFLOW_FILE_EXTENSION}")
        };
        let file_path = dir.join(file_name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&file_path)
        {
            Ok(mut file) => {
                if std::io::Write::write_all(&mut file, source.as_bytes()).is_err() {
                    return None;
                }
                return Some(file_path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// Name → file-name stem: whitespace becomes `-`, anything outside Unicode letters/digits and `_ . -` is
/// dropped, truncation is by code point, and a result that is empty or dots-only gets a fallback word
/// (so path/reserved characters such as `/ \ : * ? " < > |` can never appear).
fn workflow_draft_slug(name: &str) -> String {
    let trimmed = name.trim();
    let mut out = String::new();
    let mut pending_dash = false;
    for c in trimmed.chars() {
        if c.is_whitespace() {
            pending_dash = !out.is_empty();
            continue;
        }
        let allowed = c.is_alphanumeric() || matches!(c, '_' | '.' | '-');
        if !allowed {
            continue;
        }
        if pending_dash && !out.ends_with('-') {
            out.push('-');
        }
        pending_dash = false;
        out.push(c);
    }
    let reduced: String = out.chars().take(WORKFLOW_DRAFT_MAX_SLUG_CHARS).collect();
    let reduced = reduced.trim_matches(|c| c == '.' || c == '-').to_string();
    if reduced.is_empty() || reduced.chars().all(|c| c == '.') {
        return WORKFLOW_DRAFT_FALLBACK_SLUG.to_string();
    }
    reduced
}

// ——————————————————————— Argument validation ———————————————————————

#[derive(Clone, Debug, PartialEq)]
pub enum WorkflowArgsValidation {
    Ok(BTreeMap<String, Value>),
    Errors(Vec<String>),
}

/// Validate against the declarations and fill in defaults; **all** violations are collected and returned at once.
pub fn validate_workflow_args(
    declaration: Option<&SavedWorkflowArgsDeclaration>,
    provided: Option<&serde_json::Map<String, Value>>,
) -> WorkflowArgsValidation {
    let empty = SavedWorkflowArgsDeclaration::new();
    let declared = declaration.unwrap_or(&empty);
    let empty_given = serde_json::Map::new();
    let given = provided.unwrap_or(&empty_given);
    let mut errors = Vec::new();
    let mut args = BTreeMap::new();
    let declared_names: Vec<&String> = declared.keys().collect();

    for key in given.keys() {
        if declared.contains_key(key) {
            continue;
        }
        errors.push(if declared_names.is_empty() {
            format!("unknown argument '{key}': this workflow declares no arguments")
        } else {
            format!(
                "unknown argument '{key}' (declared: {})",
                declared_names
                    .iter()
                    .map(|name| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        });
    }

    for (key, spec) in declared {
        match given.get(key) {
            None => {
                if let Some(default) = &spec.default {
                    if let Some(failure) = type_mismatch(key, spec, default, "default value") {
                        errors.push(failure);
                    } else {
                        args.insert(key.clone(), default.clone());
                    }
                } else if spec.required == Some(true) {
                    errors.push(format!("missing required argument '{key}'"));
                }
            }
            Some(value) => {
                if let Some(failure) = type_mismatch(key, spec, value, "value") {
                    errors.push(failure);
                } else {
                    args.insert(key.clone(), value.clone());
                }
            }
        }
    }

    if errors.is_empty() {
        WorkflowArgsValidation::Ok(args)
    } else {
        WorkflowArgsValidation::Errors(errors)
    }
}

/// The explanation when the type does not match, None when it does. `json` accepts anything (even null); `number` rejects NaN/Infinity.
fn type_mismatch(
    key: &str,
    spec: &SavedWorkflowArgDeclaration,
    value: &Value,
    what: &str,
) -> Option<String> {
    let ok = match spec.arg_type {
        SavedWorkflowArgType::Json => true,
        SavedWorkflowArgType::String => value.is_string(),
        SavedWorkflowArgType::Boolean => value.is_boolean(),
        SavedWorkflowArgType::Number => match value {
            Value::Number(number) => number.as_f64().map(|n| n.is_finite()).unwrap_or(false),
            _ => false,
        },
    };
    if ok {
        return None;
    }
    let expected = match spec.arg_type {
        SavedWorkflowArgType::String => "a string",
        SavedWorkflowArgType::Number => "a finite number",
        SavedWorkflowArgType::Boolean => "a boolean",
        SavedWorkflowArgType::Json => "a JSON value",
    };
    Some(format!(
        "argument '{key}' {what} must be {expected}, got {}",
        json_kind(value)
    ))
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir(tag: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "zcode-saved-wf-{tag}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn meta(description: &str) -> SavedWorkflowMeta {
        SavedWorkflowMeta {
            description: description.to_string(),
            when_to_use: None,
            args: None,
        }
    }

    #[test]
    fn name_validation_blocks_path_traversal() {
        assert!(is_valid_saved_workflow_name("pr-review_v2.main"));
        assert!(!is_valid_saved_workflow_name(""));
        assert!(!is_valid_saved_workflow_name(".."));
        assert!(!is_valid_saved_workflow_name("."));
        assert!(!is_valid_saved_workflow_name("../etc/passwd"));
        assert!(!is_valid_saved_workflow_name("a b"));
        assert!(!is_valid_saved_workflow_name(&"x".repeat(65)));
        assert!(is_valid_saved_workflow_name(&"x".repeat(64)));
    }

    #[test]
    fn frontmatter_round_trips_byte_stable() {
        let mut args = SavedWorkflowArgsDeclaration::new();
        args.insert(
            "pr".to_string(),
            SavedWorkflowArgDeclaration {
                arg_type: SavedWorkflowArgType::Number,
                description: Some("PR number".to_string()),
                required: Some(true),
                default: None,
            },
        );
        let meta = SavedWorkflowMeta {
            description: "Review a PR".to_string(),
            when_to_use: Some("when a PR needs review".to_string()),
            args: Some(args),
        };
        let script = "return { ok: true };\n";
        let serialized = serialize_saved_workflow(&meta, script);
        let parsed = parse_saved_workflow(&serialized).expect("parses");
        assert_eq!(parsed.meta, meta);
        assert_eq!(parsed.script, script, "script bytes preserved");
        assert_eq!(
            parsed.body_line_offset,
            serialized.lines().position(|l| l == "*/").unwrap() + 1
        );
        // A second serialization is byte-identical (the key order is fixed).
        assert_eq!(
            serialize_saved_workflow(&parsed.meta, &parsed.script),
            serialized
        );
    }

    #[test]
    fn parse_failures_are_named() {
        assert_eq!(
            parse_saved_workflow("const x = 1;").unwrap_err().reason,
            "missing_frontmatter"
        );
        assert_eq!(
            parse_saved_workflow("/* nc-code-workflow\ndescription: x\n")
                .unwrap_err()
                .reason,
            "unterminated_frontmatter"
        );
        assert_eq!(
            parse_saved_workflow("/* nc-code-workflow\ndescription: [unclosed\n*/\nbody")
                .unwrap_err()
                .reason,
            "invalid_yaml"
        );
        let unknown = "/* nc-code-workflow\ndescription: x\nbogus: 1\n*/\nbody";
        assert_eq!(
            parse_saved_workflow(unknown).unwrap_err().reason,
            "invalid_yaml"
        );
        let empty_desc = "/* nc-code-workflow\ndescription: ''\n*/\nbody";
        assert_eq!(
            parse_saved_workflow(empty_desc).unwrap_err().reason,
            "invalid_metadata"
        );
    }

    #[test]
    fn resolve_is_project_first_and_reports_parse_errors() {
        let cwd = temp_dir("resolve");
        let home = temp_dir("resolve-home");
        save_saved_workflow(
            &cwd,
            "alpha",
            &meta("project copy"),
            "return 1;",
            None,
            Some(&home),
        )
        .unwrap();
        save_saved_workflow(
            &cwd,
            "alpha",
            &meta("global copy"),
            "return 2;",
            Some(SavedWorkflowScope::Global),
            Some(&home),
        )
        .unwrap();
        let resolved = match resolve_saved_workflow(&cwd, "alpha", None, Some(&home)) {
            SavedWorkflowResolveResult::Ok(resolved) => resolved,
            _ => panic!("must resolve"),
        };
        assert_eq!(resolved.scope, SavedWorkflowScope::Project);
        assert_eq!(resolved.meta.description, "project copy");
        // A targeted scope looks only at that one root.
        let resolved = match resolve_saved_workflow(
            &cwd,
            "alpha",
            Some(SavedWorkflowScope::Global),
            Some(&home),
        ) {
            SavedWorkflowResolveResult::Ok(resolved) => resolved,
            _ => panic!("must resolve global"),
        };
        assert_eq!(resolved.meta.description, "global copy");
        // A broken file: parse_error names the path; it never degrades into not_found.
        std::fs::write(cwd.join(".zcode/workflows/broken.dwf.ts"), "not a workflow").unwrap();
        match resolve_saved_workflow(&cwd, "broken", None, Some(&home)) {
            SavedWorkflowResolveResult::Err(SavedWorkflowResolveFailure::ParseError {
                reason,
                ..
            }) => {
                assert_eq!(reason, "missing_frontmatter")
            }
            _ => panic!("must be parse_error"),
        }
        assert!(matches!(
            resolve_saved_workflow(&cwd, "missing", None, Some(&home)),
            SavedWorkflowResolveResult::Err(SavedWorkflowResolveFailure::NotFound)
        ));
        assert!(matches!(
            resolve_saved_workflow(&cwd, "../x", None, Some(&home)),
            SavedWorkflowResolveResult::Err(SavedWorkflowResolveFailure::InvalidName(_))
        ));
    }

    #[test]
    fn list_shadows_and_reports_invalid_without_failing() {
        let cwd = temp_dir("list");
        let home = temp_dir("list-home");
        save_saved_workflow(
            &cwd,
            "alpha",
            &meta("project"),
            "return 1;",
            None,
            Some(&home),
        )
        .unwrap();
        save_saved_workflow(
            &cwd,
            "alpha",
            &meta("global"),
            "return 2;",
            Some(SavedWorkflowScope::Global),
            Some(&home),
        )
        .unwrap();
        save_saved_workflow(
            &cwd,
            "beta",
            &meta("global only"),
            "return 3;",
            Some(SavedWorkflowScope::Global),
            Some(&home),
        )
        .unwrap();
        std::fs::write(cwd.join(".zcode/workflows/broken.dwf.ts"), "bad").unwrap();
        std::fs::write(cwd.join(".zcode/workflows/not a name.dwf.ts"), "x").unwrap();

        let list = list_saved_workflows(&cwd, None, Some(&home));
        assert_eq!(list.entries.len(), 2, "shadowed global alpha is not listed");
        assert_eq!(list.entries[0].name, "alpha");
        assert_eq!(list.entries[0].scope, SavedWorkflowScope::Project);
        assert_eq!(list.entries[1].name, "beta");
        assert_eq!(list.entries[1].scope, SavedWorkflowScope::Global);
        assert_eq!(list.invalid.len(), 2, "bad files are named, not fatal");

        let global_only = list_saved_workflows(&cwd, Some(SavedWorkflowScope::Global), Some(&home));
        assert_eq!(
            global_only.entries.len(),
            2,
            "scoped listing does not shadow"
        );
    }

    #[test]
    fn save_reports_overwrite_and_shadowing() {
        let cwd = temp_dir("save");
        let home = temp_dir("save-home");
        let (_, scope, overwritten) =
            save_saved_workflow(&cwd, "gamma", &meta("one"), "return 1;", None, Some(&home))
                .unwrap();
        assert_eq!(scope, SavedWorkflowScope::Project);
        assert!(!overwritten);
        let (_, _, overwritten) =
            save_saved_workflow(&cwd, "gamma", &meta("two"), "return 2;", None, Some(&home))
                .unwrap();
        assert!(overwritten);
        // Saving a global definition while the project one exists means it is shadowed by the project file (hidden_by_project).
        assert_eq!(
            find_saved_workflow_shadowing(&cwd, "gamma", SavedWorkflowScope::Global, Some(&home)),
            Some(SavedWorkflowShadowing::HiddenByProject)
        );
        save_saved_workflow(
            &cwd,
            "gamma",
            &meta("g"),
            "return 3;",
            Some(SavedWorkflowScope::Global),
            Some(&home),
        )
        .unwrap();
        assert_eq!(
            find_saved_workflow_shadowing(&cwd, "gamma", SavedWorkflowScope::Project, Some(&home)),
            Some(SavedWorkflowShadowing::HidesGlobal)
        );
        assert_eq!(
            find_saved_workflow_shadowing(&cwd, "gamma", SavedWorkflowScope::Global, Some(&home)),
            Some(SavedWorkflowShadowing::HiddenByProject)
        );
    }

    #[test]
    fn move_global_to_project_is_byte_identical_and_refuses_overwrite() {
        let cwd = temp_dir("move");
        let home = temp_dir("move-home");
        save_saved_workflow(
            &cwd,
            "delta",
            &meta("global"),
            "return 1;",
            Some(SavedWorkflowScope::Global),
            Some(&home),
        )
        .unwrap();
        let from = home.join(".zcode/workflows/delta.dwf.ts");
        let bytes_before = std::fs::read(&from).unwrap();
        let moved = move_saved_workflow(&cwd, "delta", Some(&home));
        assert!(matches!(moved, SavedWorkflowMoveResult::Ok { .. }));
        let to = cwd.join(".zcode/workflows/delta.dwf.ts");
        assert_eq!(std::fs::read(&to).unwrap(), bytes_before);
        assert!(!from.exists());
        // An existing target is refused.
        save_saved_workflow(
            &cwd,
            "delta",
            &meta("again"),
            "return 9;",
            Some(SavedWorkflowScope::Global),
            Some(&home),
        )
        .unwrap();
        assert!(matches!(
            move_saved_workflow(&cwd, "delta", Some(&home)),
            SavedWorkflowMoveResult::TargetExists(_)
        ));
        assert!(matches!(
            move_saved_workflow(&cwd, "absent", Some(&home)),
            SavedWorkflowMoveResult::NotFound
        ));
    }

    #[test]
    fn drafts_slug_unicode_and_collisions() {
        let cwd = temp_dir("drafts");
        let first = write_workflow_draft(Some(&cwd), "PR review #12", "return 1;").expect("draft");
        assert!(first
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("PR-review-12"));
        let second = write_workflow_draft(Some(&cwd), "PR review #12", "return 2;").expect("draft");
        assert_ne!(first, second, "each inline submit mints a new file");
        assert!(second.file_name().unwrap().to_string_lossy().contains("-2"));
        let chinese =
            write_workflow_draft(Some(&cwd), "Code review process", "return 3;").expect("draft");
        assert!(chinese
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("Code-review-process"));
        let nothing = write_workflow_draft(Some(&cwd), "///", "return 4;").expect("draft");
        assert!(nothing
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("workflow"));
        // The .gitignore is written once, only when absent.
        let gitignore = cwd.join(".zcode/workflow-drafts/.gitignore");
        assert_eq!(std::fs::read_to_string(&gitignore).unwrap(), "*\n");
        std::fs::write(&gitignore, "*\n!keep\n").unwrap();
        let _ = write_workflow_draft(Some(&cwd), "again", "return 5;");
        assert_eq!(std::fs::read_to_string(&gitignore).unwrap(), "*\n!keep\n");
        assert!(write_workflow_draft(None, "x", "return 6;").is_none());
    }

    #[test]
    fn args_validation_collects_all_errors_and_applies_defaults() {
        let mut declared = SavedWorkflowArgsDeclaration::new();
        declared.insert(
            "pr".to_string(),
            SavedWorkflowArgDeclaration {
                arg_type: SavedWorkflowArgType::Number,
                description: None,
                required: Some(true),
                default: None,
            },
        );
        declared.insert(
            "mode".to_string(),
            SavedWorkflowArgDeclaration {
                arg_type: SavedWorkflowArgType::String,
                description: None,
                required: None,
                default: Some(json!("fast")),
            },
        );
        declared.insert(
            "payload".to_string(),
            SavedWorkflowArgDeclaration {
                arg_type: SavedWorkflowArgType::Json,
                description: None,
                required: None,
                default: None,
            },
        );

        let mut given = serde_json::Map::new();
        given.insert("mode".to_string(), json!("slow"));
        let WorkflowArgsValidation::Errors(errors) =
            validate_workflow_args(Some(&declared), Some(&given))
        else {
            panic!("missing pr must fail");
        };
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("missing required argument 'pr'"));

        given.insert("pr".to_string(), json!(42));
        given.insert("extra".to_string(), json!(1));
        let WorkflowArgsValidation::Errors(errors) =
            validate_workflow_args(Some(&declared), Some(&given))
        else {
            panic!("unknown extra must fail");
        };
        assert!(errors[0].contains("unknown argument 'extra'"));

        given.remove("extra");
        given.insert("payload".to_string(), Value::Null);
        let WorkflowArgsValidation::Ok(args) =
            validate_workflow_args(Some(&declared), Some(&given))
        else {
            panic!("valid args");
        };
        assert_eq!(args.get("pr"), Some(&json!(42)));
        assert_eq!(args.get("mode"), Some(&json!("slow")));
        assert_eq!(args.get("payload"), Some(&Value::Null), "json accepts null");

        // A default value goes through the same type check.
        let mut bad_default = SavedWorkflowArgsDeclaration::new();
        bad_default.insert(
            "n".to_string(),
            SavedWorkflowArgDeclaration {
                arg_type: SavedWorkflowArgType::Number,
                description: None,
                required: None,
                default: Some(json!("3")),
            },
        );
        let WorkflowArgsValidation::Errors(errors) =
            validate_workflow_args(Some(&bad_default), Some(&serde_json::Map::new()))
        else {
            panic!("bad default must fail");
        };
        assert!(errors[0].contains("default value must be a finite number"));

        // A workflow that declares no args is wrong on any argument it receives.
        let mut undeclared_given = serde_json::Map::new();
        undeclared_given.insert("x".to_string(), json!(1));
        let WorkflowArgsValidation::Errors(errors) =
            validate_workflow_args(None, Some(&undeclared_given))
        else {
            panic!("undeclared args must fail");
        };
        assert!(errors[0].contains("declares no arguments"));
    }
}
