//! Saved dynamic-workflow files (`.dwf.ts`): frontmatter codec + project/global file storage.
//!
//! Transplanted item by item from:
//! - `apps/nc-code-cli/packages/core/src/tool/handlers/saved-workflows/frontmatter.ts`
//! - `apps/nc-code-cli/packages/core/src/tool/handlers/saved-workflows/store.ts`
//! - `apps/nc-code-cli/packages/contracts/src/tools/saved-workflow.ts` (constants and schema vocabulary)
//! - `apps/nc-code-cli/packages/bootstrap/src/nc-code-protocol/saved-workflows.ts`
//!   (Delete / only change the shape of the results of the two pure file operations of metadata)
//!
//! The result shape is aligned to `packages/shared/src/nc-code-protocol/index.ts`
//! `zcodeWorkflows*ResultSchema` (`workflows` / `invalid` / `dir` / `bodyLineOffset` etc.
//! camelCase field names are word-for-word consistent).
//!
//! File shape (the metadata block is a **block comment**, so the entire file is still valid TypeScript, and editor highlighting/formatting is as usual):
//!
//! ```text
//! /* nc-code-workflow
//! description: Review a pull request
//! args:
//! pr:
//! type: number
//! required: true
//! */
//! <plain dwf script, byte-for-byte>
//! ```
//!
//! The storage layer is deliberately **synchronous** (for the same reason as the original TS): the `prepareApproval` contract of the confirmation window is synchronous,
//! Runs initiated from the `saved` source must read the script **before** the pop-up window; these files are local, single, and measured in KB.
//!
//! ## Known differences from TS implementation (observable semantics consistent)
//!
//! - YAML text formatting is determined by `serde_yaml` rather than npm `yaml`. Both follow YAML 1.2 core schema
//!   Parsed, semantics are the same; user-written blocks may have newline/quote style change when rewritten. The script after the terminating line is always
//!   Preserved byte by byte - that's the baseline against which run's script hash is compared to the editor's content.
//! - The I/O error text is the OS error string; Node will be additionally prefixed with syscall. Failure `reason` vocabulary
//!   (`not_found` / `read_error` / `write_error` ...) Exactly the same.
//! - `rename` is only atomic within the same volume. Fallback across volumes read → write → remove (same as TS); two calls
//!   A crash between will leave a copy on the target, the source is still there, and the TS has the same window.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value as JsonValue;

/// The extension of the saved file. `.dwf.ts` allows scanning to distinguish the file from the project source code without opening it, while the editor presses
/// TypeScript highlighting (frontmatter is a block comment, the syntax is legal).
pub const SAVED_WORKFLOW_FILE_EXTENSION: &str = ".dwf.ts";

/// The storage directory of the project scope (relative to the session working directory).
pub const SAVED_WORKFLOW_PROJECT_DIR: &str = ".zcode/workflows";

/// The storage directory of the global scope (relative to the home directory of the agent process).
pub const SAVED_WORKFLOW_GLOBAL_DIR: &str = ".zcode/workflows";

/// The opening tag of frontmatter: must be the first non-blank content of the file.
pub const SAVED_WORKFLOW_SENTINEL: &str = "/* nc-code-workflow";

/// The upper limit of name length (the file names of each platform are far within PATH_MAX).
pub const SAVED_WORKFLOW_MAX_NAME_CHARS: usize = 64;

/// The closing tag of frontmatter: a line with only the end of the block comment.
const SAVED_WORKFLOW_TERMINATOR: &str = "*/";

/// The reason for invalid when the file name is not a valid workflow name (same as TS verbatim).
const UNUSABLE_FILE_NAME_REASON: &str = "file name is not a usable workflow name";

const META_KEYS: [&str; 3] = ["description", "whenToUse", "args"];
const ARG_DECLARATION_KEYS: [&str; 4] = ["type", "description", "required", "default"];
const ARG_TYPE_LIST_LABEL: &str = "'string' | 'number' | 'boolean' | 'json'";

/// Scope. The scope of the file is inferred from the directory where it is located, and frontmatter does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SavedWorkflowScope {
    Project,
    Global,
}

impl SavedWorkflowScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Global => "global",
        }
    }
}

/// A search root: scope label + absolute directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedWorkflowRoot {
    pub scope: SavedWorkflowScope,
    pub dir: PathBuf,
}

/// The search root for this session, in order of priority: `[project, global]`.
///
/// All searches are in order first-wins: the one in the project always wins the global one (masking with the same name).
/// `home_dir` is injected by the host (the TS side always takes `os.homedir()` and does not follow any `storage.dir` configuration).
pub fn saved_workflow_roots(cwd: &Path, home_dir: &Path) -> [SavedWorkflowRoot; 2] {
    [
        SavedWorkflowRoot {
            scope: SavedWorkflowScope::Project,
            dir: saved_workflow_dir(cwd, SAVED_WORKFLOW_PROJECT_DIR),
        },
        SavedWorkflowRoot {
            scope: SavedWorkflowScope::Global,
            dir: saved_workflow_dir(home_dir, SAVED_WORKFLOW_GLOBAL_DIR),
        },
    ]
}

/// The search root for a single scope. The scope is a known enumeration, here the value is obtained according to the construction, there is no "not found".
pub fn saved_workflow_root(
    cwd: &Path,
    scope: SavedWorkflowScope,
    home_dir: &Path,
) -> SavedWorkflowRoot {
    let (base, relative) = match scope {
        SavedWorkflowScope::Project => (cwd, SAVED_WORKFLOW_PROJECT_DIR),
        SavedWorkflowScope::Global => (home_dir, SAVED_WORKFLOW_GLOBAL_DIR),
    };
    SavedWorkflowRoot {
        scope,
        dir: saved_workflow_dir(base, relative),
    }
}

/// Relative directories in constants are segmented by `/` and spliced with `Path::push`: on Windows, the native delimiter is obtained.
/// Instead of treating the entire `.zcode/workflows` section as a file name with a slash.
fn saved_workflow_dir(base: &Path, relative: &str) -> PathBuf {
    let mut dir = base.to_path_buf();
    for segment in relative.split('/') {
        dir.push(segment);
    }
    dir
}

/// Whether the name can be used as a file name (i.e. whether it may point to a saved workflow).
///
/// Length 1..=64, character set `[A-Za-z0-9_.-]`, and cannot consist entirely of dots (`.` / `..` are directory entries).
/// This check **must** occur before spelling the path: `../../.ssh/id_rsa` After spelling the path, it will be a readable absolute
/// Path, the character set itself is the line of defense for path crossing.
pub fn is_valid_saved_workflow_name(name: &str) -> bool {
    let char_count = name.chars().count();
    if char_count == 0 || char_count > SAVED_WORKFLOW_MAX_NAME_CHARS {
        return false;
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
    {
        return false;
    }
    name.chars().any(|ch| ch != '.')
}

/// name → filename. The name has passed [`is_valid_saved_workflow_name`] before calling.
pub fn saved_workflow_file_name(name: &str) -> String {
    format!("{name}.dwf.ts")
}

/// The position of the name under the given root. The write side and the read side share it - the two separate paths are exactly the "save successfully but cannot be read out"
/// Classic origin.
pub fn saved_workflow_path(root: &SavedWorkflowRoot, name: &str) -> PathBuf {
    root.dir.join(saved_workflow_file_name(name))
}

/// Type vocabulary for parameters. Three primitives plus one `json` for the bottom line: the primitives can be verified as "wrong transmission", `json` clearly indicates
/// "We accept everything here".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SavedWorkflowArgType {
    String,
    Number,
    Boolean,
    Json,
}

impl SavedWorkflowArgType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Json => "json",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "string" => Some(Self::String),
            "number" => Some(Self::Number),
            "boolean" => Some(Self::Boolean),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

/// A parameter declaration.
///
/// `default` uses `Option<JsonValue>` instead of "default value + sentinel": `Some(Value::Null)` represents the file
/// If `default: null` is explicitly written, `None` means not written - both must be able to be reserialized into the same file.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedWorkflowArgDeclaration {
    #[serde(rename = "type")]
    pub kind: SavedWorkflowArgType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<JsonValue>,
}

/// parameter name → declaration. `IndexMap` iterates in insertion order (TS's `z.record` also preserves object key order), so YAML
/// order written in is retained unchanged in serialization and list JSON. `serde_json::Map` is only available in this workspace version
/// `Value` provides an ordered implementation and cannot accommodate declared types.
pub type SavedWorkflowArgsDeclaration = IndexMap<String, SavedWorkflowArgDeclaration>;
/// Metadata body in frontmatter. Strict schema: misspelling a key is a visible invalid line, rather than being silent
/// Discarded fields - saved files are manually modified by the user.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedWorkflowMeta {
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<SavedWorkflowArgsDeclaration>,
}

/// A row in the list: metadata + drop point, **excluding script text** (enumeration is not reading).
#[derive(Debug, Clone, PartialEq, Serialize)]
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

/// A file that exists but cannot be read. The list does not fail because of a bad file, but it must be named.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SavedWorkflowInvalidEntry {
    pub path: String,
    pub reason: String,
}

