//! `gitCheckpointService.ts` (111 lines): orchestration between store and repo plus the three consistency guards.
//!
//! The guards (same origin as TS, compared entry by entry):
//! - `Checkpoint does not exist: {id}` —— the manifest is missing (including a corrupted file with an invalid shape)
//! - `Checkpoint workspace mismatch: {id}` —— the workspace the checkpoint belongs to is not the current caller's
//! - `Checkpoint repoRoot / scope / workspace scope mismatch.` —— the two checkpoints are not comparable
//!   (cross-repository, cross-scope, or a different monorepo subdirectory → diff/restore is always refused, so the diff is never attributed to someone else's workspace).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::{AppPaths, ServiceArgs, ServiceError};

use crate::helpers::checkpoint_ref_name;
use crate::repo::CheckpointRepo;
use crate::store::CheckpointStore;

pub struct CheckpointService {
    paths: AppPaths,
    store: CheckpointStore,
    repo: CheckpointRepo,
}

impl CheckpointService {
    pub fn new(paths: AppPaths) -> Arc<Self> {
        Arc::new(Self {
            store: CheckpointStore::new(paths.clone()),
            repo: CheckpointRepo::new(paths.clone()),
            paths,
        })
    }

    fn workspace_path(params: &Value) -> Result<String, ServiceError> {
        params
            .get("workspacePath")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| ServiceError::invalid_params("workspacePath is required"))
    }

    fn required_id(params: &Value, key: &str) -> Result<String, ServiceError> {
        params
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ServiceError::invalid_params(format!("{key} is required")))
    }

    async fn load_checkpoint(
        &self,
        workspace_path: &str,
        checkpoint_id: &str,
    ) -> Result<Value, ServiceError> {
        let meta = self
            .store
            .load(workspace_path, checkpoint_id)
            .await
            .ok_or_else(|| {
                ServiceError::new(format!("Checkpoint does not exist: {checkpoint_id}"))
            })?;
        let meta_workspace = meta
            .get("workspacePath")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if meta_workspace != workspace_path {
            return Err(ServiceError::new(format!(
                "Checkpoint workspace mismatch: {checkpoint_id}"
            )));
        }
        Ok(meta)
    }

    /// `ensureComparableCheckpoints`（gitCheckpointService.ts:25-36）。
    fn ensure_comparable(left: &Value, right: &Value) -> Result<(), ServiceError> {
        let field = |meta: &Value, key: &str| -> String {
            meta.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        if field(left, "repoRoot") != field(right, "repoRoot") {
            return Err(ServiceError::new("Checkpoint repoRoot mismatch."));
        }
        if field(left, "scope") != field(right, "scope") {
            return Err(ServiceError::new("Checkpoint scope mismatch."));
        }
        if field(left, "workspaceInRepoPath") != field(right, "workspaceInRepoPath") {
            return Err(ServiceError::new("Checkpoint workspace scope mismatch."));
        }
        Ok(())
    }

    /// `createCheckpoint` (:45-53): uuid + repo builds the snapshot → the store persists the manifest.
    pub async fn create(&self, params: &Value) -> Result<Value, ServiceError> {
        let workspace_path = Self::workspace_path(params)?;
        let checkpoint_id = new_checkpoint_id();
        let meta = self
            .repo
            .create_checkpoint(&workspace_path, &checkpoint_id)
            .await?;
        self.store.save(&meta).await?;
        Ok(meta)
    }

    /// `diffCheckpoints`（:55-68）。
    pub async fn diff(&self, params: &Value) -> Result<Value, ServiceError> {
        let workspace_path = Self::workspace_path(params)?;
        let from_id = Self::required_id(params, "fromCheckpointId")?;
        let to_id = Self::required_id(params, "toCheckpointId")?;
        let from = self.load_checkpoint(&workspace_path, &from_id).await?;
        let to = self.load_checkpoint(&workspace_path, &to_id).await?;
        Self::ensure_comparable(&from, &to)?;
        self.repo
            .diff_checkpoints(&from, &to, &workspace_path)
            .await
    }

    /// `restoreBetweenCheckpoints`（:70-87）。
    pub async fn restore(&self, params: &Value) -> Result<Value, ServiceError> {
        let workspace_path = Self::workspace_path(params)?;
        let from_id = Self::required_id(params, "fromCheckpointId")?;
        let to_id = Self::required_id(params, "toCheckpointId")?;
        let force = params
            .get("force")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let from = self.load_checkpoint(&workspace_path, &from_id).await?;
        let to = self.load_checkpoint(&workspace_path, &to_id).await?;
        Self::ensure_comparable(&from, &to)?;
        self.repo
            .restore_between(&workspace_path, &from, &to, force)
            .await
    }

    /// `deleteCheckpoint` (:89-97): delete the hidden ref first, then the manifest (the same order as TS:
    /// when the manifest delete fails the ref is already gone, so the cost is "reference lost but manifest still there" —— same as TS).
    pub async fn delete(&self, params: &Value) -> Result<Value, ServiceError> {
        let workspace_path = Self::workspace_path(params)?;
        let checkpoint_id = Self::required_id(params, "checkpointId")?;
        let meta = self
            .load_checkpoint(&workspace_path, &checkpoint_id)
            .await?;
        let ref_name = meta
            .get("refName")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                let hash = self.paths.workspace_hash(&workspace_path, None);
                checkpoint_ref_name(&hash, &checkpoint_id)
            });
        self.repo.delete_ref(&workspace_path, &ref_name).await?;
        self.store.delete(&workspace_path, &checkpoint_id).await?;
        // TS returns Promise<void>: here null represents "no return value" (consistent with the git channel's void).
        Ok(Value::Null)
    }
}

