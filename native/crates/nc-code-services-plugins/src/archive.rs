//! Zip and GitHub-archive plugin sources — port of `adapters/src/plugins/zip-source.ts`,
//! `github-archive-source.ts` and the cleanup helpers in `helpers.ts`.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use nc_code_service_core::ServiceError;
use serde_json::{json, Value};

use crate::fetch::Fetcher;
use crate::fsutil::{is_directory, is_file, resolve_inside};

const ZIP_DOWNLOAD_MAX_BYTES: usize = 200 * 1024 * 1024;
const ZIP_EXTRACT_MAX_BYTES: u64 = 500 * 1024 * 1024;
const ZIP_MAX_ENTRIES: usize = 20_000;
const ZIP_MAX_SINGLE_FILE_BYTES: u64 = 50 * 1024 * 1024;
const DENIED_HEADERS: [&str; 4] = [
    "authorization",
    "cookie",
    "proxy-authorization",
    "set-cookie",
];

pub struct ResolvedArchive {
    pub path: PathBuf,
    pub cleanup: Option<PathBuf>,
}

/// `resolveZipPluginSource`: plugin manifest entries require a sha256.
pub async fn resolve_zip_plugin_source(
    fetcher: &Fetcher,
    url: &str,
    headers: Option<&Value>,
    sha256: &str,
    path: Option<&str>,
    strip_root: Option<bool>,
    cancelled: &AtomicBool,
) -> Result<ResolvedArchive, ServiceError> {
    validate_zip_download_url(url)?;
    if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ServiceError::new(
            "Plugin zip source sha256 must be a 64 character hex string",
        ));
    }
    validate_zip_headers(headers)?;
    if let Some(path) = path {
        normalize_zip_relative_path(path)?;
    }
    resolve_http_zip_source(
        fetcher,
        url,
        headers,
        Some(&sha256.to_ascii_lowercase()),
        path,
        strip_root,
        false,
        cancelled,
    )
    .await
}

/// `resolveHttpZipSource`: download (bounded), verify, extract, then resolve the root.
#[allow(clippy::too_many_arguments)]
pub async fn resolve_http_zip_source(
    fetcher: &Fetcher,
    url: &str,
    headers: Option<&Value>,
    sha256: Option<&str>,
    path: Option<&str>,
    strip_root: Option<bool>,
    require_single_root: bool,
    cancelled: &AtomicBool,
) -> Result<ResolvedArchive, ServiceError> {
    validate_zip_download_url(url)?;
    validate_zip_headers(headers)?;
    if let Some(path) = path {
        normalize_zip_relative_path(path)?;
    }
    if let Some(sha) = sha256 {
        if sha.len() != 64 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ServiceError::new(
                "Plugin zip source sha256 must be a 64 character hex string",
            ));
        }
    }
    let temp_root = temp_dir("zcode-plugin-zip-");
    std::fs::create_dir_all(&temp_root)
        .map_err(|error| ServiceError::new(format!("create {}: {error}", temp_root.display())))?;
    let result = async {
        let bytes = fetcher
            .download_zip_bytes(url, headers, ZIP_DOWNLOAD_MAX_BYTES, cancelled)
            .await?;
        let actual = crate::fetch::sha256_hex(&bytes);
        if let Some(expected) = sha256 {
            if actual != expected.to_ascii_lowercase() {
                return Err(ServiceError::new(format!(
                    "Plugin zip sha256 mismatch: expected={}, actual={}",
                    expected.to_ascii_lowercase(),
                    actual
                )));
            }
        }
        let extract_root = temp_root.join("extract");
        let top_level_segments = extract_zip(&bytes, &extract_root)?;
        let plugin_root = resolve_zip_root(
            &extract_root,
            path,
            require_single_root,
            strip_root,
            &top_level_segments,
        )?;
        Ok(ResolvedArchive {
            path: plugin_root,
            cleanup: Some(temp_root.clone()),
        })
    }
    .await;
    if result.is_err() {
        let _ = cleanup_temp_dir(Some(temp_root));
    }
    result
}

fn temp_dir(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{prefix}{}", uuid::Uuid::new_v4()))
}

fn validate_zip_headers(headers: Option<&Value>) -> Result<(), ServiceError> {
    let Some(headers) = headers else {
        return Ok(());
    };
    let Some(record) = headers.as_object() else {
        return Err(ServiceError::new(
            "Plugin zip source headers must be an object",
        ));
    };
    for (key, value) in record {
        if DENIED_HEADERS.contains(&key.to_ascii_lowercase().as_str()) {
            return Err(ServiceError::new(format!(
                "Plugin zip source header is not allowed: {key}"
            )));
        }
        if !value.is_string() {
            return Err(ServiceError::new(format!(
                "Plugin zip source header must be a string: {key}"
            )));
        }
    }
    Ok(())
}

