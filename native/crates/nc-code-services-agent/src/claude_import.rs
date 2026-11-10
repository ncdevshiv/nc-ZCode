//! Claude Code native history import — source discovery and visible-text policy.
//!
//! Ownership (`specs/claude-session-import.md`): this module decides **which files are
//! importable and which text the user actually saw**; `nc-code-host::session_import` turns
//! the result into durable rows/history. The host never learns Claude's JSONL shape and this
//! module never writes transcript rows.
//!
//! NO-INVENTION: tool traffic, file changes, usage and the source runtime mode/model/provider
//! are not migrated. Only visible user/assistant text crosses the boundary.

use std::path::{Component, Path, PathBuf};

use nc_code_service_core::AppPaths;
use serde_json::{json, Value};

pub const PROVIDER: &str = "claude";
pub const MIGRATION_SOURCE: &str = "claudeCode";

/// Claude keeps its own subagent transcripts and temporary git worktrees next to real
/// projects; neither is a user workspace worth migrating.
const IGNORED_DIRS: &[&str] = &["subagents", "worktree", "worktrees"];
const HEAD_RECORDS: usize = 16;
const TITLE_CHARS: usize = 50;

#[derive(Clone, Debug)]
pub struct Candidate {
    pub session_id: String,
    pub workspace_path: String,
    pub source_path: PathBuf,
    pub updated_at: i64,
    pub created_at: Option<i64>,
    pub preview_title: Option<String>,
}

impl Candidate {
    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "provider": PROVIDER,
            "sessionId": self.session_id,
            "workspacePath": self.workspace_path,
            "sourcePath": self.source_path.to_string_lossy(),
            "updatedAt": self.updated_at,
        });
        if let Some(created_at) = self.created_at {
            value["createdAt"] = json!(created_at);
        }
        if let Some(title) = &self.preview_title {
            value["previewTitle"] = json!(title);
        }
        value
    }
}

#[derive(Clone, Debug)]
pub struct ImportedMessage {
    pub role: &'static str,
    pub content: String,
    pub timestamp: i64,
}

#[derive(Clone, Debug)]
pub struct ParsedSession {
    pub workspace_path: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub messages: Vec<ImportedMessage>,
}

impl ParsedSession {
    /// The host payload. `model` is deliberately absent: the source runtime selection must
    /// not leak into the workspace's current config on resume.
    pub fn host_payload(&self, session_id: &str) -> Value {
        json!({
            "sessionId": session_id,
            "title": self.title,
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
            "messages": self
                .messages
                .iter()
                .map(|message| json!({
                    "role": message.role,
                    "content": message.content,
                    "timestamp": message.timestamp,
                }))
                .collect::<Vec<_>>(),
        })
    }
}

/// `claude-import-<sha256("claude:<workspacePath>:<sessionId>")[..24]>` — deterministic, so
/// re-importing the same source targets the same task instead of creating a duplicate.
pub fn imported_task_id(workspace_path: &str, session_id: &str) -> String {
    let digest = nc_code_host::content_hash::sha256_hex(
        format!("claude:{workspace_path}:{session_id}").as_bytes(),
    );
    format!("claude-import-{}", &digest[..24])
}

/// The native projects roots. `ZCODE_DATA_BASE_DIR` can move `.zcode` elsewhere while
/// `.claude` stays under the real user home, so both are searched (de-duplicated).
pub fn projects_roots(paths: &AppPaths) -> Vec<PathBuf> {
    let mut homes: Vec<PathBuf> = vec![paths.home_dir().to_path_buf()];
    if let Ok(home) = std::env::var("HOME") {
        let home = home.trim();
        if !home.is_empty() {
            homes.push(PathBuf::from(home));
        }
    }
    homes.push(paths.data_base_dir().to_path_buf());
    let mut roots: Vec<PathBuf> = Vec::new();
    for home in homes {
        let root = home.join(".claude").join("projects");
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

pub fn scan(
    paths: &AppPaths,
    workspace_path: Option<&str>,
    modified_since: Option<i64>,
    limit: Option<usize>,
) -> Vec<Candidate> {
    scan_roots(
        &projects_roots(paths),
        workspace_path,
        modified_since,
        limit,
    )
}

/// The discovery core, separated from root resolution so tests can point at an isolated
/// tree instead of the developer's real `~/.claude`.
pub fn scan_roots(
    roots: &[PathBuf],
    workspace_path: Option<&str>,
    modified_since: Option<i64>,
    limit: Option<usize>,
) -> Vec<Candidate> {
    let filter = workspace_path.map(normalize_path);
    let mut files: Vec<(PathBuf, i64)> = Vec::new();
    for root in roots {
        collect_files(root, &mut files);
    }
    files.sort();
    files.dedup();
    let mut candidates: Vec<Candidate> = Vec::new();
    for (path, updated_at) in files {
        if let Some(since) = modified_since {
            if updated_at < since {
                continue;
            }
        }
        let Some(candidate) = candidate_from_file(&path, updated_at, filter.as_deref()) else {
            continue;
        };
        candidates.push(candidate);
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.updated_at));
    if let Some(limit) = limit.filter(|limit| *limit > 0) {
        candidates.truncate(limit);
    }
    candidates
}

/// Direct-by-id import reuses the scan boundary: a sidechain transcript or a temporary
/// worktree must not become importable just because the UI asked for its id.
pub fn find(paths: &AppPaths, workspace_path: Option<&str>, session_id: &str) -> Option<Candidate> {
    find_in_roots(&projects_roots(paths), workspace_path, session_id)
}

pub fn find_in_roots(
    roots: &[PathBuf],
    workspace_path: Option<&str>,
    session_id: &str,
) -> Option<Candidate> {
    let filter = workspace_path.map(normalize_path);
    let file_name = format!("{session_id}.jsonl");
    for root in roots {
        let Some(path) = find_session_file(root, &file_name) else {
            continue;
        };
        let updated_at = modified_ms(&path).unwrap_or(0);
        let Ok(head) = read_records(&path, Some(HEAD_RECORDS)) else {
            continue;
        };
        // A rejected boundary (sidechain / worktree) is terminal, not "try the next root".
        if has_sidechain_marker(&head) {
            return None;
        }
        let Some(workspace) = head_workspace(&head) else {
            continue;
        };
        if is_worktree_workspace(&workspace) {
            return None;
        }
        if let Some(candidate) = candidate_from_records(&path, updated_at, &head, filter.as_deref())
        {
            return Some(candidate);
        }
    }
    None
}

fn find_session_file(dir: &Path, file_name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if is_ignored_dir(&entry.file_name().to_string_lossy()) {
                continue;
            }
            if let Some(found) = find_session_file(&path, file_name) {
                return Some(found);
            }
        } else if entry.file_name().to_string_lossy() == file_name {
            return Some(path);
        }
    }
    None
}