/// Results of `workflows/list`: scanned entries, bad entries, and **scanned directories** (returned even if they do not exist,
/// GUI's file monitoring relies on it watch).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SavedWorkflowsListResult {
    pub workflows: Vec<SavedWorkflowEntry>,
    pub invalid: Vec<SavedWorkflowInvalidEntry>,
    pub dir: String,
}

/// Undirected enumeration (store's `listSavedWorkflows({ cwd })` form): two first-wins in order and with the same name.
/// The difference from [`SavedWorkflowsListResult`] is that there is no single `dir` - two roots are scanned.
#[derive(Debug, Clone, PartialEq)]
pub struct SavedWorkflowListing {
    pub entries: Vec<SavedWorkflowEntry>,
    pub invalid: Vec<SavedWorkflowInvalidEntry>,
}

/// Four reasons for parsing failure. They give users different advice: no frontmatter is "this is not a saved
/// workflow", not closed means "you deleted a line", bad YAML means "the indentation is wrong", and schema means "the field name is written wrong".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavedWorkflowParseErrorReason {
    MissingFrontmatter,
    UnterminatedFrontmatter,
    InvalidYaml,
    InvalidMetadata,
}

impl SavedWorkflowParseErrorReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingFrontmatter => "missing_frontmatter",
            Self::UnterminatedFrontmatter => "unterminated_frontmatter",
            Self::InvalidYaml => "invalid_yaml",
            Self::InvalidMetadata => "invalid_metadata",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SavedWorkflowParseError {
    pub reason: SavedWorkflowParseErrorReason,
    pub detail: String,
}

impl std::fmt::Display for SavedWorkflowParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.reason.as_str(), self.detail)
    }
}

/// A saved definition that parses successfully.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedSavedWorkflow {
    pub meta: SavedWorkflowMeta,
    /// Everything after the terminating line, byte by byte.
    pub script: String,
    /// The number of lines before the body; add it when converting diagnostics to file lines: `fileLine = bodyLine + bodyLineOffset`.
    pub body_line_offset: usize,
}

/// A successfully parsed saved definition + drop point.
///
/// `source` / `bodyLineOffset` is a store layer field (used for draft copy and diagnosis line number); protocol
/// `workflows/get` succeeds. The shape is only `ok/name/path/scope/meta/script`, and the channel layer is
/// `zcodeWorkflowsGetResultSchema` can be assembled.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedSavedWorkflow {
    pub name: String,
    pub path: String,
    pub scope: SavedWorkflowScope,
    pub meta: SavedWorkflowMeta,
    pub script: String,
    /// Original file text (metadata block + text), byte by byte. The draft copy takes just that: reserialization will make `args`
    /// Default values, comments, and handwritten YAML formatting drift in copies.
    pub source: String,
    pub body_line_offset: usize,
}

/// The failed branch of resolve. `path` is only present on parse/read errors (the protocol layer collapses it into `{reason, detail}`/// Of `zcodeSavedWorkflowFailureSchema`, the store layer reserves the path for diagnosis to point to the file).
#[derive(Debug, Clone, PartialEq)]
pub enum SavedWorkflowResolveFailure {
    InvalidName { detail: String },
    NotFound,
    ParseError { path: String, detail: String },
    ReadError { path: String, detail: String },
}

impl SavedWorkflowResolveFailure {
    pub fn reason(&self) -> &'static str {
        match self {
            Self::InvalidName { .. } => "invalid_name",
            Self::NotFound => "not_found",
            Self::ParseError { .. } => "parse_error",
            Self::ReadError { .. } => "read_error",
        }
    }

    pub fn path(&self) -> Option<&str> {
        match self {
            Self::ParseError { path, .. } | Self::ReadError { path, .. } => Some(path),
            Self::InvalidName { .. } | Self::NotFound => None,
        }
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::InvalidName { detail }
            | Self::ParseError { detail, .. }
            | Self::ReadError { detail, .. } => Some(detail),
            Self::NotFound => None,
        }
    }
}

/// The result of resolve: success with complete definition, failure with one of the four states.
#[derive(Debug, Clone, PartialEq)]
pub enum SavedWorkflowResolveResult {
    Ok(ResolvedSavedWorkflow),
    Failed(SavedWorkflowResolveFailure),
}

/// The result of moving the global file back to the project file (`ZCodeWorkflowsMoveResult` aligned verbatim).
#[derive(Debug, Clone, PartialEq)]
pub enum SavedWorkflowMoveResult {
    Moved { from: String, to: String },
    InvalidName { detail: String },
    NotFound,
    TargetExists { path: String },
    ReadError { path: String, detail: String },
    WriteError { path: String, detail: String },
}

impl SavedWorkflowMoveResult {
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Self::Moved { .. } => None,
            Self::InvalidName { .. } => Some("invalid_name"),
            Self::NotFound => Some("not_found"),
            Self::TargetExists { .. } => Some("target_exists"),
            Self::ReadError { .. } => Some("read_error"),
            Self::WriteError { .. } => Some("write_error"),
        }
    }

    pub fn path(&self) -> Option<&str> {
        match self {
            Self::TargetExists { path }
            | Self::ReadError { path, .. }
            | Self::WriteError { path, .. } => Some(path),
            Self::Moved { .. } | Self::InvalidName { .. } | Self::NotFound => None,
        }
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::InvalidName { detail }
            | Self::ReadError { detail, .. }
            | Self::WriteError { detail, .. } => Some(detail),
            Self::Moved { .. } | Self::NotFound | Self::TargetExists { .. } => None,
        }
    }
}

impl Serialize for SavedWorkflowResolveResult {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Ok(resolved) => {
                let mut map = serializer.serialize_map(Some(8))?;
                map.serialize_entry("ok", &true)?;
                map.serialize_entry("name", &resolved.name)?;
                map.serialize_entry("path", &resolved.path)?;
                map.serialize_entry("scope", &resolved.scope)?;
                map.serialize_entry("meta", &resolved.meta)?;
                map.serialize_entry("script", &resolved.script)?;
                map.serialize_entry("source", &resolved.source)?;
                map.serialize_entry("bodyLineOffset", &resolved.body_line_offset)?;
                map.end()
            }
            Self::Failed(failure) => {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("ok", &false)?;
                map.serialize_entry("reason", failure.reason())?;
                if let Some(path) = failure.path() {
                    map.serialize_entry("path", path)?;
                }
                if let Some(detail) = failure.detail() {
                    map.serialize_entry("detail", detail)?;
                }
                map.end()
            }
        }
    }
}

impl Serialize for SavedWorkflowMoveResult {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Moved { from, to } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("ok", &true)?;
                map.serialize_entry("from", from)?;
                map.serialize_entry("to", to)?;
                map.end()
            }
            failure => {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("ok", &false)?;
                if let Some(reason) = failure.reason() {
                    map.serialize_entry("reason", reason)?;
                }
                if let Some(path) = failure.path() {
                    map.serialize_entry("path", path)?;
                }
                if let Some(detail) = failure.detail() {
                    map.serialize_entry("detail", detail)?;
                }
                map.end()
            }
        }
    }
}

/// Delete a saved definition by name (`zcodeWorkflowsDeleteResultSchema` verbatim alignment: fails **without**
/// `path`, `invalid_name` also **without** `detail`).
#[derive(Debug, Clone, PartialEq)]
pub enum SavedWorkflowDeleteResult {
    Deleted { path: String },
    InvalidName,
    NotFound,
    ReadError { detail: String },
}

impl SavedWorkflowDeleteResult {
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Self::Deleted { .. } => None,
            Self::InvalidName => Some("invalid_name"),
            Self::NotFound => Some("not_found"),
            Self::ReadError { .. } => Some("read_error"),
        }
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::ReadError { detail } => Some(detail),
            Self::Deleted { .. } | Self::InvalidName | Self::NotFound => None,
        }
    }
}

impl Serialize for SavedWorkflowDeleteResult {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Deleted { path } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("ok", &true)?;
                map.serialize_entry("path", path)?;
                map.end()
            }
            failure => {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("ok", &false)?;
                if let Some(reason) = failure.reason() {
                    map.serialize_entry("reason", reason)?;
                }
                if let Some(detail) = failure.detail() {
                    map.serialize_entry("detail", detail)?;
                }
                map.end()
            }
        }
    }
}

/// Only overwrite metadata results (`zcodeWorkflowsUpdateMetaResultSchema` verbatim alignment).
///
/// Failed branch reuse [`SavedWorkflowResolveFailure`], **discarded** according to the rules of the protocol `toFailure` during serialization
/// `path` (shared failure schema only has `reason` + `detail`).
#[derive(Debug, Clone, PartialEq)]
pub enum SavedWorkflowUpdateMetaResult {
    Updated { path: String },
    Failed(SavedWorkflowResolveFailure),
}
impl Serialize for SavedWorkflowUpdateMetaResult {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Updated { path } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("ok", &true)?;
                map.serialize_entry("path", path)?;
                map.end()
            }
            Self::Failed(failure) => {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("ok", &false)?;
                map.serialize_entry("reason", failure.reason())?;
                if let Some(detail) = failure.detail() {
                    map.serialize_entry("detail", detail)?;
                }
                map.end()
            }
        }
    }
}

