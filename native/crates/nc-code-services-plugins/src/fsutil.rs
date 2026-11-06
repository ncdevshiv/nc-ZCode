//! Filesystem helpers shared by the plugin engine.
//!
//! Mirrors the TypeScript atomic-write conventions: JSON documents are written as
//! `JSON.stringify(value, null, 2) + "\n"` through `nc_code_service_core::atomic`
//! (temp file + rename with Windows sharing-violation retries). Directory swaps use
//! stage → move-aside → rename → cleanup, with restore-on-failure, matching the
//! `activateDirectoryAtomically` semantics of `adapters/src/plugins/atomic-directory.ts`
//! at a granularity the channels need.

use std::path::{Component, Path, PathBuf};

use nc_code_service_core::atomic::{atomic_write_text, read_text_if_exists};
use nc_code_service_core::ServiceError;
use serde_json::Value;

/// Expand a leading `~` against `home`, then lexically normalize the path.
/// Matches TS `resolvePath` in the adapters config layer.
pub fn resolve_user_path(raw: &str, home: &Path) -> PathBuf {
    let trimmed = raw.trim();
    let base = if trimmed == "~" {
        home.to_path_buf()
    } else if let Some(rest) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
    {
        home.join(rest)
    } else {
        PathBuf::from(trimmed)
    };
    normalize_path(&base)
}

/// Lexical normalization (no IO), mirroring `path.normalize`.
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Reject `..` traversal below a root, like TS `resolveInside`.
pub fn resolve_inside(root: &Path, raw: &str) -> Option<PathBuf> {
    if Path::new(raw).is_absolute() {
        return None;
    }
    let candidate = normalize_path(&root.join(raw));
    candidate.starts_with(root).then_some(candidate)
}

/// JSON documents in plugin storage are `{ ... }\n` with two-space indent
/// (`writeJsonFile` in `marketplace.ts`).
pub async fn write_json_atomic(path: &Path, value: &Value) -> Result<(), ServiceError> {
    let text = format!("{}\n", serde_json::to_string_pretty(value)?);
    atomic_write_text(path, &text).await?;
    Ok(())
}

pub async fn read_json_if_exists(path: &Path) -> Result<Option<Value>, ServiceError> {
    let Some(text) = read_text_if_exists(path).await? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_str(&text).unwrap_or(Value::Null)))
}

pub fn is_directory(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_dir())
}

pub fn is_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

/// Recursive directory copy without following symlinks (plugin sources are untrusted;
/// the TS loader explicitly refuses symlinked skill files).
pub fn copy_dir_all(source: &Path, target: &Path) -> Result<(), ServiceError> {
    std::fs::create_dir_all(target)
        .map_err(|error| ServiceError::new(format!("create {}: {error}", target.display())))?;
    let entries = std::fs::read_dir(source)
        .map_err(|error| ServiceError::new(format!("read {}: {error}", source.display())))?;
    for entry in entries {
        let entry = entry.map_err(|error| ServiceError::new(error.to_string()))?;
        let file_type = entry
            .file_type()
            .map_err(|error| ServiceError::new(error.to_string()))?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        if file_type.is_symlink() {
            // Symlinks are skipped (trust boundary, same as the skill scanner).
            continue;
        }
        if file_type.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .map_err(|error| ServiceError::new(format!("copy {}: {error}", from.display())))?;
        }
    }
    Ok(())
}

/// Replace `target` with `staged`: move any previous target aside first so a failed
/// rename can restore it (simplified `activateDirectoryAtomically`).
pub fn swap_directories(staged: &Path, target: &Path) -> Result<(), ServiceError> {
    let mut swap = DirectorySwap::activate(staged, target)?;
    swap.commit();
    Ok(())
}

/// Deferred-commit directory activation: after [`DirectorySwap::activate`] the new
/// directory is live and the previous one is kept aside until [`DirectorySwap::commit`].
/// [`DirectorySwap::rollback`] restores the previous snapshot; dropping without a commit
/// leaves the previous snapshot in place for manual recovery, same as a crashed writer.
pub struct DirectorySwap {
    target: PathBuf,
    backup: Option<PathBuf>,
    settled: bool,
}