fn candidate_from_file(path: &Path, updated_at: i64, filter: Option<&str>) -> Option<Candidate> {
    let head = read_records(path, Some(HEAD_RECORDS)).ok()?;
    if has_sidechain_marker(&head) {
        return None;
    }
    let workspace = head_workspace(&head)?;
    if is_worktree_workspace(&workspace) {
        return None;
    }
    candidate_from_records(path, updated_at, &head, filter)
}

fn candidate_from_records(
    path: &Path,
    updated_at: i64,
    head: &[Value],
    filter: Option<&str>,
) -> Option<Candidate> {
    let info = head_info(head);
    let workspace = info.workspace_path?;
    if let Some(filter) = filter {
        if normalize_path(&workspace) != filter {
            return None;
        }
    }
    // The file stem is the session id Claude itself uses; the in-file `sessionId` field can
    // be absent on head records that carry no message.
    let session_id = path.file_stem()?.to_string_lossy().to_string();
    Some(Candidate {
        session_id,
        workspace_path: workspace,
        source_path: path.to_path_buf(),
        updated_at,
        created_at: info.created_at,
        preview_title: info.preview_title,
    })
}

fn collect_files(dir: &Path, out: &mut Vec<(PathBuf, i64)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if !is_ignored_dir(&entry.file_name().to_string_lossy()) {
                collect_files(&path, out);
            }
        } else if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
            let updated_at = modified_ms(&path).unwrap_or(0);
            out.push((path, updated_at));
        }
    }
}

fn is_ignored_dir(name: &str) -> bool {
    let lower = name.to_lowercase();
    IGNORED_DIRS.contains(&lower.as_str())
}

fn modified_ms(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as i64)
}

/// Lexical `normalize(resolve(path))` + Windows case folding, used only for comparison.
/// `canonicalize` is not usable here: it yields `\\?\` UNC paths that would never equal the
/// caller's plain workspace string.
pub fn normalize_path(path: &str) -> String {
    let trimmed = path.trim();
    let absolute = if Path::new(trimmed).is_absolute() {
        PathBuf::from(trimmed)
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(trimmed)
    };
    let mut prefix = String::new();
    let mut parts: Vec<String> = Vec::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(value) => prefix = value.as_os_str().to_string_lossy().to_string(),
            Component::RootDir => parts.clear(),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(value) => parts.push(value.to_string_lossy().to_string()),
        }
    }
    let joined = if prefix.is_empty() {
        format!("/{}", parts.join("/"))
    } else {
        format!("{prefix}/{}", parts.join("/"))
    };
    if cfg!(windows) {
        joined.to_lowercase()
    } else {
        joined
    }
}

/// Claude puts temporary execution surfaces under `<repo>/.claude/worktree(s)/…`. Those are
/// short-lived, not projects the user wants in their task list.
pub fn is_worktree_workspace(path: &str) -> bool {
    let segments: Vec<String> = normalize_path(path)
        .split('/')
        .map(str::to_lowercase)
        .collect();
    segments
        .windows(2)
        .any(|pair| pair[0] == ".claude" && (pair[1] == "worktree" || pair[1] == "worktrees"))
}

/// A malformed line fails the whole read (TS `readJsonLinesFile` throws): importing half a
/// transcript would silently lose the user's history.
pub fn read_records(path: &Path, limit: Option<usize>) -> Result<Vec<Value>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut records = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value = serde_json::from_str::<Value>(line).map_err(|error| {
            format!(
                "failed to parse Claude JSONL {}:{}: {error}",
                path.display(),
                index + 1
            )
        })?;
        if !value.is_object() {
            return Err(format!(
                "failed to parse Claude JSONL {}:{}: not an object",
                path.display(),
                index + 1
            ));
        }
        records.push(value);
        if let Some(limit) = limit {
            if records.len() >= limit {
                break;
            }
        }
    }
    Ok(records)
}

