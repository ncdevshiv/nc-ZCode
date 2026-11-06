//! Workspace-safe write path for renderer-produced browser recordings.
//!
//! The renderer captures the child webview through CDP and encodes WebM with
//! `MediaRecorder`; this module is the only place that persists the bytes. The
//! relative path is validated segment by segment, the parent is canonicalized
//! and must stay inside the workspace, and the payload must be a real WebM
//! (EBML magic) so a failed capture can never be reported as an artifact.

use std::path::{Component, Path, PathBuf};

const MAX_RECORDING_BYTES: usize = 256 * 1024 * 1024;
const WEBM_EBML_MAGIC: [u8; 4] = [0x1a, 0x45, 0xdf, 0xa3];

pub fn resolve_recording_path(workspace: &Path, relative: &str) -> Result<PathBuf, String> {
    let relative = relative.trim();
    if relative.is_empty() {
        return Err("invalid_recording_path".to_string());
    }
    let candidate = Path::new(relative);
    if candidate.is_absolute() {
        return Err("recording_path_not_allowed".to_string());
    }
    for component in candidate.components() {
        match component {
            Component::Normal(_) => {}
            _ => return Err("recording_path_not_allowed".to_string()),
        }
    }
    if !relative.to_ascii_lowercase().ends_with(".webm") {
        return Err("recording_path_not_webm".to_string());
    }
    let workspace = workspace
        .canonicalize()
        .map_err(|_| "workspace_not_found".to_string())?;
    let target = workspace.join(candidate);
    let parent = target
        .parent()
        .ok_or_else(|| "invalid_recording_path".to_string())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let parent = parent.canonicalize().map_err(|error| error.to_string())?;
    if !parent.starts_with(&workspace) {
        return Err("recording_path_not_allowed".to_string());
    }
    Ok(target)
}

pub fn write_recording(workspace: &Path, relative: &str, data: &[u8]) -> Result<PathBuf, String> {
    if data.is_empty() {
        return Err("recording_empty".to_string());
    }
    if data.len() > MAX_RECORDING_BYTES {
        return Err("recording_too_large".to_string());
    }
    if !data.starts_with(&WEBM_EBML_MAGIC) {
        return Err("recording_not_webm".to_string());
    }
    let target = resolve_recording_path(workspace, relative)?;
    if target
        .symlink_metadata()
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err("recording_path_not_allowed".to_string());
    }
    let temp = target.with_extension("webm.part");
    std::fs::write(&temp, data).map_err(|error| error.to_string())?;
    if target.exists() {
        std::fs::remove_file(&target).map_err(|error| error.to_string())?;
    }
    std::fs::rename(&temp, &target).map_err(|error| error.to_string())?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "zcode-recording-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&root).expect("workspace");
        root
    }

    fn webm_bytes() -> Vec<u8> {
        let mut bytes = WEBM_EBML_MAGIC.to_vec();
        bytes.extend_from_slice(b"test-payload");
        bytes
    }

    #[test]
    fn writes_nested_webm_inside_the_workspace() {
        let root = temp_workspace();
        let target =
            write_recording(&root, "browser-recordings/job-1.webm", &webm_bytes()).expect("write");
        assert_eq!(std::fs::read(&target).expect("read"), webm_bytes());
        assert!(target.starts_with(root.canonicalize().expect("canonical")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_escape_absolute_and_non_webm_paths() {
        let root = temp_workspace();
        for relative in [
            "../escape.webm",
            "/abs/escape.webm",
            "nested/../../escape.webm",
        ] {
            assert_eq!(
                resolve_recording_path(&root, relative).unwrap_err(),
                "recording_path_not_allowed"
            );
        }
        assert_eq!(
            resolve_recording_path(&root, "recording.mp4").unwrap_err(),
            "recording_path_not_webm"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_empty_and_non_webm_payloads() {
        let root = temp_workspace();
        assert_eq!(
            write_recording(&root, "job.webm", &[]).unwrap_err(),
            "recording_empty"
        );
        assert_eq!(
            write_recording(&root, "job.webm", b"not-webm").unwrap_err(),
            "recording_not_webm"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn overwrites_an_existing_recording() {
        let root = temp_workspace();
        let first = webm_bytes();
        write_recording(&root, "job.webm", &first).expect("first write");
        let mut second = webm_bytes();
        second.extend_from_slice(b"second");
        let target = write_recording(&root, "job.webm", &second).expect("second write");
        assert_eq!(std::fs::read(target).expect("read"), second);
        let _ = std::fs::remove_dir_all(root);
    }
}