impl DirectorySwap {
    /// Stage must already contain the full new tree. This is the TS commit point:
    /// target → backup, staged → target, with restore-on-failure.
    pub fn activate(staged: &Path, target: &Path) -> Result<Self, ServiceError> {
        let parent = target
            .parent()
            .ok_or_else(|| ServiceError::new("swap target has no parent"))?;
        std::fs::create_dir_all(parent)
            .map_err(|error| ServiceError::new(format!("create {}: {error}", parent.display())))?;
        let backup = parent.join(format!(
            ".{}.backup-{}",
            target
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "dir".to_string()),
            uuid::Uuid::new_v4()
        ));
        let had_target = target.exists();
        if had_target {
            std::fs::rename(target, &backup).map_err(|error| {
                let _ = std::fs::remove_dir_all(staged);
                ServiceError::new(format!("move aside {}: {error}", target.display()))
            })?;
        }
        match std::fs::rename(staged, target) {
            Ok(()) => Ok(Self {
                target: target.to_path_buf(),
                backup: had_target.then_some(backup),
                settled: false,
            }),
            Err(error) => {
                if had_target {
                    let _ = std::fs::rename(&backup, target);
                }
                let _ = std::fs::remove_dir_all(&backup);
                Err(ServiceError::new(format!(
                    "activate {}: {error}",
                    target.display()
                )))
            }
        }
    }

    /// Authority state is durable: drop the previous snapshot.
    pub fn commit(&mut self) {
        if self.settled {
            return;
        }
        self.settled = true;
        if let Some(backup) = &self.backup {
            let _ = std::fs::remove_dir_all(backup);
        }
        self.backup = None;
    }

    /// Restore the previous snapshot after a later write failed.
    pub fn rollback(&mut self) -> Result<(), ServiceError> {
        if self.settled {
            return Ok(());
        }
        self.settled = true;
        let removal = if self.target.is_dir() {
            std::fs::remove_dir_all(&self.target)
        } else {
            std::fs::remove_file(&self.target)
        };
        if let Err(error) = removal {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(ServiceError::new(format!(
                    "rollback {}: {error}",
                    self.target.display()
                )));
            }
        }
        if let Some(backup) = self.backup.take() {
            std::fs::rename(&backup, &self.target).map_err(|error| {
                ServiceError::new(format!("restore {}: {error}", self.target.display()))
            })?;
        }
        Ok(())
    }

    /// Path of the staged content that was activated (used by callers to write into it).
    pub fn target(&self) -> &Path {
        &self.target
    }
}

/// Create a unique staging sibling of `target` and copy `source` (if any) into it.
pub fn create_staging_directory(
    target: &Path,
    source: Option<&Path>,
) -> Result<PathBuf, ServiceError> {
    let parent = target
        .parent()
        .ok_or_else(|| ServiceError::new("staging target has no parent"))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| ServiceError::new(format!("create {}: {error}", parent.display())))?;
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "dir".to_string());
    let staging = parent.join(format!(".{name}.stage-{}", uuid::Uuid::new_v4()));
    if let Some(source) = source {
        copy_dir_all(source, &staging)?;
    } else {
        std::fs::create_dir_all(&staging)
            .map_err(|error| ServiceError::new(format!("create {}: {error}", staging.display())))?;
    }
    Ok(staging)
}

pub fn remove_path_best_effort(path: &Path) {
    if path.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

/// ISO-8601 UTC timestamp with millisecond precision, like `new Date().toISOString()`.
pub fn now_iso() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let total_seconds = now.as_secs();
    let millis = now.subsec_millis();
    let days = (total_seconds / 86_400) as i64;
    let (year, month, day) = civil_from_days(days);
    let seconds_of_day = total_seconds % 86_400;
    format!(
        "{year:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        month,
        day,
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60,
        millis
    )
}

pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// Howard Hinnant's civil_from_days (used only by `now_iso`).
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn resolve_inside_allows_plain_relative_paths() {
        let root = Path::new("/plugins");
        let resolved = resolve_inside(root, "pkg/index.js").expect("allowed");
        assert_eq!(resolved, PathBuf::from("/plugins/pkg/index.js"));
    }

    #[test]
    fn resolve_inside_rejects_traversal_and_absolute() {
        let root = Path::new("/plugins");
        // `..` escape root → None (trust boundary: plugin source is not trusted).
        assert!(resolve_inside(root, "../secret").is_none());
        assert!(resolve_inside(root, "pkg/../../etc/passwd").is_none());
        // Absolute path → None.
        assert!(resolve_inside(root, "/etc/passwd").is_none());
        // `..` within root does not escape → allowed (still under root after normalization).
        assert!(resolve_inside(root, "pkg/../other.js").is_some());
    }

    #[test]
    fn normalize_path_collapses_curdir_and_double_slash() {
        assert_eq!(
            normalize_path(Path::new("/a/./b//c")),
            PathBuf::from("/a/b/c")
        );
    }
}
