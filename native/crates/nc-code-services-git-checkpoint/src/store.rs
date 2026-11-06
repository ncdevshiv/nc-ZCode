//! `GitCheckpointStore` (gitCheckpointStore.ts): `{appConfigDir}/checkpoints/{workspaceHash}/{id}.json`.
//!
//! The three disciplines shared with TS: (1) a write lands in a temporary file first and is then renamed (atomic); (2) write operations on the same manifest file
//! are serialized per file (TS uses a promise chain, here one lock per file); (3) anything read that does not match
//! the `GitCheckpointMeta` shape is treated as nonexistent (the `isGitCheckpointMeta` type guard),
//! so a corrupted file is never taken for a usable checkpoint.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nc_code_service_core::{AppPaths, ServiceError};
use serde_json::Value;
use tokio::sync::Mutex;

pub struct CheckpointStore {
    paths: AppPaths,
    /// One lock per manifest file: concurrent save/load/delete are serialized per file.
    locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

impl CheckpointStore {
    pub fn new(paths: AppPaths) -> Self {
        Self {
            paths,
            locks: Mutex::new(HashMap::new()),
        }
    }

    fn dir_for(&self, workspace_path: &str) -> PathBuf {
        // Same algorithm as TS getWorkspaceHash(workspacePath) (AppPaths::workspace_hash: sha256[..12]).
        self.paths
            .app_config_dir()
            .join("checkpoints")
            .join(self.paths.workspace_hash(workspace_path, None))
    }

    fn file_for(&self, workspace_path: &str, checkpoint_id: &str) -> PathBuf {
        self.dir_for(workspace_path)
            .join(format!("{checkpoint_id}.json"))
    }

    async fn lock_for(&self, file_path: &Path) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().await;
        Arc::clone(locks.entry(file_path.to_path_buf()).or_default())
    }

    /// `save` + `writeManifestAtomic`（gitCheckpointStore.ts:71-86）。
    pub async fn save(&self, meta: &Value) -> Result<(), ServiceError> {
        let Some(workspace_path) = meta.get("workspacePath").and_then(Value::as_str) else {
            return Err(ServiceError::invalid_params(
                "checkpoint manifest requires workspacePath",
            ));
        };
        let Some(checkpoint_id) = meta.get("checkpointId").and_then(Value::as_str) else {
            return Err(ServiceError::invalid_params(
                "checkpoint manifest requires checkpointId",
            ));
        };
        let file_path = self.file_for(workspace_path, checkpoint_id);
        let lock = self.lock_for(&file_path).await;
        let _guard = lock.lock().await;
        let dir = self.dir_for(workspace_path);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|error| ServiceError::new(format!("checkpoint dir create failed: {error}")))?;
        let temp_path = dir.join(format!(
            "{checkpoint_id}.{}.{}.tmp",
            std::process::id(),
            now_nanos_tag()
        ));
        let body = serde_json::to_string_pretty(meta)
            .map_err(|error| ServiceError::new(format!("checkpoint manifest encode: {error}")))?;
        tokio::fs::write(&temp_path, format!("{body}\n"))
            .await
            .map_err(|error| ServiceError::new(format!("checkpoint manifest write: {error}")))?;
        tokio::fs::rename(&temp_path, &file_path)
            .await
            .map_err(|error| ServiceError::new(format!("checkpoint manifest rename: {error}")))?;
        Ok(())
    }

    /// `load` (:88-101): a read failure/shape mismatch → `None` (TS catch → null).
    pub async fn load(&self, workspace_path: &str, checkpoint_id: &str) -> Option<Value> {
        let file_path = self.file_for(workspace_path, checkpoint_id);
        let lock = self.lock_for(&file_path).await;
        let _guard = lock.lock().await;
        let text = tokio::fs::read_to_string(&file_path).await.ok()?;
        let parsed: Value = serde_json::from_str(&text).ok()?;
        is_checkpoint_meta(&parsed).then_some(parsed)
    }

    /// `delete` (:103-111): force semantics (a missing file still counts as success).
    pub async fn delete(
        &self,
        workspace_path: &str,
        checkpoint_id: &str,
    ) -> Result<(), ServiceError> {
        let file_path = self.file_for(workspace_path, checkpoint_id);
        let lock = self.lock_for(&file_path).await;
        let _guard = lock.lock().await;
        match tokio::fs::remove_file(&file_path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(ServiceError::new(format!(
                "checkpoint manifest delete failed: {error}"
            ))),
        }
    }
}

/// `isGitCheckpointMeta` (gitCheckpointStore.ts:7-21): a per-field shape guard,
/// where `scope` must be "workspace" (this implementation only handles the workspace scope).
fn is_checkpoint_meta(value: &Value) -> bool {
    let object = match value.as_object() {
        Some(object) => object,
        None => return false,
    };
    let is_string = |key: &str| object.get(key).and_then(Value::as_str).is_some();
    is_string("checkpointId")
        && is_string("workspacePath")
        && is_string("repoRoot")
        && is_string("workspaceInRepoPath")
        && is_string("refName")
        && is_string("commitOid")
        && object.get("createdAt").and_then(Value::as_i64).is_some()
        && object.get("scope").and_then(Value::as_str) == Some("workspace")
}

fn now_nanos_tag() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default()
}
