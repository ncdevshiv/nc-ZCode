//! Network + subprocess primitives for real marketplace/plugin sources:
//! bounded HTTPS JSON/zip fetches and the system `git` CLI, mirroring
//! `requestMarketplaceJson`, `zip-source.ts`/`github-archive-source.ts` and
//! `execGitCloneWithRetry` in `adapters/src/plugins`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nc_code_service_core::ServiceError;
use serde_json::Value;
use sha2::{Digest, Sha256};

const MAX_JSON_BYTES: usize = 10 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: usize = 256 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const JSON_TIMEOUT: Duration = Duration::from_secs(180);
const GIT_TIMEOUT: Duration = Duration::from_secs(90);
const GIT_CLONE_MAX_ATTEMPTS: usize = 3;
const RETRYABLE_GIT_MARKERS: [&str; 9] = [
    "RPC failed",
    "Operation timed out",
    "Recv failure",
    "expected flush",
    "early EOF",
    "remote end hung up",
    "HTTP/2 stream",
    "Connection reset",
    "failed to connect",
];

pub struct Fetcher {
    client: reqwest::Client,
}

impl Fetcher {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .user_agent("zcode-plugins/1.0")
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client");
        Self { client }
    }

    /// `requestMarketplaceJson`: manual bounded redirects, size-capped body, JSON.
    pub async fn get_marketplace_json(
        &self,
        url: &str,
        headers: Option<&Value>,
        cancelled: &AtomicBool,
    ) -> Result<Value, ServiceError> {
        let mut current_url = url.to_string();
        let mut current_headers = headers.cloned();
        for _ in 0..=MAX_REDIRECTS {
            check_cancelled(cancelled)?;
            let mut request = self.client.get(&current_url).timeout(JSON_TIMEOUT);
            if let Some(Value::Object(map)) = &current_headers {
                for (key, value) in map {
                    if let Some(text) = value.as_str() {
                        request = request.header(key.as_str(), text);
                    }
                }
            }
            let response = request.send().await.map_err(network_error)?;
            let status = response.status();
            if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                let Some(location) = location else {
                    return Err(ServiceError::new(format!(
                        "Marketplace redirect is missing Location header: {current_url}"
                    )));
                };
                let redirect_url = join_url(&current_url, &location);
                // Drop marketplace-specific headers when the redirect leaves the origin
                // (TS does the same to avoid leaking credentials to a CDN).
                if url_origin(&redirect_url).as_deref() != url_origin(&current_url).as_deref() {
                    current_headers = None;
                }
                current_url = redirect_url;
                continue;
            }
            if !status.is_success() {
                return Err(ServiceError::new(format!(
                    "Failed to fetch marketplace: {} {}",
                    status.as_u16(),
                    status.canonical_reason().unwrap_or("HTTP error")
                )));
            }
            let bytes = capped_bytes(response, MAX_JSON_BYTES, cancelled).await?;
            return serde_json::from_slice(&bytes).map_err(|error| {
                ServiceError::new(format!("Marketplace response is not valid JSON: {error}"))
            });
        }
        Err(ServiceError::new(format!(
            "Marketplace fetch exceeded redirect limit: {url}"
        )))
    }

    /// Download a zip archive (GitHub zipball / url-zip plugin source) with size cap
    /// and optional sha256 pin, then extract it to a fresh temp directory.
    pub async fn fetch_zip_archive(
        &self,
        url: &str,
        headers: Option<&Value>,
        sha256: Option<&str>,
        target_dir: &Path,
        cancelled: &AtomicBool,
    ) -> Result<(), ServiceError> {
        let mut request = self.client.get(url).timeout(JSON_TIMEOUT);
        if let Some(Value::Object(map)) = headers {
            for (key, value) in map {
                if let Some(text) = value.as_str() {
                    request = request.header(key.as_str(), text);
                }
            }
        }
        let response = request.send().await.map_err(network_error)?;
        let status = response.status();
        if !status.is_success() {
            return Err(ServiceError::named(
                "PluginZipDownloadError",
                format!("Plugin archive fetch failed with HTTP {}", status.as_u16()),
            )
            .with_code(match status.as_u16() {
                401 | 403 | 404 => format!("plugin_archive_status_{}", status.as_u16()),
                _ => "plugin_archive_fetch_failed".into(),
            }));
        }
        let bytes = capped_bytes(response, MAX_ARCHIVE_BYTES, cancelled).await?;
        if let Some(expected) = sha256 {
            let digest = hex_sha256(&bytes);
            if !digest.eq_ignore_ascii_case(expected) {
                return Err(ServiceError::new(format!(
                    "Plugin zip sha256 mismatch: expected {expected}, received {digest}"
                )));
            }
        }
        extract_zip(&bytes, target_dir)
    }

    /// Download an arbitrary URL with bounded redirects and a hard byte cap
    /// (`zip-source.ts` `downloadZipArchive`). Cross-origin redirects drop the
    /// caller headers so marketplace metadata is not leaked to a CDN.
    pub async fn download_zip_bytes(
        &self,
        url: &str,
        headers: Option<&Value>,
        max_bytes: usize,
        cancelled: &AtomicBool,
    ) -> Result<Vec<u8>, ServiceError> {
        let mut current_url = url.to_string();
        let mut current_headers = headers.cloned();
        for _ in 0..=MAX_REDIRECTS {
            check_cancelled(cancelled)?;
            let mut request = self.client.get(&current_url).timeout(JSON_TIMEOUT);
            if let Some(Value::Object(map)) = &current_headers {
                for (key, value) in map {
                    if let Some(text) = value.as_str() {
                        request = request.header(key.as_str(), text);
                    }
                }
            }
            let response = request.send().await.map_err(network_error)?;
            let status = response.status();
            if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                let Some(location) = location else {
                    return Err(ServiceError::new(format!(
                        "Plugin zip download redirect is missing Location header: {current_url}"
                    )));
                };
                let redirect_url = join_url(&current_url, &location);
                if url_origin(&redirect_url).as_deref() != url_origin(&current_url).as_deref() {
                    current_headers = None;
                }
                current_url = redirect_url;
                continue;
            }
            if !status.is_success() {
                return Err(ServiceError::named(
                    "PluginZipDownloadError",
                    format!("Failed to download plugin zip: {}", status.as_u16()),
                )
                .with_code(format!("plugin_zip_status_{}", status.as_u16())));
            }
            return capped_bytes(response, max_bytes, cancelled).await;
        }
        Err(ServiceError::new(format!(
            "Plugin zip download exceeded redirect limit: {url}"
        )))
    }

    /// `execGitCloneWithRetry` + optional sparse checkout and sha checkout.
    pub async fn git_clone(
        &self,
        url: &str,
        ref_name: Option<&str>,
        sha: Option<&str>,
        sparse_paths: &[String],
        target_dir: &Path,
        cancelled: &AtomicBool,
    ) -> Result<(), ServiceError> {
        let git_binary = std::env::var("ZCODE_GIT_BINARY")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "git".to_string());
        for attempt in 1..=GIT_CLONE_MAX_ATTEMPTS {
            check_cancelled(cancelled)?;
            if attempt > 1 {
                let _ = std::fs::remove_dir_all(target_dir);
                std::fs::create_dir_all(target_dir)
                    .map_err(|error| ServiceError::new(error.to_string()))?;
            }
            let mut args: Vec<String> = vec!["clone".into(), "--depth".into(), "1".into()];
            if sha.is_some() {
                args.retain(|arg| arg != "--depth" && arg != "1");
                args = vec!["clone".into()];
            }
            if let Some(value) = ref_name {
                args.extend(["--branch".into(), value.to_string()]);
            }
            if !sparse_paths.is_empty() {
                args.extend(["--filter=blob:none".into(), "--sparse".into()]);
            }
            args.extend([url.to_string(), target_dir.display().to_string()]);
            match run_git(&git_binary, &args, cancelled).await {
                Ok(()) => {
                    if !sparse_paths.is_empty() {
                        let mut set = vec![
                            "-C".to_string(),
                            target_dir.display().to_string(),
                            "sparse-checkout".into(),
                            "set".into(),
                        ];
                        set.extend(sparse_paths.iter().cloned());
                        run_git(&git_binary, &set, cancelled).await?;
                    }
                    if let Some(sha) = sha {
                        run_git(
                            &git_binary,
                            &[
                                "-C".to_string(),
                                target_dir.display().to_string(),
                                "checkout".into(),
                                sha.to_string(),
                            ],
                            cancelled,
                        )
                        .await?;
                    }
                    return Ok(());
                }
                Err(error) => {
                    let output = error.message.clone();
                    if attempt >= GIT_CLONE_MAX_ATTEMPTS
                        || !RETRYABLE_GIT_MARKERS
                            .iter()
                            .any(|marker| output.contains(marker))
                    {
                        return Err(error);
                    }
                    tokio::time::sleep(Duration::from_millis(1000 * attempt as u64)).await;
                }
            }
        }
        Err(ServiceError::new("git clone failed"))
    }
}

