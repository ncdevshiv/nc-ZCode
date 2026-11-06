//! Chrome profile import for the Tauri shell.
//!
//! `importChromeBrowserData` copies the discovered Chrome profile's cookies into
//! the app's webview cookie store:
//!
//! - Windows: copy `Network/Cookies`, read the AES key from `Local State`
//!   (`os_crypt.encrypted_key`, DPAPI-protected), decrypt `v10`/`v11` AES-GCM
//!   values, and set them through CDP `Network.setCookie`.
//! - macOS: read `Chrome Safe Storage` from the Keychain, derive the AES-128 key
//!   with PBKDF2-HMAC-SHA1 (1003 iterations), decrypt `v10` AES-CBC values, and
//!   set them through Tauri's `WebviewWindow::set_cookie`.
//! - Linux: same AES-128-CBC path with Chrome's documented `peanuts` fallback
//!   (1 iteration), matching the Electron implementation.
//!
//! Chrome cookie schema 24+ prepends a 32-byte host digest to the plaintext; it
//! is stripped before the value is used.
//!
//! Local Storage import remains Windows-only: the Windows path drives a temp
//! Chrome over CDP `DOMStorage`, and Tauri has no cross-origin DOM-storage write
//! API on WKWebView/WebKitGTK. macOS/Linux report
//! `chrome_local_storage_import_failed` explicitly instead of pretending the
//! data was copied.

use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::Mutex;
#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
use base64::Engine;
use serde::{Deserialize, Serialize};
#[cfg(windows)]
use serde_json::{json, Value};
use tauri::WebviewWindow;

#[cfg(windows)]
const CDP_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_COOKIES: usize = 2_000;
const MAX_LOCAL_STORAGE_ORIGINS: usize = 1_000;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChromeBrowserDataImportOptions {
    #[serde(default)]
    pub allow_elevated_chrome_decryption: bool,
}

#[derive(Debug, Clone, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChromeCookieStats {
    pub imported: u64,
    pub skipped: u64,
    pub failed: u64,
}

#[derive(Debug, Clone, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChromeLocalStorageStats {
    pub origins_imported: u64,
    pub entries_imported: u64,
    pub origins_skipped: u64,
    pub origins_failed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChromeBrowserDataImportResult {
    pub success: bool,
    pub cookies: ChromeCookieStats,
    pub local_storage: ChromeLocalStorageStats,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issues: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ChromeBrowserDataImportResult {
    fn failure(error: &str) -> Self {
        Self {
            success: false,
            error: Some(error.to_string()),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone)]
struct CookieRecord {
    domain: String,
    name: String,
    value: String,
    path: String,
    secure: bool,
    http_only: bool,
    expires_unix: Option<f64>,
    same_site: Option<&'static str>,
}

fn chrome_expires_to_unix(expires_utc: i64) -> Option<f64> {
    if expires_utc <= 0 {
        return None;
    }
    // Chrome stores microseconds since 1601-01-01; Unix epoch offset is 11644473600s.
    Some((expires_utc as f64 / 1_000_000.0) - 11_644_473_600.0)
}

fn same_site_name(value: i64) -> Option<&'static str> {
    match value {
        0 => Some("None"),
        1 => Some("Lax"),
        2 => Some("Strict"),
        _ => None,
    }
}

/// PBKDF2-HMAC-SHA1 (RFC 2898). Chrome derives its CBC key with this on
/// macOS (1003 iterations) and Linux (1 iteration, `peanuts`).
#[cfg(any(test, target_os = "macos", target_os = "linux"))]
pub(crate) fn pbkdf2_sha1(
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    key_len: usize,
) -> Vec<u8> {
    use hmac::{Hmac, KeyInit, Mac};
    use sha1::Sha1;

    type HmacSha1 = Hmac<Sha1>;
    let iterations = iterations.max(1);
    let mut output = Vec::with_capacity(key_len);
    let mut block_index: u32 = 1;
    while output.len() < key_len {
        let mut mac = HmacSha1::new_from_slice(password).expect("HMAC accepts any key length");
        mac.update(salt);
        mac.update(&block_index.to_be_bytes());
        let mut previous = mac.finalize().into_bytes();
        let mut accumulated = previous;
        for _ in 1..iterations {
            let mut mac = HmacSha1::new_from_slice(password).expect("HMAC accepts any key length");
            mac.update(&previous);
            previous = mac.finalize().into_bytes();
            for (target, value) in accumulated.iter_mut().zip(previous.iter()) {
                *target ^= *value;
            }
        }
        output.extend_from_slice(&accumulated);
        block_index += 1;
    }
    output.truncate(key_len);
    output
}

/// Chrome cookie schema 24+ prepends `SHA-256(host_key)` (32 bytes) to the
/// decrypted value; strip it before the cookie is used.
#[cfg(any(test, target_os = "macos", target_os = "linux"))]
pub(crate) fn strip_host_digest(plaintext: Vec<u8>, schema_version: i64) -> Vec<u8> {
    if schema_version >= 24 && plaintext.len() >= 32 {
        plaintext[32..].to_vec()
    } else {
        plaintext
    }
}

/// Chrome's macOS/Linux cookie protection: AES-128-CBC with a 16-space IV.
#[cfg(any(test, target_os = "macos", target_os = "linux"))]
fn aes_128_cbc_decrypt(key: &[u8], iv: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    use aes::Aes128;
    use cbc::cipher::{block_padding::Pkcs7, BlockModeDecrypt, KeyIvInit};

    type Aes128CbcDec = cbc::Decryptor<Aes128>;
    Aes128CbcDec::new_from_slices(key, iv)
        .map_err(|_| "invalid_key".to_string())?
        .decrypt_padded_vec::<Pkcs7>(ciphertext)
        .map_err(|_| "chrome_cookie_app_bound_decryption_failed".to_string())
}

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
pub(crate) fn decrypt_cookie_value_cbc(
    key: &[u8],
    encrypted: &[u8],
    schema_version: i64,
) -> Result<String, String> {
    if key.len() != 16 {
        return Err("invalid_key".to_string());
    }
    if encrypted.len() <= 3 || &encrypted[..3] != b"v10" {
        return Err("chrome_cookie_protection_unsupported".to_string());
    }
    let iv = [0x20u8; 16];
    let plaintext = aes_128_cbc_decrypt(key, &iv, &encrypted[3..])?;
    let plaintext = strip_host_digest(plaintext, schema_version);
    String::from_utf8(plaintext).map_err(|_| "invalid_utf8".to_string())
}

/// macOS `security` exit codes that mean the user denied Keychain access
/// (errSecInteractionNotAllowed / errSecAuthFailed / errSecUserCanceled).
#[cfg(any(test, target_os = "macos"))]
pub(crate) fn map_macos_keychain_exit(code: i32) -> Option<&'static str> {
    match code {
        36 | 51 | 128 => Some("chrome_cookie_access_denied"),
        _ => None,
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn first_existing_dir(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|path| path.is_dir())
}

#[cfg(windows)]
fn decrypt_cookie_value(key: &[u8], encrypted: &[u8]) -> Result<String, String> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    if encrypted.len() < 3 + 12 + 16 {
        return Err("chrome_cookie_protection_unsupported".to_string());
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| "invalid_key".to_string())?;
    let nonce = Nonce::from_slice(&encrypted[3..15]);
    let plaintext = cipher
        .decrypt(nonce, &encrypted[15..])
        .map_err(|_| "chrome_cookie_app_bound_decryption_failed".to_string())?;
    String::from_utf8(plaintext).map_err(|_| "invalid_utf8".to_string())
}

