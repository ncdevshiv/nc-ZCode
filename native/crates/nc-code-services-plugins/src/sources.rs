//! Marketplace source parsing and accessors — port of `parseMarketplaceSourceInput`
//! and the `MarketplaceSource` discriminated union in
//! `adapters/src/plugins/marketplace.ts`.

use std::path::{Path, PathBuf};

use nc_code_service_core::ServiceError;
use serde_json::{Map, Value};

use crate::fsutil::{is_directory, is_file, normalize_path, resolve_user_path};

pub const SOURCE_GITHUB: &str = "github";
pub const SOURCE_GIT: &str = "git";
pub const SOURCE_URL: &str = "url";
pub const SOURCE_FILE: &str = "file";
pub const SOURCE_DIRECTORY: &str = "directory";
pub const SOURCE_NPM: &str = "npm";
pub const SOURCE_SETTINGS: &str = "settings";

/// `parseMarketplaceSourceInput`: the entry point shared by `marketplace add` and
/// `validate --source`. `home` expands a leading `~` (TS `process.env.HOME`).
pub fn parse_marketplace_source_input(
    input: &str,
    home: &Path,
    working_directory: &Path,
) -> Result<Value, ServiceError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ServiceError::new("Marketplace source is empty"));
    }

    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        let (url, reference) = split_ref(trimmed);
        if url.ends_with(".git") || url.contains("/_git/") {
            return Ok(git_source(&url, reference.as_deref()));
        }
        if let Some(repository) = github_repository(&url) {
            let git_url = if url.ends_with(".git") {
                url.clone()
            } else {
                format!("{url}.git")
            };
            let _ = repository;
            return Ok(git_source(&git_url, reference.as_deref()));
        }
        return Ok(url_source(&url));
    }

    if is_git_ssh_url(trimmed) {
        let (url, reference) = split_ref(trimmed);
        return Ok(git_source(&url, reference.as_deref()));
    }

    if let Some(resolved) = resolve_path_input(trimmed, home, working_directory) {
        if !path_exists(&resolved) {
            return Err(ServiceError::new(format!(
                "Marketplace source path does not exist: {}",
                resolved.display()
            )));
        }
        if is_file(&resolved) {
            if !resolved.to_string_lossy().ends_with(".json") {
                return Err(ServiceError::new(format!(
                    "Marketplace file must be a .json file: {}",
                    resolved.display()
                )));
            }
            return Ok(file_source(&resolved));
        }
        if is_directory(&resolved) {
            return Ok(directory_source(&resolved));
        }
        return Err(ServiceError::new(format!(
            "Marketplace source path is not a file or directory: {}",
            resolved.display()
        )));
    }

    if trimmed.contains('/') && !trimmed.contains(':') {
        let (repo, reference) = split_github_shorthand(trimmed);
        return Ok(github_source(&repo, reference.as_deref()));
    }

    Err(ServiceError::new(format!(
        "Unsupported marketplace source: {input}"
    )))
}

pub fn source_kind(source: &Value) -> &str {
    source.get("source").and_then(Value::as_str).unwrap_or("")
}

pub fn source_str<'a>(source: &'a Value, key: &str) -> Option<&'a str> {
    source.get(key).and_then(Value::as_str)
}

pub fn source_string_list(source: &Value, key: &str) -> Vec<String> {
    source
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// `normalizeGitUrl`: `owner/repo` shorthand becomes a public GitHub HTTPS URL.
pub fn normalize_git_url(value: &str) -> String {
    let is_shorthand = {
        let mut parts = value.split('/');
        let first = parts.next().unwrap_or_default();
        let second = parts.next().unwrap_or_default();
        !first.is_empty() && !second.is_empty() && parts.next().is_none()
    };
    if is_shorthand && !value.contains(':') {
        format!("https://github.com/{value}.git")
    } else {
        value.to_string()
    }
}

/// GitHub shorthand (`owner/repo`, optional `#ref` or `@ref`).
pub fn split_github_shorthand(input: &str) -> (String, Option<String>) {
    let hash = input.rfind('#');
    let at = input.rfind('@');
    let index = match (hash, at) {
        (Some(hash), Some(at)) => hash.max(at),
        (Some(hash), None) => hash,
        (None, Some(at)) => at,
        (None, None) => return (input.to_string(), None),
    };
    if index == 0 {
        return (input.to_string(), None);
    }
    (
        input[..index].to_string(),
        Some(input[index + 1..].to_string()),
    )
}

fn split_ref(input: &str) -> (String, Option<String>) {
    match input.rfind('#') {
        Some(index) => (
            input[..index].to_string(),
            Some(input[index + 1..].to_string()),
        ),
        None => (input.to_string(), None),
    }
}

fn is_git_ssh_url(input: &str) -> bool {
    // TS `/^[a-zA-Z0-9._-]+@[^:]+:.+/`: user@host:path.
    let Some((user, rest)) = input.split_once('@') else {
        return false;
    };
    if user.is_empty()
        || !user
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        return false;
    }
    match rest.split_once(':') {
        Some((host, path)) => !host.is_empty() && !path.is_empty(),
        None => false,
    }
}