/// Metadata + script → file body.
///
/// The key order is fixed (description → whenToUse → args) instead of following the insertion order of object literals: save twice to get
/// The same file byte by byte, otherwise a meaningless diff will be created in git every time it is saved. The script is placed byte by byte after the terminating line.
pub fn serialize_saved_workflow(
    meta: &SavedWorkflowMeta,
    script: &str,
) -> Result<String, serde_yaml::Error> {
    let mut yaml = serde_yaml::to_string(meta)?;
    // TS side relies on `stringifyYaml` with its own trailing newline, so that the terminating line can occupy one line. `serde_yaml` also has a tail
    // Line break; the completion here is just to hard-code this format invariant in this module.
    if !yaml.ends_with('\n') {
        yaml.push('\n');
    }
    Ok(format!(
        "{SAVED_WORKFLOW_SENTINEL}\n{yaml}{SAVED_WORKFLOW_TERMINATOR}\n{script}"
    ))
}

/// File body → metadata + script.
pub fn parse_saved_workflow(source: &str) -> Result<ParsedSavedWorkflow, SavedWorkflowParseError> {
    let lines: Vec<&str> = source.split('\n').collect();

    let mut start = 0usize;
    while start < lines.len() && js_trim(lines[start]).is_empty() {
        start += 1;
    }
    if start >= lines.len() || js_trim(lines[start]) != SAVED_WORKFLOW_SENTINEL {
        return Err(SavedWorkflowParseError {
            reason: SavedWorkflowParseErrorReason::MissingFrontmatter,
            detail: format!(
                "file does not start with the `{SAVED_WORKFLOW_SENTINEL}` metadata block"
            ),
        });
    }

    let mut end = start + 1;
    while end < lines.len() && js_trim(lines[end]) != SAVED_WORKFLOW_TERMINATOR {
        end += 1;
    }
    if end >= lines.len() {
        return Err(SavedWorkflowParseError {
            reason: SavedWorkflowParseErrorReason::UnterminatedFrontmatter,
            detail: format!("metadata block is never closed with `{SAVED_WORKFLOW_TERMINATOR}`"),
        });
    }

    let body_text = lines[start + 1..end].join("\n");
    // Everything after the terminating line is script and left as is (the same holds true for files without a newline at the end: slice gives an empty slice).
    let script = lines[end + 1..].join("\n");
    let meta = parse_saved_workflow_meta(&body_text)?;

    // The text starts on the line next to the terminating line, so there are exactly `end + 1` lines before it.
    Ok(ParsedSavedWorkflow {
        meta,
        script,
        body_line_offset: end + 1,
    })
}

/// A piece of metadata text (YAML; JSON is a subset of YAML) → strictly validated [`SavedWorkflowMeta`].
///
/// The protocol layer uses it to verify the `meta` input parameter of `updateSavedWorkflowMeta`: first explode it at the protocol boundary
/// `invalid_metadata`, instead of writing a file that cannot be read back.
pub fn parse_saved_workflow_meta(
    source: &str,
) -> Result<SavedWorkflowMeta, SavedWorkflowParseError> {
    // The empty YAML document is null; `parseYaml("")` on the TS side also gets null, and then the schema reports
    // "Expected object, received null". Writing this out explicitly does not rely on the library's handling of empty input.
    let value: serde_yaml::Value = if source.trim().is_empty() {
        serde_yaml::Value::Null
    } else {
        serde_yaml::from_str(source).map_err(|error| SavedWorkflowParseError {
            reason: SavedWorkflowParseErrorReason::InvalidYaml,
            detail: error.to_string(),
        })?
    };
    validate_saved_workflow_meta(&value).map_err(|detail| SavedWorkflowParseError {
        reason: SavedWorkflowParseErrorReason::InvalidMetadata,
        detail,
    })
}

/// Strict metadata schema. The issue copy is aligned with Zod (`path: message`, semicolon connection), four types of failures should be reported to the user
/// Different suggestions, so the path must point to a specific field.
fn validate_saved_workflow_meta(value: &serde_yaml::Value) -> Result<SavedWorkflowMeta, String> {
    let serde_yaml::Value::Mapping(map) = value else {
        return Err(format!(
            "(root): Expected object, received {}",
            yaml_type_name(value)
        ));
    };

    let mut issues: Vec<String> = Vec::new();

    let description = match map.get("description") {
        None => {
            issues.push("description: Required".to_string());
            None
        }
        Some(serde_yaml::Value::String(text)) if !text.is_empty() => Some(text.clone()),
        Some(serde_yaml::Value::String(_)) => {
            issues.push("description: String must contain at least 1 character(s)".to_string());
            None
        }
        Some(other) => {
            issues.push(format!(
                "description: Expected string, received {}",
                yaml_type_name(other)
            ));
            None
        }
    };

    let when_to_use = match map.get("whenToUse") {
        None => None,
        Some(serde_yaml::Value::String(text)) if !text.is_empty() => Some(text.clone()),
        Some(serde_yaml::Value::String(_)) => {
            issues.push("whenToUse: String must contain at least 1 character(s)".to_string());
            None
        }
        Some(other) => {
            issues.push(format!(
                "whenToUse: Expected string, received {}",
                yaml_type_name(other)
            ));
            None
        }
    };
    let args = match map.get("args") {
        None => None,
        Some(serde_yaml::Value::Mapping(arguments)) => {
            validate_args_declaration(arguments, &mut issues)
        }
        Some(other) => {
            issues.push(format!(
                "args: Expected object, received {}",
                yaml_type_name(other)
            ));
            None
        }
    };

    let unknown: Vec<String> = map
        .keys()
        .map(key_label)
        .filter(|key| !META_KEYS.contains(&key.as_str()))
        .map(|key| format!("'{key}'"))
        .collect();
    if !unknown.is_empty() {
        issues.push(format!(
            "(root): Unrecognized key(s) in object: {}",
            unknown.join(", ")
        ));
    }

    // When description is absent/wrong type, an issue must have been recorded above, so the Err text here is not empty.
    let Some(description) = description else {
        return Err(issues.join("; "));
    };
    if !issues.is_empty() {
        return Err(issues.join("; "));
    }
    Ok(SavedWorkflowMeta {
        description,
        when_to_use,
        args,
    })
}

fn validate_args_declaration(
    arguments: &serde_yaml::Mapping,
    issues: &mut Vec<String>,
) -> Option<SavedWorkflowArgsDeclaration> {
    let mut parsed = SavedWorkflowArgsDeclaration::new();
    let mut failed = false;
    for (key, value) in arguments {
        if !matches!(key, serde_yaml::Value::String(_)) {
            issues.push(format!(
                "args.{}: record keys must be strings, received {}",
                key_label(key),
                yaml_type_name(key)
            ));
            failed = true;
            continue;
        }
        let name = key_label(key);
        match validate_arg_declaration(value, &name, issues) {
            Some(declaration) => {
                parsed.insert(name, declaration);
            }
            None => failed = true,
        }
    }
    if failed {
        None
    } else {
        Some(parsed)
    }
}

fn validate_arg_declaration(
    value: &serde_yaml::Value,
    name: &str,
    issues: &mut Vec<String>,
) -> Option<SavedWorkflowArgDeclaration> {
    let serde_yaml::Value::Mapping(map) = value else {
        issues.push(format!(
            "args.{name}: Expected object, received {}",
            yaml_type_name(value)
        ));
        return None;
    };
    let path = format!("args.{name}");
    let mut ok = true;

    let kind = match map.get("type") {
        None => {
            issues.push(format!("{path}.type: Required"));
            ok = false;
            None
        }
        Some(serde_yaml::Value::String(text)) => match SavedWorkflowArgType::parse(text) {
            Some(kind) => Some(kind),
            None => {
                issues.push(format!(
                    "{path}.type: Invalid enum value. Expected {ARG_TYPE_LIST_LABEL}, received '{text}'"
                ));
                ok = false;
                None
            }
        },
        Some(other) => {
            issues.push(format!(
                "{path}.type: Expected {ARG_TYPE_LIST_LABEL}, received {}",
                yaml_type_name(other)
            ));
            ok = false;
            None
        }
    };

    let description = match map.get("description") {
        None => None,
        Some(serde_yaml::Value::String(text)) => Some(text.clone()),
        Some(other) => {
            issues.push(format!(
                "{path}.description: Expected string, received {}",
                yaml_type_name(other)
            ));
            ok = false;
            None
        }
    };

    let required = match map.get("required") {
        None => None,
        Some(serde_yaml::Value::Bool(flag)) => Some(*flag),
        Some(other) => {
            issues.push(format!(
                "{path}.required: Expected boolean, received {}",
                yaml_type_name(other)
            ));
            ok = false;
            None
        }
    };

    let default = match map.get("default") {
        None => None,
        Some(value) => match serde_json::to_value(value) {
            Ok(json) => Some(json),
            Err(error) => {
                issues.push(format!("{path}.default: {error}"));
                ok = false;
                None
            }
        },
    };

    let unknown: Vec<String> = map
        .keys()
        .map(key_label)
        .filter(|key| !ARG_DECLARATION_KEYS.contains(&key.as_str()))
        .map(|key| format!("'{key}'"))
        .collect();
    if !unknown.is_empty() {
        issues.push(format!(
            "{path}: Unrecognized key(s) in object: {}",
            unknown.join(", ")
        ));
        ok = false;
    }

    if !ok {
        return None;
    }
    // When ok is true, kind must be Some (missing type or wrong type will set ok false and return).
    kind.map(|kind| SavedWorkflowArgDeclaration {
        kind,
        description,
        required,
        default,
    })
}