fn parse_local_storage_origins(bytes: &[u8]) -> Vec<String> {
    const PREFIX: &[u8] = b"META:";
    let mut origins = Vec::new();
    let mut index = 0;
    while index + PREFIX.len() < bytes.len() {
        if &bytes[index..index + PREFIX.len()] != PREFIX {
            index += 1;
            continue;
        }
        let start = index + PREFIX.len();
        let mut end = start;
        while end < bytes.len() {
            let byte = bytes[end];
            if byte.is_ascii_alphanumeric()
                || matches!(byte, b':' | b'/' | b'.' | b'-' | b'_' | b'[' | b']' | b'%')
            {
                end += 1;
            } else {
                break;
            }
        }
        if end > start {
            if let Ok(text) = std::str::from_utf8(&bytes[start..end]) {
                if (text.starts_with("http://") || text.starts_with("https://"))
                    && !origins.iter().any(|item| item == text)
                {
                    origins.push(text.to_string());
                }
            }
        }
        index = end.max(index + 1);
    }
    origins
}

#[cfg(windows)]
fn chrome_user_data_dir() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(
        PathBuf::from(local)
            .join("Google")
            .join("Chrome")
            .join("User Data"),
    )
}

#[cfg(target_os = "macos")]
fn chrome_user_data_dir() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    first_existing_dir([
        home.join("Library")
            .join("Application Support")
            .join("Google")
            .join("Chrome"),
        home.join("Library")
            .join("Application Support")
            .join("Chromium"),
    ])
}

#[cfg(target_os = "linux")]
fn chrome_user_data_dir() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    const CANDIDATES: [&str; 6] = [
        "google-chrome",
        "google-chrome-beta",
        "google-chrome-unstable",
        "chromium",
        "microsoft-edge",
        "brave-browser",
    ];
    first_existing_dir(CANDIDATES.iter().map(|name| config.join(name)))
}

fn newest_profile(user_data: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(user_data).ok()? {
        let entry = entry.ok()?;
        if !entry.file_type().ok()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name != "Default" && !name.starts_with("Profile ") {
            continue;
        }
        let profile = entry.path();
        let cookies = cookies_path(&profile);
        let modified = std::fs::metadata(&cookies)
            .and_then(|metadata| metadata.modified())
            .ok();
        if modified.is_some() {
            candidates.push((modified, profile));
        }
    }
    candidates
        .into_iter()
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, profile)| profile)
}

