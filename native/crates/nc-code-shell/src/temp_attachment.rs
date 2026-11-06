//! Temporary text attachments for the Tauri shell.
//!
//! Mirrors the Electron `tempTextAttachment` helper: long pasted text becomes a
//! real file under `{dataBaseDir}/.zcode/tmp/paste-attachments/<YYYY-MM-DD>/`
//! so it can be referenced by path instead of being inlined into the prompt.
//!
//! The write is bounded (32 MiB UTF-8) and filename input is sanitized: path
//! separators, Windows-reserved characters, control characters and trailing
//! dots/spaces are replaced or trimmed before the `.txt` suffix is applied.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const MAX_TEXT_BYTES: usize = 32 * 1024 * 1024;
const MAX_FILENAME_CHARS: usize = 120;
const MAX_WRITE_ATTEMPTS: u8 = 5;
const DEFAULT_BASENAME: &str = "pasted-text.txt";
const MIME_TYPE: &str = "text/plain";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTempTextAttachmentRequest {
    pub text: String,
    #[serde(default)]
    pub filename: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CreateTempTextAttachmentResult {
    pub filename: String,
    pub local_path: String,
    pub mime_type: String,
    pub size_bytes: usize,
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    value.chars().take(max_chars).collect()
}

fn sanitize_base_name(raw: Option<&str>) -> String {
    let trimmed = raw.map(str::trim).unwrap_or_default();
    let candidate = if trimmed.is_empty() {
        DEFAULT_BASENAME
    } else {
        trimmed
    };
    let mut sanitized = String::with_capacity(candidate.len());
    for character in candidate.chars() {
        // Windows forbids control characters and these punctuation marks in
        // file names; separators must never reach the path join.
        if character.is_control()
            || matches!(
                character,
                '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
            )
        {
            sanitized.push('-');
        } else {
            sanitized.push(character);
        }
    }
    // Windows also strips trailing dots and spaces from file names, which can
    // silently change the path; normalize them away before writing.
    while sanitized.ends_with(['.', ' ']) {
        sanitized.pop();
    }
    if sanitized.is_empty() {
        sanitized = DEFAULT_BASENAME.to_string();
    }
    // Keep the `.txt` suffix inside the length budget: truncate the stem, then
    // append the suffix so the extension is never cut off.
    let stem = if sanitized.to_ascii_lowercase().ends_with(".txt") {
        sanitized[..sanitized.len() - 4].to_string()
    } else {
        sanitized
    };
    let stem = if stem.is_empty() {
        DEFAULT_BASENAME.trim_end_matches(".txt").to_string()
    } else {
        stem
    };
    let stem = truncate_chars(&stem, MAX_FILENAME_CHARS - 4);
    format!("{stem}.txt")
}

fn filename_with_suffix(base: &str, suffix: &str) -> String {
    match base.rfind('.') {
        Some(index) if index > 0 => {
            format!("{}-{}{}", &base[..index], suffix, &base[index..])
        }
        _ => format!("{base}-{suffix}.txt"),
    }
}

fn validate_text(text: &str, max_bytes: usize) -> Result<(), String> {
    if text.is_empty() {
        return Err("temporary_text_attachment_empty".to_string());
    }
    if text.len() > max_bytes {
        return Err("temporary_text_attachment_too_large".to_string());
    }
    Ok(())
}

fn date_directory(now: chrono::DateTime<chrono::Local>) -> String {
    now.format("%Y-%m-%d").to_string()
}

fn write_unique(directory: &Path, base: &str, text: &str) -> Result<(String, PathBuf), String> {
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    for _ in 0..MAX_WRITE_ATTEMPTS {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let suffix = &suffix[..8];
        let filename = filename_with_suffix(base, suffix);
        let path = directory.join(&filename);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(text.as_bytes())
                    .map_err(|error| error.to_string())?;
                return Ok((filename, path));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("temporary_text_attachment_collision".to_string())
}

fn create_in(
    root: &Path,
    request: &CreateTempTextAttachmentRequest,
    max_bytes: usize,
) -> Result<CreateTempTextAttachmentResult, String> {
    validate_text(&request.text, max_bytes)?;
    let base = sanitize_base_name(request.filename.as_deref());
    let directory = root.join(date_directory(chrono::Local::now()));
    let (filename, local_path) = write_unique(&directory, &base, &request.text)?;
    Ok(CreateTempTextAttachmentResult {
        filename,
        local_path: local_path.to_string_lossy().to_string(),
        mime_type: MIME_TYPE.to_string(),
        size_bytes: request.text.len(),
    })
}

/// `IPlatformService.createTempTextAttachment`.
#[tauri::command]
pub fn create_temp_text_attachment(
    request: CreateTempTextAttachmentRequest,
) -> Result<CreateTempTextAttachmentResult, String> {
    let paths = nc_code_service_core::AppPaths::from_env();
    let root = paths
        .nc_code_data_root()
        .join("tmp")
        .join("paste-attachments");
    create_in(&root, &request, MAX_TEXT_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "zcode-temp-attachment-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn request(text: &str, filename: Option<&str>) -> CreateTempTextAttachmentRequest {
        CreateTempTextAttachmentRequest {
            text: text.to_string(),
            filename: filename.map(str::to_string),
        }
    }

    #[test]
    fn sanitizes_path_separators_and_reserved_characters() {
        assert_eq!(
            sanitize_base_name(Some(r#"a\0b/c:d*e?f"g<h>i|j"#)),
            "a-0b-c-d-e-f-g-h-i-j.txt"
        );
        assert_eq!(sanitize_base_name(Some("a\u{0}b")), "a-b.txt");
        assert_eq!(sanitize_base_name(Some("notes.md")), "notes.md.txt");
        assert_eq!(sanitize_base_name(Some("notes.txt")), "notes.txt");
        assert_eq!(sanitize_base_name(None), DEFAULT_BASENAME);
        assert_eq!(sanitize_base_name(Some("   ")), DEFAULT_BASENAME);
        assert_eq!(sanitize_base_name(Some("trailing. ")), "trailing.txt");
    }

    #[test]
    fn caps_filename_length_and_keeps_txt_suffix() {
        let long = "a".repeat(500);
        let sanitized = sanitize_base_name(Some(&long));
        assert!(sanitized.chars().count() <= MAX_FILENAME_CHARS);
        assert!(sanitized.ends_with(".txt"));
    }

    #[test]
    fn inserts_suffix_before_extension() {
        assert_eq!(
            filename_with_suffix("notes.txt", "deadbeef"),
            "notes-deadbeef.txt"
        );
        assert_eq!(
            filename_with_suffix("notes", "deadbeef"),
            "notes-deadbeef.txt"
        );
    }

    #[test]
    fn rejects_empty_and_oversized_text() {
        let root = temp_root("limits");
        assert_eq!(
            create_in(&root, &request("", None), 8).unwrap_err(),
            "temporary_text_attachment_empty"
        );
        assert_eq!(
            create_in(&root, &request("0123456789", None), 8).unwrap_err(),
            "temporary_text_attachment_too_large"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn writes_text_with_result_metadata() {
        let root = temp_root("write");
        let result = create_in(&root, &request("hello 世界", Some("paste.txt")), 1024).unwrap();
        assert_eq!(result.mime_type, MIME_TYPE);
        assert_eq!(result.size_bytes, "hello 世界".len());
        assert!(result.filename.starts_with("paste-"));
        assert!(result.filename.ends_with(".txt"));
        let contents = std::fs::read_to_string(&result.local_path).unwrap();
        assert_eq!(contents, "hello 世界");
        let path = Path::new(&result.local_path);
        assert!(path.starts_with(&root));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn repeated_writes_do_not_collide() {
        let root = temp_root("unique");
        let first = create_in(&root, &request("a", Some("same.txt")), 16).unwrap();
        let second = create_in(&root, &request("b", Some("same.txt")), 16).unwrap();
        assert_ne!(first.filename, second.filename);
        assert_eq!(std::fs::read_to_string(&first.local_path).unwrap(), "a");
        assert_eq!(std::fs::read_to_string(&second.local_path).unwrap(), "b");
        let _ = std::fs::remove_dir_all(&root);
    }
}