/// Parse a saved workflow by name.
///
/// The name first passes the legality check and then the path is spelled out - this order is the defense line itself that the path traverses, not the input hygiene section.
/// If `scope` is given: only check that one; if not given: both are first-wins in order.
pub fn resolve_saved_workflow(
    cwd: &Path,
    name: &str,
    scope: Option<SavedWorkflowScope>,
    home_dir: &Path,
) -> SavedWorkflowResolveResult {
    if !is_valid_saved_workflow_name(name) {
        return SavedWorkflowResolveResult::Failed(SavedWorkflowResolveFailure::InvalidName {
            detail: invalid_name_detail(),
        });
    }
    let roots = match scope {
        Some(scope) => vec![saved_workflow_root(cwd, scope, home_dir)],
        None => saved_workflow_roots(cwd, home_dir).to_vec(),
    };

    for root in roots {
        let path = saved_workflow_path(&root, name);
        let source = match read_source(&path) {
            Ok(source) => source,
            // This one doesn't have it, look at the next one. The rest of the misreadings (permissions, directory) are problems with this file, please tell me.
            // Instead of pretending not to be found - "not found" will send the user to check for a name that actually exists.
            Err(error) if is_not_found(&error) => continue,
            Err(error) => {
                return SavedWorkflowResolveResult::Failed(SavedWorkflowResolveFailure::ReadError {
                    path: path_string(&path),
                    detail: describe_error(&error),
                })
            }
        };

        let parsed = match parse_saved_workflow(&source) {
            Ok(parsed) => parsed,
            Err(error) => {
                return SavedWorkflowResolveResult::Failed(
                    SavedWorkflowResolveFailure::ParseError {
                        path: path_string(&path),
                        detail: error.detail,
                    },
                )
            }
        };

        return SavedWorkflowResolveResult::Ok(ResolvedSavedWorkflow {
            name: name.to_string(),
            path: path_string(&path),
            scope: root.scope,
            meta: parsed.meta,
            script: parsed.script,
            source,
            body_line_offset: parsed.body_line_offset,
        });
    }

    SavedWorkflowResolveResult::Failed(SavedWorkflowResolveFailure::NotFound)
}

/// Enumerates save definitions under a scope root (tiled scan of depth 1, no recursion into subdirectories).
///
/// Bad files go into `invalid` instead of throwing an error: these files were manually modified by the user, and one typo shouldn't make the entire list disappear.
/// `dir` is returned even if it does not exist - the protocol layer's resulting schema requires it.
pub fn list_saved_workflows(
    cwd: &Path,
    scope: SavedWorkflowScope,
    home_dir: &Path,
) -> SavedWorkflowsListResult {
    let root = saved_workflow_root(cwd, scope, home_dir);
    let mut entries = Vec::new();
    let mut invalid = Vec::new();
    let mut claimed = HashSet::new();
    scan_scope_root(&root, &mut claimed, &mut entries, &mut invalid);
    SavedWorkflowsListResult {
        workflows: entries,
        invalid,
        dir: path_string(&root.dir),
    }
}

/// Undirected enumeration: `[project, global]` Scan in order, define first-wins with the same name; the obscured one does not appear in the list
/// - The list is about "what will be run when calling this name", not "how many copies are on the disk".
///
/// Use [`list_saved_workflows`] for directed enumeration (each root is scanned independently, without masking): the global group of the hub needs to see the
/// The portion covered by the project file.
pub fn list_all_saved_workflows(cwd: &Path, home_dir: &Path) -> SavedWorkflowListing {
    let mut entries = Vec::new();
    let mut invalid = Vec::new();
    let mut claimed = HashSet::new();
    for root in saved_workflow_roots(cwd, home_dir) {
        scan_scope_root(&root, &mut claimed, &mut entries, &mut invalid);
    }
    SavedWorkflowListing { entries, invalid }
}

fn scan_scope_root(
    root: &SavedWorkflowRoot,
    claimed: &mut HashSet<String>,
    entries: &mut Vec<SavedWorkflowEntry>,
    invalid: &mut Vec<SavedWorkflowInvalidEntry>,
) {
    let read_dir = match fs::read_dir(&root.dir) {
        Ok(read_dir) => read_dir,
        // It is normal that the directory does not exist (most projects have not saved the workflow), and it is not an error.
        Err(error) if is_not_found(&error) => return,
        Err(error) => {
            invalid.push(SavedWorkflowInvalidEntry {
                path: path_string(&root.dir),
                reason: describe_error(&error),
            });
            return;
        }
    };

    let mut file_names: Vec<String> = Vec::new();
    for entry in read_dir {
        match entry {
            Ok(entry) => file_names.push(entry.file_name().to_string_lossy().into_owned()),
            Err(error) => invalid.push(SavedWorkflowInvalidEntry {
                path: path_string(&root.dir),
                reason: describe_error(&error),
            }),
        }
    }
    // The order of readdir is file system dependent; ordering makes the list consistent on both machines. Legal names are all ASCII,
    // Verbatim identical to UTF-16 sorting of TS.
    file_names.sort();

    for file_name in file_names {
        let Some(name) = file_name.strip_suffix(".dwf.ts") else {
            continue;
        };
        let path = root.dir.join(&file_name);

        if !is_valid_saved_workflow_name(name) {
            invalid.push(SavedWorkflowInvalidEntry {
                path: path_string(&path),
                reason: UNUSABLE_FILE_NAME_REASON.to_string(),
            });
            continue;
        }
        // Already claimed by a higher-priority scope: this one cannot be reached, so it will not be listed. Claiming occurs after successful parsing,
        //So the broken project file will not block the global file with the same name.
        if claimed.contains(name) {
            continue;
        }

        // The directory entry exists but cannot be read (subdirectories, permissions) - it's not "no", it's "broken".
        let source = match read_source(&path) {
            Ok(source) => source,
            Err(error) => {
                invalid.push(SavedWorkflowInvalidEntry {
                    path: path_string(&path),
                    reason: describe_error(&error),
                });
                continue;
            }
        };

        match parse_saved_workflow(&source) {
            Ok(parsed) => {
                claimed.insert(name.to_string());
                entries.push(SavedWorkflowEntry {
                    name: name.to_string(),
                    description: parsed.meta.description.clone(),
                    when_to_use: parsed.meta.when_to_use.clone(),
                    args: parsed.meta.args.clone(),
                    scope: root.scope,
                    path: path_string(&path),
                });
            }
            Err(error) => invalid.push(SavedWorkflowInvalidEntry {
                path: path_string(&path),
                reason: error.to_string(),
            }),
        }
    }
}

/// Move the `name` of the global root to the project root of `cwd` (**only this way**).
///
/// Reverse (Project→Global) is not about moving files: most project files refer to the path/command/convention of this warehouse, just move them byte by byte.
/// A global definition that will inevitably break in other projects. Global→Project is a specialization, and a global definition will run if it falls into a certain project.
///
/// Move byte by byte (no parse, no reserialize: frontmatter does not store scope, so moving means moving files).
/// `rename` takes priority, and falls back to read → write → delete across devices; if the target already exists, it will be rejected (no overwriting - overwriting is a save with a confirmation window
/// Only available for actions). The name first goes through a legality check - the line of defense itself that the path crosses.
pub fn move_saved_workflow(cwd: &Path, name: &str, home_dir: &Path) -> SavedWorkflowMoveResult {
    if !is_valid_saved_workflow_name(name) {
        return SavedWorkflowMoveResult::InvalidName {
            detail: invalid_name_detail(),
        };
    }

    let from_root = saved_workflow_root(cwd, SavedWorkflowScope::Global, home_dir);
    let to_root = saved_workflow_root(cwd, SavedWorkflowScope::Project, home_dir);
    let from_path = saved_workflow_path(&from_root, name);
    let to_path = saved_workflow_path(&to_root, name);

    if !is_regular_file(&from_path) {
        return SavedWorkflowMoveResult::NotFound;
    }
    if is_regular_file(&to_path) {
        return SavedWorkflowMoveResult::TargetExists {
            path: path_string(&to_path),
        };
    }

    if let Err(error) = fs::create_dir_all(&to_root.dir) {
        return SavedWorkflowMoveResult::WriteError {
            path: path_string(&to_path),
            detail: describe_error(&error),
        };
    }

    match fs::rename(&from_path, &to_path) {
        Ok(()) => {
            return SavedWorkflowMoveResult::Moved {
                from: path_string(&from_path),
                to: path_string(&to_path),
            }
        }
        // When cross-device (for example, the home directory and the project branch have different mount points), rename reports EXDEV: read→write→delete fallback transfer,// The reading and writing are returned to their respective errors, so that the upper layer can say "the source cannot be read" and "the target cannot be written" as two different things.
        Err(error) if is_cross_device(&error) => {}
        Err(error) => {
            return SavedWorkflowMoveResult::WriteError {
                path: path_string(&to_path),
                detail: describe_error(&error),
            }
        }
    }

    let bytes = match fs::read(&from_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return SavedWorkflowMoveResult::ReadError {
                path: path_string(&from_path),
                detail: describe_error(&error),
            }
        }
    };
    // The same error attribution as TS: writing and deletion share the same write_error / toPath, because for the caller, both
    // "Not moved clean, go see the target".
    if let Err(error) = fs::write(&to_path, &bytes).and_then(|()| fs::remove_file(&from_path)) {
        return SavedWorkflowMoveResult::WriteError {
            path: path_string(&to_path),
            detail: describe_error(&error),
        };
    }
    SavedWorkflowMoveResult::Moved {
        from: path_string(&from_path),
        to: path_string(&to_path),
    }
}