impl Default for Fetcher {
    fn default() -> Self {
        Self::new()
    }
}

/// Drain a response with a hard size cap and cooperative cancellation between chunks.
async fn capped_bytes(
    mut response: reqwest::Response,
    max: usize,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>, ServiceError> {
    let mut out: Vec<u8> = Vec::new();
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => return Err(network_error(error)),
        };
        if cancelled.load(Ordering::Relaxed) {
            return Err(ServiceError::named(
                "AbortError",
                "Plugin operation cancelled",
            ));
        }
        if out.len() + chunk.len() > max {
            return Err(ServiceError::new(format!(
                "plugin download exceeds the {max} byte limit: {}",
                response.url()
            )));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn extract_zip(bytes: &[u8], target_dir: &Path) -> Result<(), ServiceError> {
    use std::io::Cursor;
    std::fs::create_dir_all(target_dir)
        .map_err(|error| ServiceError::new(format!("create {}: {error}", target_dir.display())))?;
    let reader = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|error| ServiceError::new(format!("invalid plugin archive: {error}")))?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| ServiceError::new(error.to_string()))?;
        let raw_name = entry.name().to_string();
        let Some(relative) = validate_zip_path(&raw_name) else {
            continue; // absolute / traversal / drive-letter entries are skipped
        };
        let out_path = target_dir.join(relative);
        // zip 7.x no longer exposes ZipFileType; use is_dir/is_file to determine directories, and unix_mode to determine symbolic links
        // (S_IFLNK bit), symbolic links are never dropped (trust boundary, same as skill scanner).
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)
                .map_err(|error| ServiceError::new(error.to_string()))?;
        } else if is_symlink_mode(entry.unix_mode()) {
            return Err(ServiceError::new(
                "plugin zip entry symlinks are not supported",
            ));
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| ServiceError::new(error.to_string()))?;
            }
            let mut file = std::fs::File::create(&out_path)
                .map_err(|error| ServiceError::new(error.to_string()))?;
            std::io::copy(&mut entry, &mut file)
                .map_err(|error| ServiceError::new(error.to_string()))?;
        }
    }
    Ok(())
}