fn object(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| value.is_object())
}

fn trimmed_str(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn is_true(value: Option<&Value>) -> bool {
    value.and_then(Value::as_bool) == Some(true)
}

fn message_of(entry: &Value) -> Option<&Value> {
    object(entry.get("message"))
}

fn request_of(entry: &Value) -> Option<&Value> {
    object(entry.get("request"))
}

pub fn has_sidechain_marker(records: &[Value]) -> bool {
    records.iter().any(|entry| {
        is_true(entry.get("isSidechain"))
            || is_true(message_of(entry).and_then(|message| message.get("isSidechain")))
            || is_true(request_of(entry).and_then(|request| request.get("isSidechain")))
    })
}

fn entry_workspace(entry: &Value) -> Option<String> {
    trimmed_str(entry.get("cwd"))
        .or_else(|| trimmed_str(message_of(entry).and_then(|message| message.get("cwd"))))
        .or_else(|| trimmed_str(request_of(entry).and_then(|request| request.get("cwd"))))
}

fn entry_model(entry: &Value) -> Option<String> {
    trimmed_str(entry.get("model"))
        .or_else(|| trimmed_str(message_of(entry).and_then(|message| message.get("model"))))
}

fn entry_timestamp(entry: &Value) -> Option<i64> {
    let message = message_of(entry);
    let request = request_of(entry);
    let candidates = [
        entry.get("timestamp"),
        entry.get("createdAt"),
        entry.get("updatedAt"),
        entry.get("created_at"),
        entry.get("updated_at"),
        entry.get("time"),
        message.and_then(|message| message.get("timestamp")),
        message.and_then(|message| message.get("createdAt")),
        message.and_then(|message| message.get("updatedAt")),
        request.and_then(|request| request.get("timestamp")),
    ];
    candidates.into_iter().find_map(to_timestamp_ms)
}

fn to_timestamp_ms(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => {
            let value = number.as_f64()?;
            if !value.is_finite() {
                return None;
            }
            if value > 1_000_000_000_000.0 {
                Some(value.trunc() as i64)
            } else if value > 1_000_000_000.0 {
                Some((value * 1000.0).trunc() as i64)
            } else {
                None
            }
        }
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            if let Ok(number) = text.parse::<f64>() {
                if number.is_finite() {
                    return to_timestamp_ms(Some(&json!(number)));
                }
            }
            parse_rfc3339_ms(text)
        }
        _ => None,
    }
}

/// RFC3339 with an optional fraction and either `Z` or a `±HH:MM` offset (TS `Date.parse`).
pub fn parse_rfc3339_ms(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || (bytes[10] != b'T' && bytes[10] != b't')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year: i64 = value.get(0..4)?.parse().ok()?;
    let month: i64 = value.get(5..7)?.parse().ok()?;
    let day: i64 = value.get(8..10)?.parse().ok()?;
    let hour: i64 = value.get(11..13)?.parse().ok()?;
    let minute: i64 = value.get(14..16)?.parse().ok()?;
    let second: i64 = value.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut index = 19;
    let millis = if bytes.get(19) == Some(&b'.') {
        let start = 20;
        let mut end = start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end == start {
            return None;
        }
        let mut fraction = value.get(start..end)?.to_string();
        while fraction.len() < 3 {
            fraction.push('0');
        }
        fraction.truncate(3);
        index = end;
        fraction.parse::<i64>().ok()?
    } else {
        0
    };
    let rest = value.get(index..)?;
    let offset_minutes = if rest.is_empty() || rest.eq_ignore_ascii_case("Z") {
        0
    } else {
        let sign = match rest.as_bytes().first() {
            Some(b'+') => 1,
            Some(b'-') => -1,
            _ => return None,
        };
        let body = rest.get(1..)?;
        if body.len() != 5 || body.as_bytes()[2] != b':' {
            return None;
        }
        let hours: i64 = body.get(0..2)?.parse().ok()?;
        let minutes: i64 = body.get(3..5)?.parse().ok()?;
        if hours > 23 || minutes > 59 {
            return None;
        }
        sign * (hours * 60 + minutes)
    };
    Some(
        days_from_civil(year, month, day) * 86_400_000
            + hour * 3_600_000
            + minute * 60_000
            + second * 1_000
            + millis
            - offset_minutes * 60_000,
    )
}

/// Howard Hinnant's days-from-civil (UTC).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = (month + 9) % 12;
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TagKind {
    IdeOpenedFile,
    Command,
}

/// Length of the opening tag at the start of `text`, if it matches `kind`.
fn open_tag_len(text: &str, kind: TagKind) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'<') {
        return None;
    }
    match kind {
        TagKind::IdeOpenedFile => {
            if text.get(..17)?.eq_ignore_ascii_case("<ide_opened_file>") {
                Some(17)
            } else {
                None
            }
        }
        TagKind::Command => {
            let rest = text.get(1..)?;
            let after = if rest.len() >= 14
                && rest
                    .get(..14)
                    .is_some_and(|head| head.eq_ignore_ascii_case("local-command-"))
            {
                rest.get(14..)?
            } else if rest.len() >= 8
                && rest
                    .get(..8)
                    .is_some_and(|head| head.eq_ignore_ascii_case("command-"))
            {
                rest.get(8..)?
            } else {
                return None;
            };
            let name_len = after.bytes().take_while(|byte| *byte != b'>').count();
            if name_len == 0 || after.as_bytes().get(name_len) != Some(&b'>') {
                return None;
            }
            Some(text.len() - after.len() + name_len + 1)
        }
    }
}