fn invalid_name_detail() -> String {
    format!(
        "workflow names may only contain letters, digits, '.', '-' and '_', and must be 1-{SAVED_WORKFLOW_MAX_NAME_CHARS} characters"
    )
}

/// Delete only by name: the name is a priori and then the path is spelled out - this order is the defense line itself that the path crosses, so the protocol does not accept the path, nor does it
/// Accepts `..`. `scope` is passed in after being parsed by the protocol layer using `scopeOf` (default `project`).
pub fn delete_saved_workflow(
    cwd: &Path,
    name: &str,
    scope: SavedWorkflowScope,
    home_dir: &Path,
) -> SavedWorkflowDeleteResult {
    if !is_valid_saved_workflow_name(name) {
        return SavedWorkflowDeleteResult::InvalidName;
    }
    let root = saved_workflow_root(cwd, scope, home_dir);
    let path = saved_workflow_path(&root, name);
    match fs::remove_file(&path) {
        Ok(()) => SavedWorkflowDeleteResult::Deleted {
            path: path_string(&path),
        },
        Err(error) if is_not_found(&error) => SavedWorkflowDeleteResult::NotFound,
        // Node's unlink will always report read_error for other errors (permissions, target is directory), not not_found.
        Err(error) => SavedWorkflowDeleteResult::ReadError {
            detail: describe_error(&error),
        },
    }
}

/// Only rewrite the metadata at the top of the file: read back the current script text, and then overwrite the entire file as `serialize(newMeta, script)`.
/// Read-modify-write is completed in the same call; no three-party merge is performed (the file is small, stand-alone, and the user is modifying it himself).
///
/// The write failure is an uncaught `writeFile` rejection (protocol error) on the TS side, and the shared result schema does not exist
/// The corresponding failure branch, so `Err(io::Error)` is also used here to pop the stack instead of forging a schema.
/// `write_error` key.
pub fn update_saved_workflow_meta(
    cwd: &Path,
    name: &str,
    meta: &SavedWorkflowMeta,
    scope: SavedWorkflowScope,
    home_dir: &Path,
) -> Result<SavedWorkflowUpdateMetaResult, io::Error> {
    let resolved = match resolve_saved_workflow(cwd, name, Some(scope), home_dir) {
        SavedWorkflowResolveResult::Ok(resolved) => resolved,
        SavedWorkflowResolveResult::Failed(failure) => {
            return Ok(SavedWorkflowUpdateMetaResult::Failed(failure))
        }
    };
    let text = serialize_saved_workflow(meta, &resolved.script)
        .map_err(|error| io::Error::other(error.to_string()))?;
    fs::write(&resolved.path, text)?;
    Ok(SavedWorkflowUpdateMetaResult::Updated {
        path: resolved.path,
    })
}