pub fn validate_zip_download_url(value: &str) -> Result<(), ServiceError> {
    let parsed = url::Url::parse(value)
        .map_err(|_| ServiceError::new(format!("Plugin zip source URL is invalid: {value}")))?;
    if parsed.scheme() == "https" {
        return Ok(());
    }
    if parsed.scheme() == "http" && parsed.host_str().is_some_and(is_loopback_host) {
        return Ok(());
    }
    Err(ServiceError::new(format!(
        "Plugin zip source URL must be HTTPS: {value}"
    )))
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']).to_ascii_lowercase();
    if host == "localhost" || host == "::1" {
        return true;
    }
    host.split('.').count() == 4
        && host.split('.').all(|part| part.parse::<u8>().is_ok())
        && host.starts_with("127.")
}

fn normalize_zip_relative_path(path: &str) -> Result<String, ServiceError> {
    if path.contains('\0') || path.contains('\\') {
        return Err(ServiceError::new(format!("Unsafe plugin zip path: {path}")));
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed.starts_with('/') || trimmed.as_bytes().get(1) == Some(&b':') {
        return Err(ServiceError::new(format!("Unsafe plugin zip path: {path}")));
    }
    if trimmed
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(ServiceError::new(format!("Unsafe plugin zip path: {path}")));
    }
    Ok(trimmed.to_string())
}

fn extract_zip(bytes: &[u8], target_root: &Path) -> Result<Vec<String>, ServiceError> {
    use std::io::Cursor;
    std::fs::create_dir_all(target_root)
        .map_err(|error| ServiceError::new(format!("create {}: {error}", target_root.display())))?;
    let reader = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|error| ServiceError::new(format!("invalid plugin archive: {error}")))?;
    let mut top_level: Vec<String> = Vec::new();
    let mut extracted_bytes: u64 = 0;
    for index in 0..archive.len() {
        if index + 1 > ZIP_MAX_ENTRIES {
            return Err(ServiceError::new(format!(
                "Plugin zip has too many entries: {}/{ZIP_MAX_ENTRIES}",
                index + 1
            )));
        }
        let mut entry = archive
            .by_index(index)
            .map_err(|error| ServiceError::new(error.to_string()))?;
        let raw_name = entry.name().to_string();
        let normalized = normalize_zip_relative_path(&raw_name)?;
        if let Some(segment) = normalized.split('/').next() {
            if !top_level.iter().any(|existing| existing == segment) {
                top_level.push(segment.to_string());
            }
        }
        let out_path = resolve_zip_path_within(target_root, &normalized)?;
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)
                .map_err(|error| ServiceError::new(error.to_string()))?;
            continue;
        }
        if entry.unix_mode().is_some_and(is_symlink_mode) {
            return Err(ServiceError::new(format!(
                "Plugin zip entry symlinks are not supported: {raw_name}"
            )));
        }
        if entry.size() > ZIP_MAX_SINGLE_FILE_BYTES {
            return Err(ServiceError::new(format!(
                "Plugin zip entry exceeds single file limit: {raw_name}"
            )));
        }
        extracted_bytes += entry.size();
        if extracted_bytes > ZIP_EXTRACT_MAX_BYTES {
            return Err(ServiceError::new(format!(
                "Plugin zip extracted content exceeds limit: {extracted_bytes}/{ZIP_EXTRACT_MAX_BYTES}"
            )));
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| ServiceError::new(error.to_string()))?;
        }
        let mut file = std::fs::File::create(&out_path)
            .map_err(|error| ServiceError::new(error.to_string()))?;
        std::io::copy(&mut entry, &mut file)
            .map_err(|error| ServiceError::new(error.to_string()))?;
    }
    Ok(top_level)
}

fn is_symlink_mode(mode: u32) -> bool {
    mode & 0o170000 == 0o120000
}

fn resolve_zip_path_within(root: &Path, relative: &str) -> Result<PathBuf, ServiceError> {
    let mut path = root.to_path_buf();
    for part in relative.split('/') {
        path.push(part);
    }
    let normalized = crate::fsutil::normalize_path(&path);
    let normalized_root = crate::fsutil::normalize_path(root);
    if !normalized.starts_with(&normalized_root) || normalized == normalized_root {
        return Err(ServiceError::new(format!(
            "Unsafe plugin zip path: {relative}"
        )));
    }
    Ok(normalized)
}