/// Byte length through the end of the first closing tag for `kind` (non-greedy `[\s\S]*?`).
fn close_tag_end(text: &str, kind: TagKind) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'<' && bytes.get(index + 1) == Some(&b'/') {
            let rest = text.get(index + 2..)?;
            let matched = match kind {
                TagKind::IdeOpenedFile => rest
                    .get(..16)
                    .is_some_and(|head| head.eq_ignore_ascii_case("ide_opened_file>")),
                TagKind::Command => {
                    let after = if rest.len() >= 14
                        && rest
                            .get(..14)
                            .is_some_and(|head| head.eq_ignore_ascii_case("local-command-"))
                    {
                        rest.get(14..)
                    } else if rest.len() >= 8
                        && rest
                            .get(..8)
                            .is_some_and(|head| head.eq_ignore_ascii_case("command-"))
                    {
                        rest.get(8..)
                    } else {
                        None
                    };
                    after.is_some_and(|after| {
                        let name_len = after.bytes().take_while(|byte| *byte != b'>').count();
                        name_len > 0 && after.as_bytes().get(name_len) == Some(&b'>')
                    })
                }
            };
            if matched {
                let after = match kind {
                    TagKind::IdeOpenedFile => index + 2 + 16,
                    TagKind::Command => {
                        let start = index + 2;
                        let rest = text.get(start..)?;
                        let skip = if rest.len() >= 14
                            && rest
                                .get(..14)
                                .is_some_and(|head| head.eq_ignore_ascii_case("local-command-"))
                        {
                            14
                        } else {
                            8
                        };
                        let after = rest.get(skip..)?;
                        let name_len = after.bytes().take_while(|byte| *byte != b'>').count();
                        start + skip + name_len + 1
                    }
                };
                return Some(after);
            }
        }
        index += 1;
    }
    None
}

/// Replace each matched tag block with a single space (the TS regex replacement), leaving an
/// unmatched opening tag alone so real content is never eaten.
fn strip_tag_blocks(text: &str, kind: TagKind) -> String {
    let mut out = String::new();
    let mut rest = text;
    loop {
        let Some(open_start) = rest.find('<') else {
            out.push_str(rest);
            break;
        };
        let after_open_start = &rest[open_start..];
        let Some(open_len) = open_tag_len(after_open_start, kind) else {
            out.push_str(&rest[..open_start + 1]);
            rest = &rest[open_start + 1..];
            continue;
        };
        let after_open = &after_open_start[open_len..];
        let Some(close_end) = close_tag_end(after_open, kind) else {
            out.push_str(&rest[..open_start + 1]);
            rest = &rest[open_start + 1..];
            continue;
        };
        out.push_str(&rest[..open_start]);
        out.push(' ');
        rest = &after_open[close_end..];
    }
    out
}

fn sanitize_visible_text(text: &str) -> String {
    let stripped = strip_tag_blocks(text, TagKind::IdeOpenedFile);
    let stripped = strip_tag_blocks(&stripped, TagKind::Command);
    stripped.replace("\r\n", "\n").trim().to_string()
}

/// Claude's SDK writes `model=<synthetic>` control placeholders as assistant bubbles. They
/// carry no user-visible value, but an API/login error written the same way *is* a bubble the
/// user saw, so it stays.
fn is_synthetic_placeholder(entry: &Value) -> bool {
    entry_model(entry).as_deref() == Some("<synthetic>")
        && !is_true(entry.get("isApiErrorMessage"))
        && !is_true(message_of(entry).and_then(|message| message.get("isApiErrorMessage")))
}

fn strip_no_response_placeholder(text: &str) -> String {
    let trimmed = text.trim_end();
    match trimmed.strip_suffix("No response requested.") {
        Some(head) => head.trim_end().to_string(),
        None => trimmed.to_string(),
    }
}

fn extract_text_field(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        value if value.is_object() => trimmed_str(value.get("text"))
            .or_else(|| trimmed_str(value.get("content")))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

pub fn extract_user_text(entry: &Value) -> Option<String> {
    if entry.get("type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let message = message_of(entry);
    if is_true(entry.get("isMeta")) || is_true(message.and_then(|message| message.get("isMeta"))) {
        return None;
    }
    let content = message
        .and_then(|message| message.get("content"))
        .filter(|value| !value.is_null())
        .or_else(|| request_of(entry).and_then(|request| request.get("prompt")))?;
    match content {
        Value::String(text) => {
            let normalized = sanitize_visible_text(text);
            (!normalized.is_empty()).then_some(normalized)
        }
        Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .filter_map(|item| match item {
                    Value::String(text) => Some(sanitize_visible_text(text)),
                    item if item.is_object() => {
                        if item.get("type").and_then(Value::as_str) == Some("tool_result") {
                            return None;
                        }
                        Some(sanitize_visible_text(&extract_text_field(item)))
                    }
                    _ => None,
                })
                .filter(|part| !part.is_empty())
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n\n"))
        }
        _ => None,
    }
}

pub fn extract_assistant_text(entry: &Value) -> Option<String> {
    if entry.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    if is_synthetic_placeholder(entry) {
        return None;
    }
    let content = message_of(entry).and_then(|message| message.get("content"))?;
    let text = match content {
        Value::String(text) => sanitize_visible_text(text),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(text) => Some(sanitize_visible_text(text)),
                item if item.is_object() => {
                    if item.get("type").and_then(Value::as_str) != Some("text") {
                        return None;
                    }
                    Some(sanitize_visible_text(&extract_text_field(item)))
                }
                _ => None,
            })
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(""),
        _ => return None,
    };
    if text.is_empty() {
        return None;
    }
    let visible = strip_no_response_placeholder(&text);
    (!visible.is_empty()).then_some(visible)
}

