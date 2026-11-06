//! Atomic text writes with Windows rename retries (mirror of TS `atomicWriteText`).
//!
//! Write to `{file}.{pid}.{seq}.tmp` in the same directory, then rename over the
//! target; retry transient sharing violations (EPERM/EACCES/EBUSY) with the same
//! backoff schedule the TS helper uses. Readers never observe a half-written file.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use tokio::fs;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

const RENAME_RETRY_DELAYS_MS: [u64; 7] = [50, 100, 200, 400, 800, 1600, 3200];
const STALE_TEMP_MS: u64 = 60_000;

pub async fn atomic_write_text(path: &Path, contents: &str) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "atomic write target has no parent directory",
        )
    })?;
    fs::create_dir_all(parent).await?;
    cleanup_stale_temps(path).await;

    let temp = temp_path(path);
    fs::write(&temp, contents).await?;
    match rename_with_retry(&temp, path).await {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temp).await;
            Err(error)
        }
    }
}

pub async fn read_text_if_exists(path: &Path) -> std::io::Result<Option<String>> {
    match fs::read_to_string(path).await {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(".{name}.{}.{sequence}.tmp", std::process::id()))
}

async fn rename_with_retry(temp: &Path, target: &Path) -> std::io::Result<()> {
    let mut attempt = 0usize;
    loop {
        match fs::rename(temp, target).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                let Some(delay) = RENAME_RETRY_DELAYS_MS.get(attempt) else {
                    return Err(error);
                };
                if !is_retryable(&error) {
                    return Err(error);
                }
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(*delay)).await;
            }
        }
    }
}

fn is_retryable(error: &std::io::Error) -> bool {
    if matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
    ) {
        return true;
    }
    matches!(error.raw_os_error(), Some(5) | Some(32) | Some(33))
}

async fn cleanup_stale_temps(path: &Path) {
    let Some(parent) = path.parent() else {
        return;
    };
    let Some(name) = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return;
    };
    let Ok(mut entries) = fs::read_dir(parent).await else {
        return;
    };
    let now = SystemTime::now();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if !file_name.starts_with(&format!(".{name}.")) || !file_name.ends_with(".tmp") {
            continue;
        }
        let Ok(metadata) = entry.metadata().await else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let stale = now
            .duration_since(modified)
            .map(|age| age.as_millis() as u64 > STALE_TEMP_MS)
            .unwrap_or(false);
        if stale {
            let _ = fs::remove_file(entry.path()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zcode-atomic-{}-{tag}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create dir");
        dir
    }

    #[tokio::test]
    async fn writes_replace_existing_content_without_leaving_temp_files() {
        let dir = temp_dir("write");
        let target = dir.join("setting.json");
        atomic_write_text(&target, "{\"a\":1}")
            .await
            .expect("first");
        atomic_write_text(&target, "{\"a\":2}")
            .await
            .expect("second");
        assert_eq!(
            tokio::fs::read_to_string(&target).await.expect("read"),
            "{\"a\":2}"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("entries")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn missing_file_reads_as_none() {
        let dir = temp_dir("missing");
        assert_eq!(
            read_text_if_exists(&dir.join("nope.json"))
                .await
                .expect("read"),
            None
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