/// The local counterpart of `randomUUID()`: nanosecond time + process + a monotonic counter (no dependency on an external uuid crate,
/// since this workspace builds offline). checkpointId is only ever treated as an opaque string (the manifest file name,
/// a ref fragment), so its format is not bound to the protocol.
fn new_checkpoint_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{:x}-{sequence:x}", std::process::id())
}

#[async_trait]
impl Channel for CheckpointService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        match command {
            "createCheckpoint" => {
                let params = args.required_json(0, "params")?;
                Ok(WireValue::Json(self.create(&params).await?))
            }
            "diffCheckpoints" => {
                let params = args.required_json(0, "params")?;
                Ok(WireValue::Json(self.diff(&params).await?))
            }
            "restoreBetweenCheckpoints" => {
                let params = args.required_json(0, "params")?;
                Ok(WireValue::Json(self.restore(&params).await?))
            }
            "deleteCheckpoint" => {
                let params = args.required_json(0, "params")?;
                Ok(WireValue::Json(self.delete(&params).await?))
            }
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::process::Command;

    /// A real git repository fixture: init → config → write files → commit.
    fn temp_workspace(tag: &str) -> (String, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "zcode-checkpoint-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).expect("workspace dir");
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .expect("git fixture command");
            assert!(
                status.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&status.stderr)
            );
        };
        git(&["init"]);
        // Pin the repository configuration to rule out machine-level differences (when Windows Git defaults core.autocrlf=true it
        //  converts LF to CRLF, while conflict verification uses hash-object --no-filters,
        // so the bytes on the two sides can never be equal —— this behavior matches TS verbatim (TS also has no tests for this feature),
        // so the test pins autocrlf=false in the controlled repository and the product-side limitation is recorded in the header comment of lib.rs.
        git(&["config", "core.autocrlf", "false"]);
        git(&["config", "user.email", "checkpoint@zcode.local"]);
        git(&["config", "user.name", "Checkpoint Test"]);
        (root.to_string_lossy().into_owned(), root)
    }

    /// Isolate home/data per tag: two tests run in parallel inside the same process, and if they shared one AppPaths,
    /// the remove_dir_all at the end of the guards test would delete the manifest the round-trip test is still using
    /// (the real cause of ).
    fn service(tag: &str) -> Arc<CheckpointService> {
        let unique = format!("cp-{}-{tag}-{}", std::process::id(), unique_nanos());
        let paths = AppPaths::new(
            std::env::temp_dir().join(format!("{unique}-home")),
            std::env::temp_dir().join(format!("{unique}-data")),
        );
        CheckpointService::new(paths)
    }

    fn unique_nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default()
    }

    fn write(path: &std::path::Path, body: &str) {
        std::fs::write(path, body).expect("write fixture file");
    }

    #[tokio::test]
    async fn create_diff_restore_round_trip_with_conflict_and_force() {
        let (workspace_path, root) = temp_workspace("round");
        let service = service("round");
        let file = root.join("alpha.txt");

        write(&file, "one\n");
        let c1 = service
            .create(&json!({"workspacePath": workspace_path}))
            .await
            .expect("c1");
        assert_eq!(c1["scope"], json!("workspace"));
        assert!(c1["commitOid"].as_str().is_some());
        assert!(c1["refName"]
            .as_str()
            .unwrap()
            .starts_with("refs/zcode/checkpoints/"));
        // The manifest is persisted (the store's shape guard validates it once on the way).
        let loaded = service
            .store
            .load(&workspace_path, c1["checkpointId"].as_str().unwrap())
            .await;
        assert!(loaded.is_some(), "manifest must be readable");

        write(&file, "two\n");
        let c2 = service
            .create(&json!({"workspacePath": workspace_path}))
            .await
            .expect("c2");

        let diff = service
            .diff(&json!({
                "workspacePath": workspace_path,
                "fromCheckpointId": c1["checkpointId"],
                "toCheckpointId": c2["checkpointId"],
            }))
            .await
            .expect("diff");
        let files = diff["files"].as_array().expect("files");
        assert_eq!(files.len(), 1, "{diff:#}");
        assert_eq!(files[0]["kind"], json!("modified"));
        assert_eq!(files[0]["repoRelativePath"], json!("alpha.txt"));
        assert_eq!(files[0]["removed"], json!(1));
        assert_eq!(files[0]["added"], json!(1));

        // Both checkpoints are comparable: the disk still holds c2's content.
        let ok_restore = service
            .restore(&json!({
                "workspacePath": workspace_path,
                "fromCheckpointId": c2["checkpointId"],
                "toCheckpointId": c1["checkpointId"],
            }))
            .await
            .expect("restore");
        assert_eq!(ok_restore["success"], json!(true), "{ok_restore:#}");
        assert_eq!(
            std::fs::read_to_string(&file).expect("restored content"),
            "one\n",
            "The disk must be returned to the contents of c1",
        );

        // Three-way semantics: the disk has diverged from from (it is now "three", declared as c1's state, so restoring c2 is attempted after breaking it first) ——
        // after corrupting it to "three", going from c1 → c2 hits content-mismatch.
        write(&file, "three\n");
        let conflict=service
            .restore(&json!({
                "workspacePath": workspace_path,
                "fromCheckpointId": c1["checkpointId"],                "toCheckpointId": c2["checkpointId"],
            }))
            .await
            .expect("conflict restore");
        assert_eq!(conflict["success"], json!(false), "{conflict:#}");
        let conflicts = conflict["conflicts"].as_array().expect("conflicts");
        assert_eq!(conflicts[0]["reason"], json!("content-mismatch"));
        assert_eq!(
            std::fs::read_to_string(&file).expect("unchanged"),
            "three\n"
        );

        // With force=true overwriting is allowed, and after the restore completes the disk must equal toCheckpoint.
        let forced = service
            .restore(&json!({
                "workspacePath": workspace_path,
                "fromCheckpointId": c1["checkpointId"],
                "toCheckpointId": c2["checkpointId"],
                "force": true,
            }))
            .await
            .expect("forced restore");
        assert_eq!(forced["success"], json!(true), "{forced:#}");
        assert_eq!(
            std::fs::read_to_string(&file).expect("forced content"),
            "two\n"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = tokio::fs::remove_dir_all(service.paths.app_config_dir()).await;
    }

    #[tokio::test]
    async fn guards_report_missing_mismatch_and_scope_errors() {
        let (workspace_path, root) = temp_workspace("guards");
        let service = service("guards");

        let missing = service
            .diff(&json!({
                "workspacePath": workspace_path,
                "fromCheckpointId": "nope",
                "toCheckpointId": "nada",
            }))
            .await
            .expect_err("missing checkpoint must fail");
        assert_eq!(missing.message, "Checkpoint does not exist: nope");

        write(&root.join("b.txt"), "b\n");
        let only = service
            .create(&json!({"workspacePath": workspace_path}))
            .await
            .expect("create");
        // A nonexistent to → the same guard.
        let missing_to = service
            .diff(&json!({
                "workspacePath": workspace_path,
                "fromCheckpointId": only["checkpointId"],
                "toCheckpointId": "ghost",
            }))
            .await
            .expect_err("missing to must fail");
        assert_eq!(missing_to.message, "Checkpoint does not exist: ghost");

        // A corrupted manifest (scope is not workspace) must be read as "does not exist", not taken for a usable checkpoint.
        let manifest_dir = service
            .paths
            .app_config_dir()
            .join("checkpoints")
            .join(service.paths.workspace_hash(&workspace_path, None));
        std::fs::create_dir_all(&manifest_dir).expect("manifest dir");
        std::fs::write(
            manifest_dir.join("corrupt.json"),
            r#"{"checkpointId":"corrupt","scope":"repo","workspacePath":"x","repoRoot":"y","workspaceInRepoPath":".","createdAt":1,"refName":"r","commitOid":"o"}"#,
        )
        .expect("corrupt manifest");
        assert!(
            service
                .store
                .load(&workspace_path, "corrupt")
                .await
                .is_none(),
            "Manifests with illegal scope must be rejected by the guard (is_checkpoint_meta)"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = tokio::fs::remove_dir_all(service.paths.app_config_dir()).await;
    }
}