struct HeadInfo {
    workspace_path: Option<String>,
    preview_title: Option<String>,
    created_at: Option<i64>,
}

fn head_workspace(head: &[Value]) -> Option<String> {
    head_info(head).workspace_path
}

/// The workspace is the `cwd` of the first **visible business message**: queue/progress head
/// records can carry a deeper temporary cwd that does not own the session.
fn head_info(head: &[Value]) -> HeadInfo {
    let mut workspace_path = None;
    for entry in head {
        let user_text = extract_user_text(entry);
        let assistant_text = extract_assistant_text(entry);
        if workspace_path.is_none() && (user_text.is_some() || assistant_text.is_some()) {
            workspace_path = entry_workspace(entry);
        }
        let Some(user_text) = user_text else {
            continue;
        };
        return HeadInfo {
            workspace_path,
            preview_title: Some(derive_title(&user_text)),
            created_at: entry_timestamp(entry),
        };
    }
    HeadInfo {
        workspace_path,
        preview_title: None,
        created_at: None,
    }
}

pub fn derive_title(content: &str) -> String {
    if content.chars().count() > TITLE_CHARS {
        format!(
            "{}...",
            content.chars().take(TITLE_CHARS).collect::<String>()
        )
    } else {
        content.to_string()
    }
}

/// Parse a whole transcript into the visible user/assistant turns.
///
/// `fallback_created_at` / `fallback_updated_at` are the source file's birth/mtime, used only
/// when no visible message carries a timestamp.
pub fn parse_session(
    candidate: &Candidate,
    fallback_created_at: Option<i64>,
    fallback_updated_at: Option<i64>,
) -> Result<ParsedSession, String> {
    let records = read_records(&candidate.source_path, None)?;
    let now = now_ms();
    let mut messages: Vec<ImportedMessage> = Vec::new();
    let mut detected_workspace: Option<String> = None;
    let mut first_user_timestamp: Option<i64> = None;
    let mut updated_at = fallback_updated_at;
    let mut turn_index: i64 = -1;
    let mut pending_content = String::new();
    let mut pending_timestamp: Option<i64> = None;

    for entry in &records {
        if let Some(user_text) = extract_user_text(entry) {
            if detected_workspace.is_none() {
                detected_workspace = entry_workspace(entry);
            }
            // Claude splits one assistant answer across several events; aggregate them at
            // the next visible user turn so history stays "one user + one assistant".
            flush_assistant(
                &mut pending_content,
                &mut pending_timestamp,
                &mut updated_at,
                &mut messages,
                turn_index,
                now,
            );
            turn_index += 1;
            let timestamp = entry_timestamp(entry).or(updated_at).unwrap_or(now);
            if first_user_timestamp.is_none() {
                first_user_timestamp = Some(timestamp);
            }
            updated_at = Some(timestamp);
            messages.push(ImportedMessage {
                role: "user",
                content: user_text,
                timestamp,
            });
            continue;
        }
        let Some(assistant_text) = extract_assistant_text(entry) else {
            continue;
        };
        if detected_workspace.is_none() {
            detected_workspace = entry_workspace(entry);
        }
        if turn_index < 0 {
            continue;
        }
        pending_content.push_str(&assistant_text);
        pending_timestamp = entry_timestamp(entry).or(pending_timestamp).or(updated_at);
        updated_at = pending_timestamp.or(updated_at);
    }
    flush_assistant(
        &mut pending_content,
        &mut pending_timestamp,
        &mut updated_at,
        &mut messages,
        turn_index,
        now,
    );

    if messages.is_empty() {
        return Err(format!(
            "Claude session {} has no visible messages to import",
            candidate.session_id
        ));
    }
    let created_at = first_user_timestamp
        .or(fallback_created_at)
        .or(updated_at)
        .unwrap_or(now);
    let updated_at = updated_at
        .or(first_user_timestamp)
        .or(fallback_created_at)
        .unwrap_or(now)
        .max(created_at);
    let title = messages
        .iter()
        .find(|message| message.role == "user")
        .map(|message| derive_title(&message.content))
        .unwrap_or_else(|| "Imported session".to_string());
    Ok(ParsedSession {
        workspace_path: detected_workspace.unwrap_or_else(|| candidate.workspace_path.clone()),
        title,
        created_at,
        updated_at,
        messages,
    })
}