fn resolve_path_input(input: &str, home: &Path, working_directory: &Path) -> Option<PathBuf> {
    let looks_like_path = input.starts_with("./")
        || input.starts_with("../")
        || input.starts_with('/')
        || input.starts_with('~')
        || drive_letter_path(input);
    if !looks_like_path {
        return None;
    }
    if let Some(stripped) = input.strip_prefix('~') {
        let rest = stripped.trim_start_matches(['/', '\\']);
        return Some(normalize_path(&home.join(rest)));
    }
    let path = PathBuf::from(input);
    if path.is_absolute() {
        Some(normalize_path(&path))
    } else {
        Some(normalize_path(&working_directory.join(path)))
    }
}

/// Resolve a declaration path (`extraKnownMarketplaces` file/directory source)
/// against the declaring config directory, expanding a leading `~`.
pub fn resolve_source_path(raw: &str, base_directory: &Path, home: &Path) -> PathBuf {
    if raw.starts_with('~') {
        return resolve_user_path(raw, home);
    }
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        normalize_path(&path)
    } else {
        normalize_path(&base_directory.join(path))
    }
}

fn drive_letter_path(input: &str) -> bool {
    let bytes = input.as_bytes();
    bytes.len() > 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

fn path_exists(path: &Path) -> bool {
    std::fs::metadata(path).is_ok()
}

fn github_repository(url: &str) -> Option<(String, String)> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    if host != "github.com" && host != "www.github.com" {
        return None;
    }
    let segments: Vec<&str> = parsed
        .path()
        .trim_start_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.len() != 2 {
        return None;
    }
    Some((
        segments[0].to_string(),
        segments[1].trim_end_matches(".git").to_string(),
    ))
}

pub fn git_source(url: &str, reference: Option<&str>) -> Value {
    let mut map = Map::new();
    map.insert("source".into(), Value::String(SOURCE_GIT.into()));
    map.insert("url".into(), Value::String(url.to_string()));
    if let Some(reference) = reference {
        map.insert("ref".into(), Value::String(reference.to_string()));
    }
    Value::Object(map)
}

pub fn github_source(repo: &str, reference: Option<&str>) -> Value {
    let mut map = Map::new();
    map.insert("source".into(), Value::String(SOURCE_GITHUB.into()));
    map.insert("repo".into(), Value::String(repo.to_string()));
    if let Some(reference) = reference {
        map.insert("ref".into(), Value::String(reference.to_string()));
    }
    Value::Object(map)
}

fn url_source(url: &str) -> Value {
    let mut map = Map::new();
    map.insert("source".into(), Value::String(SOURCE_URL.into()));
    map.insert("url".into(), Value::String(url.to_string()));
    Value::Object(map)
}

fn file_source(path: &Path) -> Value {
    let mut map = Map::new();
    map.insert("source".into(), Value::String(SOURCE_FILE.into()));
    map.insert(
        "path".into(),
        Value::String(path.to_string_lossy().into_owned()),
    );
    Value::Object(map)
}

fn directory_source(path: &Path) -> Value {
    let mut map = Map::new();
    map.insert("source".into(), Value::String(SOURCE_DIRECTORY.into()));
    map.insert(
        "path".into(),
        Value::String(path.to_string_lossy().into_owned()),
    );
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/home/tester")
    }

    fn cwd() -> PathBuf {
        PathBuf::from("/work/project")
    }

    #[test]
    fn https_github_repository_becomes_git_source_with_ref() {
        let source =
            parse_marketplace_source_input("https://github.com/owner/repo#dev", &home(), &cwd())
                .expect("source");
        assert_eq!(source_kind(&source), SOURCE_GIT);
        assert_eq!(
            source_str(&source, "url"),
            Some("https://github.com/owner/repo.git")
        );
        assert_eq!(source_str(&source, "ref"), Some("dev"));
    }

    #[test]
    fn plain_https_url_stays_url_source() {
        let source =
            parse_marketplace_source_input("https://example.com/marketplace.json", &home(), &cwd())
                .expect("source");
        assert_eq!(source_kind(&source), SOURCE_URL);
        assert_eq!(
            source_str(&source, "url"),
            Some("https://example.com/marketplace.json")
        );
    }

    #[test]
    fn github_shorthand_and_ssh_inputs() {
        let source =
            parse_marketplace_source_input("owner/repo@main", &home(), &cwd()).expect("source");
        assert_eq!(source_kind(&source), SOURCE_GITHUB);
        assert_eq!(source_str(&source, "repo"), Some("owner/repo"));
        assert_eq!(source_str(&source, "ref"), Some("main"));

        let ssh = parse_marketplace_source_input("git@github.com:owner/repo.git", &home(), &cwd())
            .expect("ssh source");
        assert_eq!(source_kind(&ssh), SOURCE_GIT);
    }

    #[test]
    fn missing_paths_and_unknown_inputs_fail() {
        let error = parse_marketplace_source_input("./does-not-exist", &home(), &cwd())
            .expect_err("missing path");
        assert!(error.message.contains("does not exist"));
        let error =
            parse_marketplace_source_input("not-a-source", &home(), &cwd()).expect_err("unknown");
        assert!(error.message.contains("Unsupported marketplace source"));
    }

    #[test]
    fn normalize_git_url_expands_shorthand_only() {
        assert_eq!(
            normalize_git_url("owner/repo"),
            "https://github.com/owner/repo.git"
        );
        assert_eq!(normalize_git_url("https://x/y.git"), "https://x/y.git");
    }
}