fn cookies_path(profile: &Path) -> PathBuf {
    let network = profile.join("Network").join("Cookies");
    if network.is_file() {
        network
    } else {
        profile.join("Cookies")
    }
}

#[cfg(windows)]
fn dpapi_unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe { CryptUnprotectData(&input, None, None, None, None, 0, &mut output) }
        .map_err(|error| format!("chrome_cookie_access_denied: {error}"))?;
    let bytes = if output.pbData.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() }
    };
    if !output.pbData.is_null() {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(output.pbData as *mut core::ffi::c_void)));
        }
    }
    Ok(bytes)
}

#[cfg(windows)]
fn read_cookie_key(profile: &Path, allow_elevated: bool) -> Result<Vec<u8>, String> {
    let user_data = profile.parent().unwrap_or(profile);
    let local_state_path = user_data.join("Local State");
    let local_state: Value = serde_json::from_slice(
        &std::fs::read(&local_state_path).map_err(|_| "chrome_cookie_access_denied".to_string())?,
    )
    .map_err(|_| "chrome_cookie_access_denied".to_string())?;
    let os_crypt = local_state.get("os_crypt");
    let encrypted_key = os_crypt
        .and_then(|value| value.get("encrypted_key"))
        .and_then(Value::as_str);
    let Some(encrypted_key) = encrypted_key else {
        if os_crypt
            .and_then(|value| value.get("app_bound_encrypted_key"))
            .and_then(Value::as_str)
            .is_some()
        {
            return Err(if allow_elevated {
                "chrome_cookie_app_bound_decryption_failed"
            } else {
                "chrome_cookie_elevation_required"
            }
            .to_string());
        }
        return Err("chrome_cookie_protection_unsupported".to_string());
    };
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encrypted_key)
        .map_err(|_| "chrome_cookie_protection_unsupported".to_string())?;
    let payload = decoded
        .strip_prefix(b"DPAPI")
        .ok_or_else(|| "chrome_cookie_protection_unsupported".to_string())?;
    dpapi_unprotect(payload)
}