fn read_source(path: &Path) -> io::Result<String> {
    let bytes = fs::read(path)?;
    // Node's `readFileSync(path, "utf8")` replaces illegal UTF-8 instead of reporting an error; the same semantics are used here,
    //Lest one bad byte turn "the file is damaged" into "cannot be read".
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn is_regular_file(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
}

/// ENOENT / ENOTDIR (`.zcode/workflows` files were created) are "not" for search purposes.
fn is_not_found(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// rename across volumes (Unix EXDEV / Windows ERROR_NOT_SAME_DEVICE), std are all classified as CrossesDevices.
fn is_cross_device(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::CrossesDevices
}

fn describe_error(error: &io::Error) -> String {
    error.to_string()
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Empty collection of JS `String.prototype.trim` (including U+FEFF): BOM written in Windows editor should not allow
/// sentinel judgment failed.
fn js_trim(line: &str) -> &str {
    line.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}')
}

fn yaml_type_name(value: &serde_yaml::Value) -> &'static str {
    match value {
        serde_yaml::Value::Null => "null",
        serde_yaml::Value::Bool(_) => "boolean",
        serde_yaml::Value::Number(_) => "number",
        serde_yaml::Value::String(_) => "string",
        serde_yaml::Value::Sequence(_) => "array",
        serde_yaml::Value::Mapping(_) => "object",
        serde_yaml::Value::Tagged(_) => "tagged value",
    }
}

fn key_label(key: &serde_yaml::Value) -> String {
    match key {
        serde_yaml::Value::String(text) => text.clone(),
        serde_yaml::Value::Number(number) => number.to_string(),
        serde_yaml::Value::Bool(flag) => flag.to_string(),
        serde_yaml::Value::Null => "null".to_string(),
        other => format!("<{}>", yaml_type_name(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        cwd: PathBuf,
        home: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            let root = std::env::temp_dir().join(format!(
                "zcode-saved-workflows-{}-{tag}-{nanos}",
                std::process::id()
            ));
            let cwd = root.join("workspace");
            let home = root.join("home");
            std::fs::create_dir_all(&cwd).expect("workspace dir");
            std::fs::create_dir_all(&home).expect("home dir");
            Self { root, cwd, home }
        }

        fn project_root(&self) -> SavedWorkflowRoot {
            saved_workflow_root(&self.cwd, SavedWorkflowScope::Project, &self.home)
        }

        fn write(
            &self,
            scope: SavedWorkflowScope,
            name: &str,
            meta: &SavedWorkflowMeta,
            script: &str,
        ) -> PathBuf {
            let root = saved_workflow_root(&self.cwd, scope, &self.home);
            std::fs::create_dir_all(&root.dir).expect("workflow dir");
            let path = saved_workflow_path(&root, name);
            let text = serialize_saved_workflow(meta, script).expect("serialize");
            std::fs::write(&path, text).expect("write workflow");
            path
        }

        fn write_raw(&self, scope: SavedWorkflowScope, file_name: &str, contents: &str) -> PathBuf {
            let root = saved_workflow_root(&self.cwd, scope, &self.home);
            std::fs::create_dir_all(&root.dir).expect("workflow dir");
            let path = root.dir.join(file_name);
            std::fs::write(&path, contents).expect("write raw");
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn simple_meta() -> SavedWorkflowMeta {
        SavedWorkflowMeta {
            description: "Review a pull request".to_string(),
            when_to_use: Some("When a PR needs a first pass".to_string()),
            args: None,
        }
    }

    fn meta_with_args() -> SavedWorkflowMeta {
        let mut args = SavedWorkflowArgsDeclaration::new();
        args.insert(
            "pr".to_string(),
            SavedWorkflowArgDeclaration {
                kind: SavedWorkflowArgType::Number,
                description: Some("PR number".to_string()),
                required: Some(true),
                default: None,
            },
        );
        SavedWorkflowMeta {
            description: "Review a pull request".to_string(),
            when_to_use: Some("When a PR needs a first pass".to_string()),
            args: Some(args),
        }
    }

    #[test]
    fn valid_workflow_round_trips_through_disk() {
        let fixture = Fixture::new("resolve");
        let path = fixture.write(
            SavedWorkflowScope::Project,
            "review-pr",
            &meta_with_args(),
            "log('hello');\n",
        );

        let listed = list_saved_workflows(&fixture.cwd, SavedWorkflowScope::Project, &fixture.home);
        assert_eq!(listed.workflows.len(), 1, "{listed:?}");
        assert!(listed.invalid.is_empty(), "{listed:?}");
        assert_eq!(
            listed.dir,
            path_string(&fixture.project_root().dir),
            "dir must be the scanned root"
        );
        let entry = &listed.workflows[0];
        assert_eq!(entry.name, "review-pr");
        assert_eq!(entry.description, "Review a pull request");
        assert_eq!(
            entry.when_to_use.as_deref(),
            Some("When a PR needs a first pass")
        );
        assert_eq!(entry.scope, SavedWorkflowScope::Project);
        assert_eq!(entry.path, path_string(&path));
        let args = entry.args.as_ref().expect("args");
        assert_eq!(args["pr"].kind, SavedWorkflowArgType::Number);
        assert_eq!(args["pr"].required, Some(true));

        let result = resolve_saved_workflow(&fixture.cwd, "review-pr", None, &fixture.home);
        let SavedWorkflowResolveResult::Ok(resolved) = result else {
            panic!("expected resolve ok, got {result:?}");
        };
        assert_eq!(resolved.name, "review-pr");
        assert_eq!(resolved.path, path_string(&path));
        assert_eq!(resolved.scope, SavedWorkflowScope::Project);
        assert_eq!(resolved.script, "log('hello');\n");
        assert_eq!(resolved.meta, meta_with_args());
        assert!(resolved.source.starts_with(SAVED_WORKFLOW_SENTINEL));
        // The purpose of bodyLineOffset: cut out the body text from the original text, and the cut out must be equal to script byte by byte.
        let body = resolved
            .source
            .split('\n')
            .skip(resolved.body_line_offset)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(body, resolved.script);
    }

    #[test]
    fn project_scope_shadows_global_in_first_wins_resolution() {
        let fixture = Fixture::new("shadowing");
        let project_path = fixture.write(
            SavedWorkflowScope::Project,
            "release",
            &SavedWorkflowMeta {
                description: "project copy".to_string(),
                when_to_use: None,
                args: None,
            },
            "project();",
        );
        let global_path = fixture.write(
            SavedWorkflowScope::Global,
            "release",
            &SavedWorkflowMeta {
                description: "global copy".to_string(),
                when_to_use: None,
                args: None,
            },
            "global();",
        );

        let SavedWorkflowResolveResult::Ok(first) =
            resolve_saved_workflow(&fixture.cwd, "release", None, &fixture.home)
        else {
            panic!("expected resolve ok");
        };
        assert_eq!(first.scope, SavedWorkflowScope::Project);
        assert_eq!(first.path, path_string(&project_path));

        let SavedWorkflowResolveResult::Ok(directed) = resolve_saved_workflow(
            &fixture.cwd,
            "release",
            Some(SavedWorkflowScope::Global),
            &fixture.home,
        ) else {
            panic!("expected directed resolve ok");
        };
        assert_eq!(directed.scope, SavedWorkflowScope::Global);
        assert_eq!(directed.path, path_string(&global_path));

        let listing = list_all_saved_workflows(&fixture.cwd, &fixture.home);
        assert_eq!(listing.entries.len(), 1, "{listing:?}");
        assert_eq!(listing.entries[0].scope, SavedWorkflowScope::Project);
        assert_eq!(listing.entries[0].path, path_string(&project_path));

        let directed_listing =
            list_saved_workflows(&fixture.cwd, SavedWorkflowScope::Global, &fixture.home);
        assert_eq!(directed_listing.workflows.len(), 1);
        assert_eq!(
            directed_listing.workflows[0].scope,
            SavedWorkflowScope::Global
        );
    }

    #[test]
    fn resolve_rejects_invalid_names_before_joining_paths() {
        let fixture = Fixture::new("names");
        // If name checking occurs after spelling the path, `../evil` will resolve to <cwd>/.zcode/evil.dwf.ts.
        let escape_path = fixture.cwd.join(".zcode").join("evil.dwf.ts");
        std::fs::create_dir_all(escape_path.parent().expect("parent")).expect("dir");
        std::fs::write(
            &escape_path,
            serialize_saved_workflow(&simple_meta(), "escaped();").expect("serialize"),
        )
        .expect("write escape");

        for name in [
            "", ".", "..", "...", "../evil", "a/b", "a\\b", "a b", "naïve",
        ] {
            let result = resolve_saved_workflow(&fixture.cwd, name, None, &fixture.home);
            let SavedWorkflowResolveResult::Failed(failure) = result else {
                panic!("name {name:?} must be rejected, got {result:?}");
            };
            assert_eq!(failure.reason(), "invalid_name", "name {name:?}");
        }

        let too_long = "a".repeat(SAVED_WORKFLOW_MAX_NAME_CHARS + 1);
        let SavedWorkflowResolveResult::Failed(failure) =
            resolve_saved_workflow(&fixture.cwd, &too_long, None, &fixture.home)
        else {
            panic!("over-long name must be rejected");
        };
        assert_eq!(failure.reason(), "invalid_name");
        assert_eq!(
            failure.detail(),
            Some(
                "workflow names may only contain letters, digits, '.', '-' and '_', and must be 1-64 characters"
            )
        );

        let max_length = "a".repeat(SAVED_WORKFLOW_MAX_NAME_CHARS);
        assert!(is_valid_saved_workflow_name(&max_length));
        assert!(!is_valid_saved_workflow_name(&too_long));
        assert!(is_valid_saved_workflow_name("a.b-c_1"));
        assert!(is_valid_saved_workflow_name("v1.2.3"));
    }

    #[test]
    fn resolve_reports_read_error_for_a_directory_entry() {
        let fixture = Fixture::new("read-error");
        let root = fixture.project_root();
        std::fs::create_dir_all(root.dir.join(saved_workflow_file_name("folder")))
            .expect("directory entry");

        let result = resolve_saved_workflow(&fixture.cwd, "folder", None, &fixture.home);
        let SavedWorkflowResolveResult::Failed(failure) = result else {
            panic!("directory must not resolve");
        };
        assert_eq!(failure.reason(), "read_error", "{failure:?}");
        assert_eq!(
            failure.path().map(str::to_string),
            Some(path_string(
                &root.dir.join(saved_workflow_file_name("folder"))
            ))
        );
        assert!(failure.detail().is_some_and(|detail| !detail.is_empty()));
    }

    #[test]
    fn resolve_reports_not_found() {
        let fixture = Fixture::new("missing");
        let result = resolve_saved_workflow(&fixture.cwd, "nope", None, &fixture.home);
        assert_eq!(
            result,
            SavedWorkflowResolveResult::Failed(SavedWorkflowResolveFailure::NotFound)
        );
    }

    #[test]
    fn parse_reports_missing_frontmatter() {
        let error = parse_saved_workflow("const x = 1;\n").expect_err("must fail");
        assert_eq!(
            error.reason,
            SavedWorkflowParseErrorReason::MissingFrontmatter
        );
        assert_eq!(
            error.detail,
            "file does not start with the `/* nc-code-workflow` metadata block"
        );
        assert_eq!(
            error.to_string(),
            "missing_frontmatter: file does not start with the `/* nc-code-workflow` metadata block"
        );
    }

    #[test]
    fn parse_allows_leading_blank_lines_and_utf8_bom() {
        let source = "\u{feff}\n\n/* nc-code-workflow\ndescription: with bom\n*/\nrun();";
        let parsed = parse_saved_workflow(source).expect("parse");
        assert_eq!(parsed.meta.description, "with bom");
        assert_eq!(parsed.script, "run();");
        assert_eq!(parsed.body_line_offset, 5);
    }

    #[test]
    fn parse_accepts_crlf_frontmatter() {
        let source = "/* nc-code-workflow\r\ndescription: windows\r\n*/\r\nrun();\r\n";
        let parsed = parse_saved_workflow(source).expect("parse crlf");
        assert_eq!(parsed.meta.description, "windows");
        assert_eq!(parsed.script, "run();\r\n");
        assert_eq!(parsed.body_line_offset, 3);
    }

    #[test]
    fn parse_reports_unterminated_frontmatter() {
        let error = parse_saved_workflow("/* nc-code-workflow\ndescription: open\nrun();")
            .expect_err("fail");
        assert_eq!(
            error.reason,
            SavedWorkflowParseErrorReason::UnterminatedFrontmatter
        );
        assert_eq!(error.detail, "metadata block is never closed with `*/`");
    }

    #[test]
    fn parse_reports_invalid_yaml() {
        let error = parse_saved_workflow("/* nc-code-workflow\ndescription: [unclosed\n*/\nrun();")
            .expect_err("fail");
        assert_eq!(error.reason, SavedWorkflowParseErrorReason::InvalidYaml);
        assert!(!error.detail.is_empty());
    }

    #[test]
    fn meta_validation_rejects_unknown_keys_and_empty_description() {
        let error = parse_saved_workflow_meta("description: \"\"\ndescripton: typo")
            .expect_err("must fail");
        assert_eq!(error.reason, SavedWorkflowParseErrorReason::InvalidMetadata);
        assert!(
            error
                .detail
                .starts_with("description: String must contain at least 1 character(s)"),
            "{error}"
        );
        assert!(
            error
                .detail
                .contains("(root): Unrecognized key(s) in object: 'descripton'"),
            "{error}"
        );

        let null_body = parse_saved_workflow_meta("").expect_err("null is not an object");
        assert_eq!(null_body.detail, "(root): Expected object, received null");
    }

    #[test]
    fn meta_validation_rejects_bad_argument_declarations() {
        let error = parse_saved_workflow_meta(
            "description: d\nargs:\n  pr: { type: integer }\n  area: { type: string, required: 'yes' }\n  extra: { type: string, bogus: 1 }\n",
        )
        .expect_err("must fail");
        assert_eq!(error.reason, SavedWorkflowParseErrorReason::InvalidMetadata);
        assert!(
            error
                .detail
                .contains("args.pr.type: Invalid enum value. Expected 'string' | 'number' | 'boolean' | 'json', received 'integer'"),
            "{error}"
        );
        assert!(
            error
                .detail
                .contains("args.area.required: Expected boolean, received string"),
            "{error}"
        );
        assert!(
            error
                .detail
                .contains("args.extra: Unrecognized key(s) in object: 'bogus'"),
            "{error}"
        );

        let accepted = parse_saved_workflow_meta(
            "description: d\nwhenToUse: w\nargs:\n  count: { type: number, required: true, default: 3, description: how many }\n",
        )
        .expect("valid meta");
        let count = &accepted.args.as_ref().expect("args")["count"];
        assert_eq!(count.kind, SavedWorkflowArgType::Number);
        assert_eq!(count.required, Some(true));
        assert_eq!(count.default, Some(JsonValue::from(3)));
        assert_eq!(count.description.as_deref(), Some("how many"));
    }

    #[test]
    fn serialize_uses_fixed_key_order_and_sentinel() {
        let text = serialize_saved_workflow(&meta_with_args(), "step();\n").expect("serialize");
        assert_eq!(text.split('\n').next(), Some(SAVED_WORKFLOW_SENTINEL));
        assert!(
            text.split('\n').any(|line| line == "*/"),
            "terminator must be its own line: {text}"
        );
        assert!(text.ends_with("\n*/\nstep();\n"), "{text}");

        let yaml = &text[SAVED_WORKFLOW_SENTINEL.len() + 1..];
        let description_at = yaml.find("description:").expect("description key");
        let when_at = yaml.find("whenToUse:").expect("whenToUse key");
        let args_at = yaml.find("args:").expect("args key");
        assert!(
            description_at < when_at && when_at < args_at,
            "key order must be description -> whenToUse -> args: {yaml}"
        );

        let minimal = SavedWorkflowMeta {
            description: "only description".to_string(),
            when_to_use: None,
            args: None,
        };
        let text = serialize_saved_workflow(&minimal, "").expect("serialize");
        assert_eq!(
            text,
            "/* nc-code-workflow\ndescription: only description\n*/\n"
        );
    }

    #[test]
    fn serialize_parse_round_trip_preserves_script_bytes() {
        let script = "const x = 1;\r\n  keep  trailing  \n\nlog(x);";
        let meta = meta_with_args();
        let text = serialize_saved_workflow(&meta, script).expect("serialize");
        let parsed = parse_saved_workflow(&text).expect("parse");
        assert_eq!(parsed.meta, meta);
        assert_eq!(parsed.script, script);
        let reconstructed = text
            .split('\n')
            .skip(parsed.body_line_offset)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(reconstructed, script);
    }

    #[test]
    fn serialize_preserves_explicit_null_default() {
        let mut args = SavedWorkflowArgsDeclaration::new();
        args.insert(
            "flag".to_string(),
            SavedWorkflowArgDeclaration {
                kind: SavedWorkflowArgType::Json,
                description: None,
                required: None,
                default: Some(JsonValue::Null),
            },
        );
        let meta = SavedWorkflowMeta {
            description: "null default".to_string(),
            when_to_use: None,
            args: Some(args),
        };
        let text = serialize_saved_workflow(&meta, "run();").expect("serialize");
        assert!(text.contains("default: null"), "{text}");
        let parsed = parse_saved_workflow(&text).expect("parse");
        let flag = &parsed.meta.args.as_ref().expect("args")["flag"];
        assert_eq!(flag.default, Some(JsonValue::Null));
        assert_eq!(parsed.meta, meta);
    }

    #[test]
    fn list_returns_dir_even_when_missing() {
        let fixture = Fixture::new("empty");
        let listed = list_saved_workflows(&fixture.cwd, SavedWorkflowScope::Project, &fixture.home);
        assert!(listed.workflows.is_empty());
        assert!(listed.invalid.is_empty());
        assert_eq!(listed.dir, path_string(&fixture.project_root().dir));
        assert!(!fixture.project_root().dir.exists());

        let all = list_all_saved_workflows(&fixture.cwd, &fixture.home);
        assert!(all.entries.is_empty());
        assert!(all.invalid.is_empty());
    }

    #[test]
    fn list_reports_invalid_files_without_failing() {
        let fixture = Fixture::new("invalid");
        fixture.write(
            SavedWorkflowScope::Project,
            "good",
            &simple_meta(),
            "good();",
        );
        let broken = fixture.write_raw(
            SavedWorkflowScope::Project,
            "broken.dwf.ts",
            "not a metadata block at all",
        );
        let folder = fixture.write_raw(SavedWorkflowScope::Project, "folder.dwf.ts", "");

        let listed = list_saved_workflows(&fixture.cwd, SavedWorkflowScope::Project, &fixture.home);
        assert_eq!(listed.workflows.len(), 1, "{listed:?}");
        assert_eq!(listed.workflows[0].name, "good");
        assert_eq!(listed.invalid.len(), 2, "{listed:?}");

        let broken_entry = listed
            .invalid
            .iter()
            .find(|entry| entry.path == path_string(&broken))
            .expect("broken entry");
        assert!(
            broken_entry
                .reason
                .starts_with("missing_frontmatter: file does not start with"),
            "{broken_entry:?}"
        );

        let folder_entry = listed
            .invalid
            .iter()
            .find(|entry| entry.path == path_string(&folder))
            .expect("folder entry");
        assert!(!folder_entry.reason.is_empty(), "{folder_entry:?}");
    }

    #[test]
    fn list_rejects_unusable_file_names() {
        let fixture = Fixture::new("unusable-name");
        let path = fixture.write_raw(
            SavedWorkflowScope::Project,
            "bad name.dwf.ts",
            serialize_saved_workflow(&simple_meta(), "run();")
                .expect("serialize")
                .as_str(),
        );

        let listed = list_saved_workflows(&fixture.cwd, SavedWorkflowScope::Project, &fixture.home);
        assert!(listed.workflows.is_empty(), "{listed:?}");
        assert_eq!(listed.invalid.len(), 1, "{listed:?}");
        assert_eq!(listed.invalid[0].path, path_string(&path));
        assert_eq!(
            listed.invalid[0].reason,
            "file name is not a usable workflow name"
        );
    }

    #[test]
    fn unscoped_listing_claims_names_only_after_a_successful_parse() {
        let fixture = Fixture::new("claim");
        let broken_project =
            fixture.write_raw(SavedWorkflowScope::Project, "shared.dwf.ts", "broken");
        let global = fixture.write(SavedWorkflowScope::Global, "shared", &simple_meta(), "g();");
        fixture.write(
            SavedWorkflowScope::Global,
            "only-global",
            &simple_meta(),
            "o();",
        );

        let listing = list_all_saved_workflows(&fixture.cwd, &fixture.home);
        assert_eq!(listing.entries.len(), 2, "{listing:?}");
        assert_eq!(listing.entries[0].name, "only-global");
        assert_eq!(listing.entries[0].scope, SavedWorkflowScope::Global);
        assert_eq!(listing.entries[1].name, "shared");
        assert_eq!(listing.entries[1].scope, SavedWorkflowScope::Global);
        assert_eq!(listing.entries[1].path, path_string(&global));
        assert_eq!(listing.invalid.len(), 1, "{listing:?}");
        assert_eq!(listing.invalid[0].path, path_string(&broken_project));
    }

    #[test]
    fn move_global_to_project_makes_project_win() {
        let fixture = Fixture::new("move");
        let global_path = fixture.write(
            SavedWorkflowScope::Global,
            "release",
            &simple_meta(),
            "release();\r\nsecond line",
        );
        let original_bytes = std::fs::read(&global_path).expect("read global");

        let SavedWorkflowResolveResult::Ok(before) =
            resolve_saved_workflow(&fixture.cwd, "release", None, &fixture.home)
        else {
            panic!("global copy must resolve before the move");
        };
        assert_eq!(before.scope, SavedWorkflowScope::Global);

        let result = move_saved_workflow(&fixture.cwd, "release", &fixture.home);
        let SavedWorkflowMoveResult::Moved { from, to } = result else {
            panic!("expected move ok, got {result:?}");
        };
        assert_eq!(from, path_string(&global_path));
        let project_path = saved_workflow_path(&fixture.project_root(), "release");
        assert_eq!(to, path_string(&project_path));
        assert!(!global_path.exists(), "source must be gone after the move");
        assert_eq!(
            std::fs::read(&project_path).expect("project copy"),
            original_bytes,
            "move must be byte-for-byte"
        );

        let SavedWorkflowResolveResult::Ok(after) =
            resolve_saved_workflow(&fixture.cwd, "release", None, &fixture.home)
        else {
            panic!("project copy must resolve after the move");
        };
        assert_eq!(after.scope, SavedWorkflowScope::Project);
        assert_eq!(after.path, to);
        assert_eq!(after.script, "release();\r\nsecond line");
    }

    #[test]
    fn move_refuses_to_overwrite_existing_target() {
        let fixture = Fixture::new("move-exists");
        let global_path = fixture.write(
            SavedWorkflowScope::Global,
            "release",
            &simple_meta(),
            "global();",
        );
        let project_path = fixture.write(
            SavedWorkflowScope::Project,
            "release",
            &simple_meta(),
            "project();",
        );
        let project_bytes = std::fs::read(&project_path).expect("read project");

        let result = move_saved_workflow(&fixture.cwd, "release", &fixture.home);
        let SavedWorkflowMoveResult::TargetExists { path } = result else {
            panic!("expected target_exists, got {result:?}");
        };
        assert_eq!(path, path_string(&project_path));
        assert!(global_path.exists(), "refused move must keep the source");
        assert_eq!(
            std::fs::read(&project_path).expect("read project"),
            project_bytes,
            "refused move must not touch the target"
        );
    }

    #[test]
    fn move_reports_not_found_and_invalid_name() {
        let fixture = Fixture::new("move-missing");
        assert_eq!(
            move_saved_workflow(&fixture.cwd, "nope", &fixture.home),
            SavedWorkflowMoveResult::NotFound
        );

        let result = move_saved_workflow(&fixture.cwd, "../evil", &fixture.home);
        let SavedWorkflowMoveResult::InvalidName { detail } = result else {
            panic!("expected invalid_name, got {result:?}");
        };
        assert!(detail.contains("1-64 characters"), "{detail}");
    }

    #[test]
    fn delete_removes_only_the_named_scope() {
        let fixture = Fixture::new("delete");
        let project_path = fixture.write(
            SavedWorkflowScope::Project,
            "release",
            &simple_meta(),
            "project();",
        );
        let global_path = fixture.write(
            SavedWorkflowScope::Global,
            "release",
            &simple_meta(),
            "global();",
        );

        let result = delete_saved_workflow(
            &fixture.cwd,
            "release",
            SavedWorkflowScope::Project,
            &fixture.home,
        );
        let SavedWorkflowDeleteResult::Deleted { path } = result else {
            panic!("expected delete ok, got {result:?}");
        };
        assert_eq!(path, path_string(&project_path));
        assert!(!project_path.exists(), "deleted scope must lose the file");
        assert!(global_path.exists(), "other scope must be untouched");

        let SavedWorkflowResolveResult::Ok(resolved) =
            resolve_saved_workflow(&fixture.cwd, "release", None, &fixture.home)
        else {
            panic!("global copy must resolve after the project copy was deleted");
        };
        assert_eq!(resolved.scope, SavedWorkflowScope::Global);

        assert_eq!(
            delete_saved_workflow(
                &fixture.cwd,
                "release",
                SavedWorkflowScope::Project,
                &fixture.home
            ),
            SavedWorkflowDeleteResult::NotFound
        );
    }

    #[test]
    fn delete_reports_invalid_name_and_not_found() {
        let fixture = Fixture::new("delete-failures");
        assert_eq!(
            delete_saved_workflow(
                &fixture.cwd,
                "../evil",
                SavedWorkflowScope::Project,
                &fixture.home
            ),
            SavedWorkflowDeleteResult::InvalidName
        );
        assert_eq!(
            delete_saved_workflow(
                &fixture.cwd,
                "nope",
                SavedWorkflowScope::Global,
                &fixture.home
            ),
            SavedWorkflowDeleteResult::NotFound
        );
    }

    #[test]
    fn update_meta_rewrites_metadata_and_keeps_script_bytes() {
        let fixture = Fixture::new("update-meta");
        let path = fixture.write(
            SavedWorkflowScope::Project,
            "release",
            &simple_meta(),
            "keep();\r\nlast line",
        );
        let updated_meta = SavedWorkflowMeta {
            description: "updated description".to_string(),
            when_to_use: None,
            args: None,
        };

        let result = update_saved_workflow_meta(
            &fixture.cwd,
            "release",
            &updated_meta,
            SavedWorkflowScope::Project,
            &fixture.home,
        )
        .expect("update meta");
        let SavedWorkflowUpdateMetaResult::Updated { path: updated_path } = result else {
            panic!("expected update ok, got {result:?}");
        };
        assert_eq!(updated_path, path_string(&path));

        let SavedWorkflowResolveResult::Ok(resolved) =
            resolve_saved_workflow(&fixture.cwd, "release", None, &fixture.home)
        else {
            panic!("updated file must resolve");
        };
        assert_eq!(resolved.meta, updated_meta);
        assert_eq!(resolved.script, "keep();\r\nlast line");
        assert!(
            !resolved.source.contains("whenToUse"),
            "rewritten metadata must drop the old key: {}",
            resolved.source
        );
    }

    #[test]
    fn update_meta_reports_resolve_failures() {
        let fixture = Fixture::new("update-meta-failures");
        let missing = update_saved_workflow_meta(
            &fixture.cwd,
            "nope",
            &simple_meta(),
            SavedWorkflowScope::Project,
            &fixture.home,
        )
        .expect("update returns a result");
        assert_eq!(
            missing,
            SavedWorkflowUpdateMetaResult::Failed(SavedWorkflowResolveFailure::NotFound)
        );

        let invalid = update_saved_workflow_meta(
            &fixture.cwd,
            "../evil",
            &simple_meta(),
            SavedWorkflowScope::Project,
            &fixture.home,
        )
        .expect("update returns a result");
        let SavedWorkflowUpdateMetaResult::Failed(failure) = invalid else {
            panic!("invalid name must fail");
        };
        assert_eq!(failure.reason(), "invalid_name");

        let json = serde_json::to_value(SavedWorkflowUpdateMetaResult::Failed(failure))
            .expect("update json");
        assert_eq!(json["ok"], false);
        assert_eq!(json["reason"], "invalid_name");
        assert!(
            json.get("path").is_none(),
            "protocol failure schema has no path: {json}"
        );
    }

    #[test]
    fn json_shapes_use_protocol_camel_case() {
        let fixture = Fixture::new("json");
        fixture.write(
            SavedWorkflowScope::Project,
            "review",
            &meta_with_args(),
            "run();",
        );

        let listed = list_saved_workflows(&fixture.cwd, SavedWorkflowScope::Project, &fixture.home);
        let json = serde_json::to_value(&listed).expect("list json");
        assert_eq!(json["dir"], path_string(&fixture.project_root().dir));
        let entry = &json["workflows"][0];
        assert_eq!(entry["whenToUse"], "When a PR needs a first pass");
        assert!(entry.get("when_to_use").is_none(), "{json}");
        assert_eq!(entry["args"]["pr"]["type"], "number");
        assert_eq!(entry["scope"], "project");

        let resolved = resolve_saved_workflow(&fixture.cwd, "review", None, &fixture.home);
        let json = serde_json::to_value(&resolved).expect("resolve json");
        assert_eq!(json["ok"], true);
        assert_eq!(json["name"], "review");
        assert!(
            json["bodyLineOffset"].as_u64().is_some(),
            "bodyLineOffset must be a number: {json}"
        );
        assert!(json.get("body_line_offset").is_none(), "{json}");
        assert_eq!(json["script"], "run();");

        let minimal = Fixture::new("json-minimal");
        minimal.write(
            SavedWorkflowScope::Project,
            "plain",
            &SavedWorkflowMeta {
                description: "only".to_string(),
                when_to_use: None,
                args: None,
            },
            "p();",
        );
        let resolved = resolve_saved_workflow(&minimal.cwd, "plain", None, &minimal.home);
        let json = serde_json::to_value(&resolved).expect("resolve json");
        assert_eq!(json["bodyLineOffset"], 3);

        let moved = move_saved_workflow(&fixture.cwd, "../evil", &fixture.home);
        let json = serde_json::to_value(&moved).expect("move json");
        assert_eq!(json["ok"], false);
        assert_eq!(json["reason"], "invalid_name");
        assert!(json["detail"].as_str().is_some_and(|text| !text.is_empty()));

        let review_path = saved_workflow_path(&fixture.project_root(), "review");
        let deleted = delete_saved_workflow(
            &fixture.cwd,
            "review",
            SavedWorkflowScope::Project,
            &fixture.home,
        );
        let json = serde_json::to_value(&deleted).expect("delete json");
        assert_eq!(json["ok"], true);
        assert_eq!(json["path"], path_string(&review_path));

        let invalid_delete = delete_saved_workflow(
            &fixture.cwd,
            "../evil",
            SavedWorkflowScope::Project,
            &fixture.home,
        );
        let json = serde_json::to_value(&invalid_delete).expect("delete json");
        assert_eq!(json["ok"], false);
        assert_eq!(json["reason"], "invalid_name");
        assert!(
            json.get("detail").is_none(),
            "delete's invalid_name carries no detail: {json}"
        );
    }
}