fn validate_zip_path(raw: &str) -> Option<PathBuf> {
    let normalized = raw.replace('\\', "/");
    if normalized.is_empty() || normalized.starts_with('/') || normalized.contains(":") {
        return None;
    }
    let mut path = PathBuf::new();
    for segment in normalized.split('/') {
        match segment {
            "" | "." => continue,
            ".." => return None,
            other => path.push(other),
        }
    }
    (!path.as_os_str().is_empty()).then_some(path)
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

async fn run_git(
    binary: &str,
    args: &[String],
    cancelled: &AtomicBool,
) -> Result<(), ServiceError> {
    use tokio::process::Command;
    check_cancelled(cancelled)?;
    let output = tokio::time::timeout(
        GIT_TIMEOUT,
        Command::new(binary)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output(),
    )
    .await
    .map_err(|_| ServiceError::new("Git command timed out"))?
    .map_err(|_| {
        ServiceError::named(
            "GitUnavailable",
            format!("system Git is required for git sources but was not available ({binary})"),
        )
        .with_code("plugin_git_unavailable")
    })?;
    if !output.status.success() {
        let mut message = String::from_utf8_lossy(&output.stderr).into_owned();
        message.push('\n');
        message.push_str(&String::from_utf8_lossy(&output.stdout));
        return Err(ServiceError::new(format!(
            "git {} failed: {}",
            args.first().cloned().unwrap_or_default(),
            message.trim()
        )));
    }
    check_cancelled(cancelled)?;
    Ok(())
}

pub fn check_cancelled(cancelled: &AtomicBool) -> Result<(), ServiceError> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(ServiceError::named(
            "AbortError",
            "Plugin operation canceled",
        ));
    }
    Ok(())
}

/// `sha256_hex` is public so callers can verify zip plugin sources they already hold.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex_sha256(bytes)
}

fn network_error(error: reqwest::Error) -> ServiceError {
    ServiceError::new(format!("plugin source request failed: {error}"))
}

fn join_url(base: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_string();
    }
    let Some(parsed_base) = url::Url::parse(base).ok() else {
        return location.to_string();
    };
    let suffix = location.trim_start_matches('/');
    format!(
        "{}://{}/{}",
        parsed_base.scheme(),
        parsed_base.host_str().unwrap_or_default(),
        suffix
    )
}

fn url_origin(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    Some(format!("{}://{}", parsed.scheme(), parsed.host_str()?))
}
/// The S_IFLNK bit of unix_mode determines symbolic links (POSIX high 4 bits 0o120000).
fn is_symlink_mode(mode: Option<u32>) -> bool {
    mode.is_some_and(|m| m & 0o170000 == 0o120000)
}