#[cfg(windows)]
fn read_cookies(
    profile: &Path,
    allow_elevated: bool,
) -> Result<(Vec<CookieRecord>, ChromeCookieStats), String> {
    let source = cookies_path(profile);
    if !source.is_file() {
        return Err("chrome_profile_not_found".to_string());
    }
    let temp = std::env::temp_dir().join(format!(
        "zcode-chrome-cookies-{}-{}.sqlite",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::copy(&source, &temp).map_err(|error| {
        if matches!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
        ) {
            "chrome_profile_locked".to_string()
        } else {
            "chrome_cookie_access_denied".to_string()
        }
    })?;
    let key = read_cookie_key(profile, allow_elevated)?;
    let connection = rusqlite::Connection::open_with_flags(
        &temp,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "chrome_cookie_access_denied".to_string())?;
    let mut statement = connection
        .prepare(
            "SELECT host_key, name, value, encrypted_value, path, expires_utc, is_secure, is_httponly, samesite FROM cookies",
        )
        .map_err(|_| "chrome_cookie_protection_unsupported".to_string())?;
    let mut cookies = Vec::new();
    let mut stats = ChromeCookieStats::default();
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
            ))
        })
        .map_err(|_| "chrome_cookie_protection_unsupported".to_string())?;
    for row in rows.flatten() {
        if cookies.len() >= MAX_COOKIES {
            stats.skipped += 1;
            continue;
        }
        let (domain, name, plain, encrypted, path, expires, secure, http_only, samesite) = row;
        if domain.is_empty() || name.is_empty() {
            stats.skipped += 1;
            continue;
        }
        let value = if !plain.is_empty() {
            plain
        } else if encrypted.starts_with(b"v10") || encrypted.starts_with(b"v11") {
            match decrypt_cookie_value(&key, &encrypted) {
                Ok(value) => value,
                Err(_) => {
                    stats.failed += 1;
                    continue;
                }
            }
        } else if !encrypted.is_empty() {
            match dpapi_unprotect(&encrypted) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                Err(_) => {
                    stats.failed += 1;
                    continue;
                }
            }
        } else {
            stats.skipped += 1;
            continue;
        };
        cookies.push(CookieRecord {
            domain,
            name,
            value,
            path: if path.is_empty() {
                "/".to_string()
            } else {
                path
            },
            secure: secure != 0,
            http_only: http_only != 0,
            expires_unix: chrome_expires_to_unix(expires),
            same_site: same_site_name(samesite),
        });
    }
    let _ = std::fs::remove_file(&temp);
    Ok((cookies, stats))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn read_cookie_key(_profile: &Path, _allow_elevated: bool) -> Result<Vec<u8>, String> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("security")
            .args(["find-generic-password", "-w", "-s", "Chrome Safe Storage"])
            .output()
            .map_err(|_| "chrome_cookie_protection_unsupported".to_string())?;
        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);
            return Err(map_macos_keychain_exit(code)
                .unwrap_or("chrome_cookie_access_denied")
                .to_string());
        }
        let password = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if password.is_empty() {
            return Err("chrome_cookie_access_denied".to_string());
        }
        return Ok(pbkdf2_sha1(password.as_bytes(), b"saltysalt", 1003, 16));
    }
    #[cfg(target_os = "linux")]
    {
        // Chrome's documented fallback when no keyring password is available;
        // matches the Electron implementation (`createCbcDecryptor("peanuts", 1)`).
        Ok(pbkdf2_sha1(b"peanuts", b"saltysalt", 1, 16))
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn read_cookies(
    profile: &Path,
    allow_elevated: bool,
) -> Result<(Vec<CookieRecord>, ChromeCookieStats), String> {
    let _ = allow_elevated;
    let source = cookies_path(profile);
    if !source.is_file() {
        return Err("chrome_profile_not_found".to_string());
    }
    let temp = std::env::temp_dir().join(format!(
        "zcode-chrome-cookies-{}-{}.sqlite",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::copy(&source, &temp).map_err(|error| {
        if matches!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
        ) {
            "chrome_profile_locked".to_string()
        } else {
            "chrome_cookie_access_denied".to_string()
        }
    })?;
    // Chrome keeps recent writes in the WAL; copy it next to the snapshot so
    // SQLite replays them when the temp database is opened.
    for suffix in ["-wal", "-shm"] {
        let mut from = source.clone().into_os_string();
        from.push(suffix);
        let from = PathBuf::from(from);
        if from.is_file() {
            let mut to = temp.clone().into_os_string();
            to.push(suffix);
            let _ = std::fs::copy(&from, PathBuf::from(to));
        }
    }
    let key = read_cookie_key(profile, allow_elevated)?;
    let connection = rusqlite::Connection::open_with_flags(
        &temp,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "chrome_cookie_access_denied".to_string())?;
    let schema_version: i64 = connection
        .query_row("SELECT value FROM meta WHERE key = 'version'", [], |row| {
            row.get::<_, String>(0)
        })
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut statement = connection
        .prepare(
            "SELECT host_key, name, value, encrypted_value, path, expires_utc, is_secure, is_httponly, samesite FROM cookies",
        )
        .map_err(|_| "chrome_cookie_protection_unsupported".to_string())?;
    let mut cookies = Vec::new();
    let mut stats = ChromeCookieStats::default();
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
            ))
        })
        .map_err(|_| "chrome_cookie_protection_unsupported".to_string())?;
    for row in rows.flatten() {
        if cookies.len() >= MAX_COOKIES {
            stats.skipped += 1;
            continue;
        }
        let (domain, name, plain, encrypted, path, expires, secure, http_only, samesite) = row;
        if domain.is_empty() || name.is_empty() {
            stats.skipped += 1;
            continue;
        }
        let value = if !plain.is_empty() {
            plain
        } else if encrypted.starts_with(b"v10") {
            match decrypt_cookie_value_cbc(&key, &encrypted, schema_version) {
                Ok(value) => value,
                Err(_) => {
                    stats.failed += 1;
                    continue;
                }
            }
        } else if !encrypted.is_empty() {
            stats.failed += 1;
            continue;
        } else {
            stats.skipped += 1;
            continue;
        };
        cookies.push(CookieRecord {
            domain,
            name,
            value,
            path: if path.is_empty() {
                "/".to_string()
            } else {
                path
            },
            secure: secure != 0,
            http_only: http_only != 0,
            expires_unix: chrome_expires_to_unix(expires),
            same_site: same_site_name(samesite),
        });
    }
    let _ = std::fs::remove_file(&temp);
    for suffix in ["-wal", "-shm"] {
        let mut path = temp.clone().into_os_string();
        path.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(path));
    }
    Ok((cookies, stats))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn import_cookies(
    window: &WebviewWindow,
    profile: &Path,
    allow_elevated: bool,
) -> Result<(ChromeCookieStats, Vec<String>), String> {
    use tauri::webview::cookie::{time::OffsetDateTime, SameSite};

    let (cookies, mut stats) = read_cookies(profile, allow_elevated)?;
    for cookie in cookies {
        let mut target = tauri::Cookie::new(cookie.name.clone(), cookie.value.clone());
        target.set_domain(cookie.domain.clone());
        target.set_path(cookie.path.clone());
        target.set_secure(cookie.secure);
        target.set_http_only(cookie.http_only);
        if let Some(same_site) = cookie.same_site {
            let same_site = match same_site {
                "None" => Some(SameSite::None),
                "Lax" => Some(SameSite::Lax),
                "Strict" => Some(SameSite::Strict),
                _ => None,
            };
            if let Some(same_site) = same_site {
                target.set_same_site(same_site);
            }
        }
        if let Some(expires) = cookie.expires_unix {
            if let Ok(datetime) = OffsetDateTime::from_unix_timestamp(expires as i64) {
                target.set_expires(datetime);
            }
        }
        match window.set_cookie(target) {
            Ok(()) => stats.imported += 1,
            Err(_) => stats.failed += 1,
        }
    }
    Ok((stats, Vec::new()))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn import_local_storage(_window: &WebviewWindow, profile: &Path) -> ChromeLocalStorageStats {
    let local_storage = profile.join("Local Storage").join("leveldb");
    if !local_storage.is_dir() {
        return ChromeLocalStorageStats::default();
    }
    let mut origins: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&local_storage) {
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            // LevelDB files can be large; the origin scan only needs the keys.
            if !metadata.is_file() || metadata.len() > 32 * 1024 * 1024 {
                continue;
            }
            let Ok(bytes) = std::fs::read(entry.path()) else {
                continue;
            };
            for origin in parse_local_storage_origins(&bytes) {
                if !origins.contains(&origin) {
                    origins.push(origin);
                }
                if origins.len() >= MAX_LOCAL_STORAGE_ORIGINS {
                    break;
                }
            }
        }
    }
    ChromeLocalStorageStats {
        origins_failed: origins.len().max(1) as u64,
        error: Some("chrome_local_storage_import_failed".to_string()),
        ..Default::default()
    }
}

