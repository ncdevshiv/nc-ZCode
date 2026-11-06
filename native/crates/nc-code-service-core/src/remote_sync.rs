//! Remote sync write-access preflight (port of `packages/services/src/remote-sync/remoteSyncWriteAccess.ts`).

use std::path::Path;

use serde::Serialize;
use tokio::fs;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSyncWriteAccessResult {
    pub ok: bool,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn marker_name() -> String {
    format!(
        ".zcode-sync-preflight-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

pub async fn check_remote_sync_directory_write_access(
    directory_path: &Path,
) -> RemoteSyncWriteAccessResult {
    let path_str = directory_path.to_string_lossy().to_string();
    let marker = directory_path.join(marker_name());
    let result = async {
        fs::create_dir_all(directory_path).await?;
        // create_new semantics
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
            .await?
            .sync_all()
            .await?;
        fs::write(&marker, "ok").await?;
        fs::remove_file(&marker).await?;
        Ok::<(), std::io::Error>(())
    }
    .await;

    match result {
        Ok(()) => RemoteSyncWriteAccessResult {
            ok: true,
            path: path_str,
            error: None,
        },
        Err(e) => {
            let _ = fs::remove_file(&marker).await;
            RemoteSyncWriteAccessResult {
                ok: false,
                path: path_str,
                error: Some(e.to_string()),
            }
        }
    }
}

pub async fn check_remote_sync_directories_write_access(
    directory_paths: &[impl AsRef<Path>],
) -> RemoteSyncWriteAccessResult {
    for directory_path in directory_paths {
        let result = check_remote_sync_directory_write_access(directory_path.as_ref()).await;
        if !result.ok {
            return result;
        }
    }
    let joined = directory_paths
        .iter()
        .map(|p| p.as_ref().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    RemoteSyncWriteAccessResult {
        ok: true,
        path: joined,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn write_access_ok() {
        let tmp = TempDir::new().unwrap();
        let r = check_remote_sync_directory_write_access(tmp.path()).await;
        assert!(r.ok);
    }

    #[tokio::test]
    async fn write_access_multiple() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let r = check_remote_sync_directories_write_access(&[a.path(), b.path()]).await;
        assert!(r.ok);
    }
}