fn resolve_zip_root(
    extract_root: &Path,
    path: Option<&str>,
    require_single_root: bool,
    strip_root: Option<bool>,
    top_level_segments: &[String],
) -> Result<PathBuf, ServiceError> {
    if require_single_root && top_level_segments.len() != 1 {
        return Err(ServiceError::new(format!(
            "Plugin zip must contain exactly one top-level directory: {}",
            top_level_segments.len()
        )));
    }
    if let Some(path) = path {
        let normalized = normalize_zip_relative_path(path)?;
        let requested = resolve_zip_path_within(extract_root, &normalized)?;
        if !is_directory(&requested) {
            return Err(ServiceError::new(format!(
                "Plugin zip source subdirectory does not exist: {path}"
            )));
        }
        return Ok(requested);
    }
    if has_plugin_manifest(extract_root) {
        return Ok(extract_root.to_path_buf());
    }
    if strip_root != Some(false) && top_level_segments.len() == 1 {
        let candidate = resolve_zip_path_within(extract_root, &top_level_segments[0])?;
        if is_directory(&candidate) {
            return Ok(candidate);
        }
    }
    if !is_directory(extract_root) {
        return Err(ServiceError::new(
            "Plugin zip did not extract a plugin root directory",
        ));
    }
    Ok(extract_root.to_path_buf())
}

pub fn has_plugin_manifest(root: &Path) -> bool {
    is_file(&root.join(".zcode-plugin/plugin.json"))
        || is_file(&root.join(".claude-plugin/plugin.json"))
        || is_file(&root.join(".codex-plugin/plugin.json"))
}

/// `resolveGitHubArchiveSource`: public GitHub HTTPS repositories only.
pub async fn resolve_github_archive_source(
    fetcher: &Fetcher,
    url: &str,
    pin: Option<&str>,
    path: Option<&str>,
    cancelled: &AtomicBool,
) -> Result<ResolvedArchive, ServiceError> {
    let Some((owner, repo)) = parse_public_github_repository(url) else {
        return Err(github_requires_git_error(format!(
            "source is not a public GitHub HTTPS repository: {url}"
        )));
    };
    let archive_url = build_github_archive_url(&owner, &repo, pin);
    let headers = json!({
        "Accept": "application/vnd.github+json",
        "User-Agent": "zcode-Plugin-Installer",
    });
    let resolved = resolve_http_zip_source(
        fetcher,
        &archive_url,
        Some(&headers),
        None,
        None,
        Some(true),
        true,
        cancelled,
    )
    .await?;
    let mut selected = resolved.path.clone();
    if let Some(path) = path {
        match resolve_inside(&resolved.path, path.trim_start_matches("./")) {
            Some(candidate) if is_directory(&candidate) => selected = candidate,
            _ => {
                let _ = cleanup_temp_dir(resolved.cleanup);
                return Err(ServiceError::new(format!(
                    "Plugin source subdirectory does not exist: {path}"
                )));
            }
        }
    }
    if let Some(reason) = detect_required_git_semantics(&resolved.path, &selected) {
        let _ = cleanup_temp_dir(resolved.cleanup);
        return Err(github_requires_git_error(reason));
    }
    Ok(ResolvedArchive {
        path: selected,
        cleanup: resolved.cleanup,
    })
}