#[cfg(windows)]
fn call_window_cdp(window: &WebviewWindow, method: &str, params: Value) -> Result<Value, String> {
    use std::sync::mpsc;
    use webview2_com::CallDevToolsProtocolMethodCompletedHandler;
    use windows::core::HSTRING;

    let method = method.to_string();
    let (sender, receiver) = mpsc::channel::<Result<String, String>>();
    let sender = std::sync::Arc::new(Mutex::new(Some(sender)));
    let handler_sender = std::sync::Arc::clone(&sender);
    let error_sender = std::sync::Arc::clone(&sender);
    window
        .with_webview(move |platform_webview| {
            let started = (|| -> Result<(), String> {
                let controller = platform_webview.controller();
                let core =
                    unsafe { controller.CoreWebView2() }.map_err(|error| error.to_string())?;
                let handler = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(
                    move |error_code, json| {
                        let result = if error_code.is_ok() {
                            Ok(json)
                        } else {
                            Err("chrome_import_cdp_failed".to_string())
                        };
                        if let Some(sender) = handler_sender
                            .lock()
                            .ok()
                            .and_then(|mut guard| guard.take())
                        {
                            let _ = sender.send(result);
                        }
                        Ok(())
                    },
                ));
                unsafe {
                    core.CallDevToolsProtocolMethod(
                        &HSTRING::from(method),
                        &HSTRING::from(params.to_string()),
                        &handler,
                    )
                }
                .map_err(|error| error.to_string())
            })();
            if let Err(error) = started {
                if let Some(sender) = error_sender.lock().ok().and_then(|mut guard| guard.take()) {
                    let _ = sender.send(Err(error));
                }
            }
        })
        .map_err(|error| error.to_string())?;
    let raw = receiver
        .recv_timeout(CDP_TIMEOUT)
        .map_err(|_| "chrome_import_cdp_timeout".to_string())??;
    serde_json::from_str(&raw).map_err(|_| "chrome_import_cdp_failed".to_string())
}

#[cfg(windows)]
fn import_cookies(
    window: &WebviewWindow,
    profile: &Path,
    allow_elevated: bool,
) -> Result<(ChromeCookieStats, Vec<String>), String> {
    let (cookies, mut stats) = read_cookies(profile, allow_elevated)?;
    for cookie in cookies {
        let mut params = json!({
            "name": cookie.name,
            "value": cookie.value,
            "domain": cookie.domain,
            "path": cookie.path,
            "secure": cookie.secure,
            "httpOnly": cookie.http_only,
        });
        if let Some(expires) = cookie.expires_unix {
            params["expires"] = json!(expires);
        }
        if let Some(same_site) = cookie.same_site {
            params["sameSite"] = json!(same_site);
        }
        match call_window_cdp(window, "Network.setCookie", params) {
            Ok(result)
                if result
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(false) =>
            {
                stats.imported += 1;
            }
            Ok(_) => stats.failed += 1,
            Err(_) => stats.failed += 1,
        }
    }
    Ok((stats, Vec::new()))
}