fn flush_assistant(
    pending_content: &mut String,
    pending_timestamp: &mut Option<i64>,
    updated_at: &mut Option<i64>,
    messages: &mut Vec<ImportedMessage>,
    turn_index: i64,
    now: i64,
) {
    if pending_content.is_empty() || turn_index < 0 {
        pending_content.clear();
        *pending_timestamp = None;
        return;
    }
    let timestamp = pending_timestamp.or(*updated_at).unwrap_or(now);
    messages.push(ImportedMessage {
        role: "assistant",
        content: std::mem::take(pending_content),
        timestamp,
    });
    *updated_at = pending_timestamp.or(*updated_at);
    *pending_timestamp = None;
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "zcode-claude-import-{}-{tag}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&root).expect("root");
        root
    }

    fn projects(root: &Path) -> Vec<PathBuf> {
        vec![root.join(".claude").join("projects")]
    }

    fn write_session(root: &Path, dir: &str, session_id: &str, entries: &[Value]) -> PathBuf {
        let project = root.join(".claude").join("projects").join(dir);
        std::fs::create_dir_all(&project).expect("project dir");
        let path = project.join(format!("{session_id}.jsonl"));
        let text = entries
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, format!("{text}\n")).expect("write jsonl");
        path
    }

    fn user(cwd: &str, text: &str, timestamp: &str) -> Value {
        json!({
            "type": "user",
            "cwd": cwd,
            "timestamp": timestamp,
            "message": { "role": "user", "content": text },
        })
    }

    fn assistant(cwd: &str, text: &str, timestamp: &str) -> Value {
        json!({
            "type": "assistant",
            "cwd": cwd,
            "timestamp": timestamp,
            "message": {
                "role": "assistant",
                "model": "claude-sonnet-4",
                "content": [{ "type": "text", "text": text }],
            },
        })
    }

    fn candidate_at(path: &Path, workspace: &str) -> Candidate {
        Candidate {
            session_id: path.file_stem().unwrap().to_string_lossy().to_string(),
            workspace_path: workspace.to_string(),
            source_path: path.to_path_buf(),
            updated_at: modified_ms(path).unwrap_or(0),
            created_at: None,
            preview_title: None,
        }
    }

    fn set_modified(path: &Path, time: std::time::SystemTime) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for mtime");
        file.set_modified(time).expect("set mtime");
    }

    #[test]
    fn timestamps_accept_rfc3339_offsets_and_epoch_units() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            parse_rfc3339_ms("2024-01-02T03:04:05.678Z"),
            Some(1_704_164_645_678)
        );
        assert_eq!(
            parse_rfc3339_ms("2024-01-02T03:04:05Z"),
            Some(1_704_164_645_000)
        );
        assert_eq!(
            parse_rfc3339_ms("2024-01-02T05:04:05+02:00"),
            Some(1_704_164_645_000)
        );
        assert_eq!(
            parse_rfc3339_ms("2024-01-02T01:04:05-02:00"),
            Some(1_704_164_645_000)
        );
        assert_eq!(parse_rfc3339_ms("2024-13-02T03:04:05Z"), None);
        assert_eq!(parse_rfc3339_ms("not a date"), None);
        assert_eq!(
            entry_timestamp(&json!({"timestamp": 1_704_164_645u64})),
            Some(1_704_164_645_000)
        );
        assert_eq!(
            entry_timestamp(&json!({"timestamp": 1_704_164_645_678u64})),
            Some(1_704_164_645_678)
        );
        assert_eq!(
            entry_timestamp(&json!({"timestamp": "1704164645678"})),
            Some(1_704_164_645_678)
        );
        assert_eq!(entry_timestamp(&json!({"timestamp": 42})), None);
        assert_eq!(
            entry_timestamp(&json!({"message": {"createdAt": "2024-01-02T03:04:05.678Z"}})),
            Some(1_704_164_645_678)
        );
    }

    #[test]
    fn ide_and_command_blocks_are_stripped_but_unmatched_tags_survive() {
        assert_eq!(
            sanitize_visible_text("before <ide_opened_file>secret path</ide_opened_file> after"),
            "before   after"
        );
        assert_eq!(
            sanitize_visible_text("run <local-command-stdout>noise</local-command-stdout> done"),
            "run   done"
        );
        assert_eq!(
            sanitize_visible_text("run <command-name>/clear</command-name> done"),
            "run   done"
        );
        assert_eq!(
            sanitize_visible_text("a <IDE_OPENED_FILE>x</IDE_OPENED_FILE> b"),
            "a   b"
        );
        // An opening tag with no close must not eat the rest of the message.
        assert_eq!(
            sanitize_visible_text("keep <ide_opened_file>unclosed"),
            "keep <ide_opened_file>unclosed"
        );
        assert_eq!(sanitize_visible_text("  a\r\nb  "), "a\nb");
        assert_eq!(
            sanitize_visible_text("<command->x</command->"),
            "<command->x</command->"
        );
    }

    #[test]
    fn meta_and_tool_result_user_entries_are_not_visible() {
        assert_eq!(
            extract_user_text(&json!({
                "type": "user",
                "isMeta": true,
                "message": { "content": "hidden" },
            })),
            None
        );
        assert_eq!(
            extract_user_text(&json!({
                "type": "user",
                "message": { "isMeta": true, "content": "hidden" },
            })),
            None
        );
        assert_eq!(
            extract_user_text(&json!({
                "type": "user",
                "message": { "content": [
                    { "type": "tool_result", "content": "tool noise" },
                    { "type": "text", "text": "real question" },
                ]},
            })),
            Some("real question".to_string())
        );
        assert_eq!(
            extract_user_text(&json!({
                "type": "user",
                "message": { "content": [
                    { "type": "tool_result", "content": "only noise" },
                ]},
            })),
            None
        );
        assert_eq!(
            extract_user_text(&json!({
                "type": "user",
                "message": { "content": ["one", "two"] },
            })),
            Some("one\n\ntwo".to_string())
        );
        assert_eq!(
            extract_user_text(&json!({
                "type": "user",
                "request": { "prompt": "legacy prompt" },
            })),
            Some("legacy prompt".to_string())
        );
        assert_eq!(extract_user_text(&json!({"type": "assistant"})), None);
    }

    #[test]
    fn synthetic_placeholders_are_dropped_but_api_errors_are_kept() {
        assert_eq!(
            extract_assistant_text(&json!({
                "type": "assistant",
                "message": { "model": "<synthetic>", "content": "No response requested." },
            })),
            None
        );
        assert_eq!(
            extract_assistant_text(&json!({
                "type": "assistant",
                "isApiErrorMessage": true,
                "message": { "model": "<synthetic>", "content": "API Error: 401 unauthorized" },
            })),
            Some("API Error: 401 unauthorized".to_string())
        );
        assert_eq!(
            extract_assistant_text(&json!({
                "type": "assistant",
                "message": { "content": [
                    { "type": "tool_use", "name": "Bash", "input": {} },
                    { "type": "text", "text": "visible" },
                ]},
            })),
            Some("visible".to_string())
        );
        assert_eq!(
            extract_assistant_text(&json!({
                "type": "assistant",
                "message": { "content": "answer\n\nNo response requested." },
            })),
            Some("answer".to_string())
        );
        assert_eq!(
            extract_assistant_text(&json!({
                "type": "assistant",
                "message": { "content": "No response requested." },
            })),
            None
        );
    }

    #[test]
    fn sidechain_markers_reject_a_transcript() {
        assert!(has_sidechain_marker(&[json!({"isSidechain": true})]));
        assert!(has_sidechain_marker(&[json!({
            "message": {"isSidechain": true}
        })]));
        assert!(has_sidechain_marker(&[json!({
            "request": {"isSidechain": true}
        })]));
        assert!(!has_sidechain_marker(&[json!({"isSidechain": false})]));
    }

    #[test]
    fn worktree_workspaces_and_ignored_dirs_are_excluded() {
        assert!(is_worktree_workspace("/repo/.claude/worktrees/agent-1"));
        assert!(is_worktree_workspace("C:\\Repo\\.claude\\Worktree\\a"));
        assert!(!is_worktree_workspace("/repo/.claude/projects"));
        assert!(!is_worktree_workspace("/repo/src"));

        let root = temp_root("ignored");
        let real = write_session(
            &root,
            "-repo",
            "real-session",
            &[user("/repo", "hello", "2024-01-02T03:04:05.678Z")],
        );
        for dir in ["subagents", "worktree", "worktrees", "SubAgents"] {
            write_session(
                &root,
                dir,
                &format!("{dir}-session"),
                &[user("/repo", "hidden", "2024-01-02T03:04:05.678Z")],
            );
        }
        let found = scan_roots(&projects(&root), None, None, None);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].source_path, real);
        assert_eq!(found[0].session_id, "real-session");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn task_id_is_the_deterministic_source_digest() {
        assert_eq!(
            imported_task_id("/ws/project", "session-1"),
            "claude-import-6f31abf018813b3d0cfac689"
        );
        assert_ne!(
            imported_task_id("/ws/project", "session-1"),
            imported_task_id("/ws/other", "session-1")
        );
    }

    #[test]
    fn scan_reports_candidates_sorted_filtered_and_limited() {
        let root = temp_root("scan");
        let older = write_session(
            &root,
            "-repo-a",
            "older",
            &[user(
                "/repo/a",
                "older question",
                "2024-01-02T03:04:05.678Z",
            )],
        );
        let newer = write_session(
            &root,
            "-repo-b",
            "newer",
            &[user(
                "/repo/b",
                "newer question",
                "2024-01-03T03:04:05.678Z",
            )],
        );
        set_modified(
            &older,
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_704_164_645_678),
        );
        set_modified(
            &newer,
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_704_251_045_678),
        );

        let all = scan_roots(&projects(&root), None, None, None);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].session_id, "newer");
        assert_eq!(all[0].workspace_path, "/repo/b");
        assert_eq!(all[0].preview_title.as_deref(), Some("newer question"));
        assert_eq!(all[0].created_at, Some(1_704_251_045_678));

        let filtered = scan_roots(&projects(&root), Some("/repo/a"), None, None);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].session_id, "older");

        let since = scan_roots(&projects(&root), None, Some(1_704_200_000_000), None);
        assert_eq!(since.len(), 1);
        assert_eq!(since[0].session_id, "newer");

        let limited = scan_roots(&projects(&root), None, None, Some(1));
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].session_id, "newer");
        // limit=0 means "no limit" (TS only slices when limit > 0).
        assert_eq!(scan_roots(&projects(&root), None, None, Some(0)).len(), 2);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn find_reuses_the_scan_boundary() {
        let root = temp_root("find");
        write_session(
            &root,
            "-repo",
            "good",
            &[user("/repo", "hello", "2024-01-02T03:04:05.678Z")],
        );
        write_session(
            &root,
            "-repo",
            "sidechain",
            &[
                json!({"isSidechain": true}),
                user("/repo", "hello", "2024-01-02T03:04:05.678Z"),
            ],
        );
        write_session(
            &root,
            "-repo",
            "worktree",
            &[user(
                "/repo/.claude/worktrees/tmp",
                "hello",
                "2024-01-02T03:04:05.678Z",
            )],
        );
        let roots = projects(&root);
        assert!(find_in_roots(&roots, None, "good").is_some());
        assert!(find_in_roots(&roots, None, "missing").is_none());
        assert!(find_in_roots(&roots, None, "sidechain").is_none());
        assert!(find_in_roots(&roots, None, "worktree").is_none());
        assert!(find_in_roots(&roots, Some("/elsewhere"), "good").is_none());
        assert!(find_in_roots(&roots, Some("/repo"), "good").is_some());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn parse_aggregates_split_assistant_events_and_keeps_source_times() {
        let root = temp_root("parse");
        let path = write_session(
            &root,
            "-repo",
            "split",
            &[
                user("/repo", "first question", "2024-01-02T03:04:05.678Z"),
                assistant("/repo", "part one", "2024-01-02T03:04:06.000Z"),
                assistant("/repo", "part two", "2024-01-02T03:04:07.000Z"),
                user("/repo", "second", "2024-01-02T03:04:10.000Z"),
                assistant("/repo", "answer two", "2024-01-02T03:04:12.000Z"),
            ],
        );
        let parsed = parse_session(&candidate_at(&path, "/repo"), None, None).expect("parse");
        assert_eq!(parsed.workspace_path, "/repo");
        assert_eq!(parsed.title, "first question");
        assert_eq!(parsed.created_at, 1_704_164_645_678);
        assert_eq!(parsed.updated_at, 1_704_164_652_000);
        assert_eq!(parsed.messages.len(), 4);
        assert_eq!(parsed.messages[0].role, "user");
        assert_eq!(parsed.messages[1].role, "assistant");
        assert_eq!(parsed.messages[1].content, "part onepart two");
        assert_eq!(parsed.messages[1].timestamp, 1_704_164_647_000);
        assert_eq!(parsed.messages[3].content, "answer two");
        // The host payload carries no source model: runtime selection must not leak.
        let payload = parsed.host_payload("claude-import-x");
        assert!(payload.get("model").is_none());
        assert!(payload["messages"][0].get("model").is_none());
        assert_eq!(payload["createdAt"], json!(1_704_164_645_678i64));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn an_orphan_assistant_and_an_empty_transcript_are_not_imported() {
        let root = temp_root("orphan");
        let path = write_session(
            &root,
            "-repo",
            "orphan",
            &[
                assistant("/repo", "before any user turn", "2024-01-02T03:04:05.678Z"),
                user("/repo", "real", "2024-01-02T03:04:06.000Z"),
            ],
        );
        let parsed = parse_session(&candidate_at(&path, "/repo"), None, None).expect("parse");
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.messages[0].role, "user");

        let empty = write_session(
            &root,
            "-repo",
            "empty",
            &[json!({"type": "summary", "summary": "nothing visible"})],
        );
        let error =
            parse_session(&candidate_at(&empty, "/repo"), None, None).expect_err("no messages");
        assert!(error.contains("no visible messages"), "{error}");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn workspace_comes_from_the_first_visible_message_not_a_queue_record() {
        let root = temp_root("cwd");
        let path = write_session(
            &root,
            "-repo",
            "queue",
            &[
                json!({"type": "queue-operation", "cwd": "/tmp/deeper"}),
                user("/real/workspace", "hello", "2024-01-02T03:04:05.678Z"),
            ],
        );
        let head = read_records(&path, Some(HEAD_RECORDS)).expect("head");
        assert_eq!(head_workspace(&head).as_deref(), Some("/real/workspace"));
        let parsed = parse_session(&candidate_at(&path, "/fallback"), None, None).expect("parse");
        assert_eq!(parsed.workspace_path, "/real/workspace");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_malformed_line_fails_the_whole_read() {
        let root = temp_root("malformed");
        let project = root.join(".claude").join("projects").join("-repo");
        std::fs::create_dir_all(&project).expect("project");
        let path = project.join("broken.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"user\",\"message\":{\"content\":\"ok\"}}\nnot json\n",
        )
        .expect("write");
        let error = read_records(&path, None).expect_err("malformed line");
        assert!(error.contains(":2"), "{error}");
        assert!(read_records(&path, Some(HEAD_RECORDS)).is_err());
        assert!(scan_roots(&projects(&root), None, None, None).is_empty());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn long_titles_are_truncated_like_the_source_product() {
        let long = "x".repeat(80);
        assert_eq!(derive_title(&long), format!("{}...", "x".repeat(50)));
        assert_eq!(derive_title("short"), "short");
    }

    #[test]
    fn normalize_path_resolves_dot_segments_and_folds_windows_case() {
        assert_eq!(
            normalize_path("/repo/./src/../lib"),
            normalize_path("/repo/lib")
        );
        assert_eq!(normalize_path("/repo/"), normalize_path("/repo"));
        if cfg!(windows) {
            assert_eq!(
                normalize_path("C:\\Repo\\Src"),
                normalize_path("c:/repo/src")
            );
        }
    }
}