fn parse_public_github_repository(value: &str) -> Option<(String, String)> {
    let parsed = url::Url::parse(value).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    if parsed.scheme() != "https"
        || (host != "github.com" && host != "www.github.com")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    let segments: Vec<&str> = parsed
        .path()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.len() != 2 {
        return None;
    }
    let owner = segments[0];
    let repo = segments[1].strip_suffix(".git").unwrap_or(segments[1]);
    let valid = |segment: &str| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
    };
    if !valid(owner) || !valid(repo) {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

fn build_github_archive_url(owner: &str, repo: &str, pin: Option<&str>) -> String {
    let pin = pin
        .map(str::trim)
        .filter(|pin| !pin.is_empty())
        .unwrap_or("HEAD");
    format!(
        "https://api.github.com/repos/{owner}/{repo}/zipball/{}",
        percent_encode(pin)
    )
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn github_requires_git_error(reason: String) -> ServiceError {
    ServiceError::named(
        "GitHubArchiveRequiresGitError",
        format!("GitHub Archive requires system Git fallback: {reason}"),
    )
}

pub fn should_fallback_github_archive_to_git(error: &ServiceError) -> bool {
    if error.name == "GitHubArchiveRequiresGitError" {
        return true;
    }
    if error.name == "PluginZipDownloadError" {
        let status = error
            .code
            .as_ref()
            .and_then(|code| code.as_str())
            .and_then(|code| code.strip_prefix("plugin_zip_status_"))
            .and_then(|status| status.parse::<u16>().ok());
        if matches!(status, Some(401 | 403 | 404)) {
            return true;
        }
    }
    let message = error.message.to_ascii_lowercase();
    message.contains("plugin zip entry symlinks are not supported")
        || message.contains("unsupported plugin zip entry type")
}

pub fn create_archive_fetch_error(source: &str, cause: &ServiceError) -> ServiceError {
    ServiceError::named(
        "PluginSourceMaterializationError",
        format!(
            "Failed to materialize public GitHub plugin source archive {source}: {}",
            cause.message
        ),
    )
    .with_code("plugin_archive_fetch_failed")
}

fn detect_required_git_semantics(repository_root: &Path, selected_root: &Path) -> Option<String> {
    if is_file(&repository_root.join(".gitmodules")) {
        return Some("repository declares Git submodules".to_string());
    }
    if directory_declares_git_lfs(repository_root, true) {
        return Some("repository declares Git LFS filters".to_string());
    }
    if selected_root != repository_root {
        if ancestor_directories_declare_git_lfs(repository_root, selected_root) {
            return Some("selected plugin path inherits Git LFS filters".to_string());
        }
        if directory_declares_git_lfs(selected_root, true) {
            return Some("selected plugin path declares Git LFS filters".to_string());
        }
    }
    None
}

fn ancestor_directories_declare_git_lfs(repository_root: &Path, selected_root: &Path) -> bool {
    let normalized_root = crate::fsutil::normalize_path(repository_root);
    let mut current = selected_root.parent().map(crate::fsutil::normalize_path);
    while let Some(directory) = current {
        if directory == normalized_root {
            break;
        }
        if directory_declares_git_lfs(&directory, false) {
            return true;
        }
        current = directory.parent().map(crate::fsutil::normalize_path);
    }
    false
}

fn directory_declares_git_lfs(root: &Path, recursive: bool) -> bool {
    let attributes = root.join(".gitattributes");
    if is_file(&attributes) {
        if let Ok(text) = std::fs::read_to_string(&attributes) {
            if text.split_whitespace().any(|token| token == "filter=lfs") {
                return true;
            }
        }
    }
    if !recursive {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    for entry in entries.flatten() {
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false)
            && directory_declares_git_lfs(&entry.path(), true)
        {
            return true;
        }
    }
    false
}

/// `cleanupPluginSourceBestEffort`: retries transient Windows locks, returns the last
/// error message instead of overwriting the primary failure.
pub fn cleanup_temp_dir(path: Option<PathBuf>) -> Option<String> {
    let path = path?;
    let mut last_error = None;
    for delay in [0u64, 25, 100] {
        if delay > 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay));
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => return None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => last_error = Some(error.to_string()),
        }
    }
    last_error
}

pub fn append_cleanup_error(mut error: ServiceError, cleanup: Option<String>) -> ServiceError {
    if let Some(cleanup) = cleanup {
        error.message = format!(
            "{}; plugin source cleanup also failed: {cleanup}",
            error.message
        );
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_paths_reject_traversal_and_absolute_entries() {
        assert!(normalize_zip_relative_path("../evil").is_err());
        assert!(normalize_zip_relative_path("/abs").is_err());
        assert!(normalize_zip_relative_path("a/../../b").is_err());
        assert_eq!(
            normalize_zip_relative_path("a/b/").expect("safe"),
            "a/b".to_string()
        );
    }

    #[test]
    fn github_archive_urls_encode_the_pin() {
        assert_eq!(
            build_github_archive_url("owner", "repo", Some("feature/x")),
            "https://api.github.com/repos/owner/repo/zipball/feature%2Fx"
        );
        assert_eq!(
            build_github_archive_url("owner", "repo", None),
            "https://api.github.com/repos/owner/repo/zipball/HEAD"
        );
    }

    #[test]
    fn only_public_github_https_repositories_are_archivable() {
        assert!(parse_public_github_repository("https://github.com/owner/repo").is_some());
        assert!(parse_public_github_repository("https://github.com/owner/repo.git").is_some());
        assert!(parse_public_github_repository("https://user:pw@github.com/o/r").is_none());
        assert!(parse_public_github_repository("http://github.com/o/r").is_none());
        assert!(parse_public_github_repository("https://example.com/o/r").is_none());
    }
}