#[cfg(windows)]
fn copy_directory(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_directory(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn import_local_storage(window: &WebviewWindow, profile: &Path) -> ChromeLocalStorageStats {
    let mut stats = ChromeLocalStorageStats::default();
    let local_storage = profile.join("Local Storage").join("leveldb");
    if !local_storage.is_dir() {
        return stats;
    }
    let mut origins = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&local_storage) {
        for entry in entries.flatten() {
            if !entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
            {
                continue;
            }
            if let Ok(bytes) = std::fs::read(entry.path()) {
                for origin in parse_local_storage_origins(&bytes) {
                    if !origins.iter().any(|item| item == &origin) {
                        origins.push(origin);
                    }
                }
            }
        }
    }
    if origins.is_empty() {
        return stats;
    }
    origins.truncate(MAX_LOCAL_STORAGE_ORIGINS);
    let Ok(executable) = nc_code_cdp::discover_installed_browser() else {
        stats.error = Some("chrome_executable_not_found".to_string());
        return stats;
    };
    let temp_root = std::env::temp_dir().join(format!(
        "zcode-chrome-local-storage-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    ));
    let temp_profile = temp_root.join("Default");
    if copy_directory(
        &profile.join("Local Storage"),
        &temp_profile.join("Local Storage"),
    )
    .is_err()
    {
        stats.error = Some("chrome_local_storage_import_failed".to_string());
        let _ = std::fs::remove_dir_all(&temp_root);
        return stats;
    }
    let launch = nc_code_cdp::ManagedBrowser::launch(
        &executable,
        &nc_code_cdp::LaunchOptions::default(),
        Some(temp_root.clone()),
    );
    let Ok(mut browser) = launch else {
        stats.error = Some("chrome_executable_not_found".to_string());
        let _ = std::fs::remove_dir_all(&temp_root);
        return stats;
    };
    let connection = tauri::async_runtime::block_on(nc_code_cdp::CdpConnection::connect(
        &browser.browser_ws_url,
        CDP_TIMEOUT,
    ));
    let Ok(connection) = connection else {
        browser.shutdown();
        let _ = std::fs::remove_dir_all(&temp_root);
        stats.error = Some("chrome_local_storage_import_failed".to_string());
        return stats;
    };
    let target = tauri::async_runtime::block_on(connection.send(
        "Target.createTarget",
        json!({ "url": "about:blank" }),
        None,
        CDP_TIMEOUT,
    ));
    let target_id = target.ok().and_then(|value| {
        value
            .get("targetId")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let Some(target_id) = target_id else {
        connection_close(&connection);
        browser.shutdown();
        let _ = std::fs::remove_dir_all(&temp_root);
        stats.error = Some("chrome_local_storage_import_failed".to_string());
        return stats;
    };
    let attach = tauri::async_runtime::block_on(connection.send(
        "Target.attachToTarget",
        json!({ "targetId": target_id, "flatten": true }),
        None,
        CDP_TIMEOUT,
    ));
    let session_id = attach.ok().and_then(|value| {
        value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let Some(session_id) = session_id else {
        connection_close(&connection);
        browser.shutdown();
        let _ = std::fs::remove_dir_all(&temp_root);
        stats.error = Some("chrome_local_storage_import_failed".to_string());
        return stats;
    };
    for origin in origins {
        let storage = json!({ "securityOrigin": origin, "isLocalStorage": true });
        let items = tauri::async_runtime::block_on(connection.send(
            "DOMStorage.getDOMStorageItems",
            json!({ "storageId": storage }),
            Some(&session_id),
            CDP_TIMEOUT,
        ));
        let Ok(items) = items else {
            stats.origins_failed += 1;
            continue;
        };
        let mut imported_for_origin = 0u64;
        if let Some(entries) = items.get("entries").and_then(Value::as_array) {
            for entry in entries {
                let Some(pair) = entry.as_array() else {
                    continue;
                };
                if pair.len() != 2 {
                    continue;
                }
                let (Some(key), Some(value)) = (pair[0].as_str(), pair[1].as_str()) else {
                    continue;
                };
                let result = call_window_cdp(
                    window,
                    "DOMStorage.setDOMStorageItem",
                    json!({ "storageId": storage, "key": key, "value": value }),
                );
                if result.is_ok() {
                    imported_for_origin += 1;
                    stats.entries_imported += 1;
                }
            }
        }
        if imported_for_origin > 0 {
            stats.origins_imported += 1;
        } else {
            stats.origins_skipped += 1;
        }
    }
    connection_close(&connection);
    browser.shutdown();
    let _ = std::fs::remove_dir_all(&temp_root);
    stats
}

#[cfg(windows)]
fn connection_close(connection: &nc_code_cdp::CdpConnection) {
    tauri::async_runtime::block_on(connection.close());
}

/// Import Chrome cookies and Local Storage into the app's WebView2 profile.
/// Windows only; other platforms return `chrome_import_not_supported`.
#[tauri::command]
pub async fn import_chrome_browser_data(
    window: WebviewWindow,
    options: Option<ChromeBrowserDataImportOptions>,
) -> Result<ChromeBrowserDataImportResult, String> {
    let allow_elevated = options
        .map(|value| value.allow_elevated_chrome_decryption)
        .unwrap_or(false);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let result =
            tauri::async_runtime::spawn_blocking(move || {
                let Some(user_data) = chrome_user_data_dir() else {
                    return ChromeBrowserDataImportResult::failure("chrome_profile_not_found");
                };
                let Some(profile) = newest_profile(&user_data) else {
                    return ChromeBrowserDataImportResult::failure("chrome_profile_not_found");
                };
                let mut result = ChromeBrowserDataImportResult::default();
                let mut issues = Vec::new();
                match import_cookies(&window, &profile, allow_elevated) {
                    Ok((stats, cookie_issues)) => {
                        result.cookies = stats;
                        issues.extend(cookie_issues);
                    }
                    Err(error) => {
                        result.error = Some(error);
                        return result;
                    }
                }
                let local_storage = import_local_storage(&window, &profile);
                if let Some(error) = &local_storage.error {
                    issues.push(error.clone());
                }
                result.local_storage = local_storage;
                let database_empty = result.cookies.imported == 0
                    && result.cookies.skipped == 0
                    && result.cookies.failed == 0;
                result.success = database_empty
                    || result.cookies.imported > 0
                    || result.local_storage.origins_imported > 0;
                if !result.success {
                    result.error =
                        Some(result.local_storage.error.clone().unwrap_or_else(|| {
                            "chrome_browser_data_import_unavailable".to_string()
                        }));
                }
                if !issues.is_empty() {
                    result.issues = Some(issues);
                }
                result
            })
            .await
            .map_err(|error| error.to_string())?;
        return Ok(result);
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = (window, allow_elevated);
        return Ok(ChromeBrowserDataImportResult::failure(
            "chrome_import_not_supported",
        ));
    }
    #[cfg(windows)]
    {
        let result = tauri::async_runtime::spawn_blocking(move || {
            let Some(user_data) = chrome_user_data_dir() else {
                return ChromeBrowserDataImportResult::failure("chrome_profile_not_found");
            };
            let Some(profile) = newest_profile(&user_data) else {
                return ChromeBrowserDataImportResult::failure("chrome_profile_not_found");
            };
            let mut result = ChromeBrowserDataImportResult::default();
            match import_cookies(&window, &profile, allow_elevated) {
                Ok((stats, issues)) => {
                    result.cookies = stats;
                    if !issues.is_empty() {
                        result.issues = Some(issues);
                    }
                }
                Err(error) => {
                    result.error = Some(error);
                    return result;
                }
            }
            let local_storage = import_local_storage(&window, &profile);
            result.local_storage = local_storage;
            result.success = result.error.is_none() && result.local_storage.error.is_none();
            result
        })
        .await
        .map_err(|error| error.to_string())?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_local_storage_meta_origins() {
        let bytes = b"\x00META:https://example.com\x00META:http://localhost:3000\x00META:https://example.com";
        let origins = parse_local_storage_origins(bytes);
        assert_eq!(
            origins,
            vec![
                "https://example.com".to_string(),
                "http://localhost:3000".to_string()
            ]
        );
    }

    #[test]
    fn converts_chrome_expiry_to_unix_seconds() {
        // 2024-01-01T00:00:00Z in Chrome microseconds since 1601.
        assert_eq!(
            chrome_expires_to_unix(13_348_540_800_000_000),
            Some(1_704_067_200.0)
        );
        assert_eq!(chrome_expires_to_unix(0), None);
    }

    #[test]
    fn maps_chrome_same_site_values() {
        assert_eq!(same_site_name(0), Some("None"));
        assert_eq!(same_site_name(1), Some("Lax"));
        assert_eq!(same_site_name(2), Some("Strict"));
        assert_eq!(same_site_name(-1), None);
    }

    #[cfg(windows)]
    #[test]
    fn decrypts_v10_aes_gcm_cookie_value() {
        use aes_gcm::aead::{Aead, KeyInit};
        use aes_gcm::{Aes256Gcm, Nonce};
        let key = [7u8; 32];
        let nonce = [9u8; 12];
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce), b"cookie-value".as_ref())
            .unwrap();
        let mut encrypted = b"v10".to_vec();
        encrypted.extend_from_slice(&nonce);
        encrypted.extend_from_slice(&ciphertext);
        assert_eq!(
            decrypt_cookie_value(&key, &encrypted).unwrap(),
            "cookie-value"
        );
    }

    #[cfg(windows)]
    #[test]
    fn rejects_truncated_encrypted_cookie() {
        assert!(decrypt_cookie_value(&[7u8; 32], b"v10short").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn reads_dpapi_protected_cookie_database() {
        use aes_gcm::aead::{Aead, KeyInit};
        use aes_gcm::{Aes256Gcm, Nonce};
        use windows::Win32::Foundation::{LocalFree, HLOCAL};
        use windows::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};

        fn dpapi_protect(data: &[u8]) -> Vec<u8> {
            let input = CRYPT_INTEGER_BLOB {
                cbData: data.len() as u32,
                pbData: data.as_ptr() as *mut u8,
            };
            let mut output = CRYPT_INTEGER_BLOB::default();
            unsafe { CryptProtectData(&input, None, None, None, None, 0, &mut output) }
                .expect("dpapi protect");
            let bytes = unsafe {
                std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec()
            };
            unsafe {
                let _ = LocalFree(Some(HLOCAL(output.pbData as *mut core::ffi::c_void)));
            }
            bytes
        }

        let root = std::env::temp_dir().join(format!(
            "zcode-chrome-import-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let user_data = root.join("User Data");
        let profile = user_data.join("Default");
        std::fs::create_dir_all(profile.join("Network")).unwrap();
        let key = [11u8; 32];
        let protected = dpapi_protect(&key);
        let mut encoded = b"DPAPI".to_vec();
        encoded.extend_from_slice(&protected);
        let local_state = json!({
            "os_crypt": {
                "encrypted_key": base64::engine::general_purpose::STANDARD.encode(encoded),
            }
        });
        std::fs::write(
            user_data.join("Local State"),
            serde_json::to_vec(&local_state).unwrap(),
        )
        .unwrap();
        let nonce = [5u8; 12];
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce), b"session-token".as_ref())
            .unwrap();
        let mut encrypted = b"v10".to_vec();
        encrypted.extend_from_slice(&nonce);
        encrypted.extend_from_slice(&ciphertext);
        let connection =
            rusqlite::Connection::open(profile.join("Network").join("Cookies")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE cookies (
                    host_key TEXT, name TEXT, value TEXT, encrypted_value BLOB,
                    path TEXT, expires_utc INTEGER, is_secure INTEGER,
                    is_httponly INTEGER, samesite INTEGER
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO cookies VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    ".example.com",
                    "sid",
                    "",
                    encrypted,
                    "/",
                    13_348_540_800_000_000i64,
                    1i64,
                    1i64,
                    1i64
                ],
            )
            .unwrap();
        drop(connection);
        let (cookies, stats) = read_cookies(&profile, false).expect("read cookies");
        assert_eq!(stats.failed, 0);
        assert_eq!(cookies.len(), 1);
        assert_eq!(cookies[0].name, "sid");
        assert_eq!(cookies[0].value, "session-token");
        assert!(cookies[0].secure);
        assert!(cookies[0].http_only);
        assert_eq!(cookies[0].same_site, Some("Lax"));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    #[test]
    fn pbkdf2_matches_rfc6070_vectors() {
        assert_eq!(
            pbkdf2_sha1(b"password", b"salt", 1, 20),
            hex("0c60c80f961f0e71f3a9b524af6012062fe037a6")
        );
        assert_eq!(
            pbkdf2_sha1(b"password", b"salt", 2, 20),
            hex("ea6c014dc72d6f8ccd1ed92ace1d41f0d8de8957")
        );
        assert_eq!(
            pbkdf2_sha1(b"password", b"salt", 4096, 20),
            hex("4b007901b765489abead49d926f721d065a429c1")
        );
    }

    #[test]
    fn aes_cbc_matches_openssl_vector() {
        // openssl enc -aes-128-cbc -K 11..11 -iv 20..20 for "chrome-cookie-value"
        let key = [0x11u8; 16];
        let iv = [0x20u8; 16];
        let ciphertext = hex("59f2cb6621478a93d2a02d8e695c60e042e8a43b1945e3b376d6e7ce6984d0b5");
        let plaintext = aes_128_cbc_decrypt(&key, &iv, &ciphertext).unwrap();
        assert_eq!(plaintext, b"chrome-cookie-value");
    }

    #[test]
    fn chrome_v10_cbc_roundtrip_strips_host_digest_for_schema_24() {
        use aes::Aes128;
        use cbc::cipher::{block_padding::Pkcs7, BlockModeEncrypt, KeyIvInit};

        type Aes128CbcEnc = cbc::Encryptor<Aes128>;
        let key = [0x11u8; 16];
        let iv = [0x20u8; 16];
        let encrypt = |plaintext: &[u8]| {
            let mut encrypted = b"v10".to_vec();
            encrypted.extend_from_slice(
                &Aes128CbcEnc::new_from_slices(&key, &iv)
                    .unwrap()
                    .encrypt_padded_vec::<Pkcs7>(plaintext),
            );
            encrypted
        };

        let plain = encrypt(b"cookie-value");
        assert_eq!(
            decrypt_cookie_value_cbc(&key, &plain, 23).unwrap(),
            "cookie-value"
        );

        let mut with_digest = vec![0u8; 32];
        with_digest.extend_from_slice(b"cookie-value");
        assert_eq!(
            decrypt_cookie_value_cbc(&key, &encrypt(&with_digest), 24).unwrap(),
            "cookie-value"
        );
        // Schema 23 keeps the digest bytes as part of the value.
        assert_ne!(
            decrypt_cookie_value_cbc(&key, &encrypt(&with_digest), 23).unwrap(),
            "cookie-value"
        );
    }

    #[test]
    fn maps_macos_keychain_denials() {
        for code in [36, 51, 128] {
            assert_eq!(
                map_macos_keychain_exit(code),
                Some("chrome_cookie_access_denied")
            );
        }
        assert_eq!(map_macos_keychain_exit(0), None);
        assert_eq!(map_macos_keychain_exit(1), None);
    }
}
