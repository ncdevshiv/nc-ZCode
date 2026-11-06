//! `memory` channel — the local Project Memory catalogue.
//!
//! Port of `packages/services/src/memory/memoryService.ts` and
//! `packages/services/src/memory/projectMemoryStableRead.ts`. Same directory, same
//! fail-closed path checks, same sort order, same preview limit and error codes:
//! `{dataBaseDir}/.zcode/cli/memories/projects/<workspaceId>/memory/*.md`.
//!
//! The path guards are not decoration. The projects root and each workspace directory must be
//! **plain directories** (a symlinked root would let `list`/`read` follow outside the local
//! profile), and the file name must match **exactly** what the catalogue listed, because a
//! case-insensitive filesystem would otherwise resolve a request for `Memory.md` onto a
//! different file than the one the UI was shown.

use std::path::{Path, PathBuf};

use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::{AppPaths, ServiceArgs, ServiceError};
use serde_json::{json, Value};

pub const PROJECT_MEMORY_PREVIEW_LIMIT_EXCEEDED_ERROR_CODE: &str =
    "PROJECT_MEMORY_PREVIEW_LIMIT_EXCEEDED";
pub const PROJECT_MEMORY_FILE_CHANGED_ERROR_CODE: &str = "PROJECT_MEMORY_FILE_CHANGED";

const PROJECT_MEMORY_INDEX_FILE_NAME: &str = "MEMORY.md";
const PROJECT_MEMORY_DIRECTORY_NAME: &str = "memory";
const PROJECT_MEMORY_PREVIEW_MAX_BYTES: u64 = 5 * 1024 * 1024;
/// `PROJECT_KEY_SUFFIX_PATTERN = /^(.*)-[a-f0-9]{16}$/i`
const WORKSPACE_ID_HASH_SUFFIX_LEN: usize = 17;

pub struct MemoryService {
    paths: AppPaths,
}

impl MemoryService {
    pub fn new(paths: AppPaths) -> Self {
        Self { paths }
    }

    /// `getProjectMemoriesRoot()`: `{dataBaseDir}/.zcode/cli/memories/projects`.
    pub fn projects_root(&self) -> PathBuf {
        self.paths
            .nc_code_data_root()
            .join("cli")
            .join("memories")
            .join("projects")
    }

    async fn list_project_memories(&self) -> Result<Vec<Value>, ServiceError> {
        let projects_root = self.projects_root();
        let entries = match require_plain_directory(&projects_root).await {
            Ok(()) => read_directory_entries(&projects_root).await,
            Err(DirGuard::Missing) => return Ok(Vec::new()),
            Err(guard) => return Err(guard.into()),
        };
        let entries = match entries {
            Some(entries) => entries,
            None => return Ok(Vec::new()),
        };

        let mut workspaces: Vec<Value> = Vec::new();
        for entry in entries {
            if !entry.is_directory {
                continue;
            }
            let workspace_id = entry.name;
            let workspace_root = projects_root.join(&workspace_id);
            let memory_root = workspace_root.join(PROJECT_MEMORY_DIRECTORY_NAME);
            if !is_plain_directory(&workspace_root).await || !is_plain_directory(&memory_root).await
            {
                continue;
            }
            let Some(memory_entries) = read_directory_entries(&memory_root).await else {
                // The memory agent can remove the directory after the check; a catalogue
                // snapshot skips the workspace instead of failing the whole list.
                continue;
            };
            let mut files: Vec<FileSummary> = Vec::new();
            for memory_entry in memory_entries {
                if !memory_entry.is_file || !is_project_memory_file_name(&memory_entry.name) {
                    continue;
                }
                let file_path = memory_root.join(&memory_entry.name);
                let metadata = match tokio::fs::symlink_metadata(&file_path).await {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    continue;
                }
                files.push(FileSummary {
                    name: memory_entry.name.clone(),
                    path: file_path,
                    kind: if memory_entry.name == PROJECT_MEMORY_INDEX_FILE_NAME {
                        FileKind::Index
                    } else {
                        FileKind::Item
                    },
                    size: metadata.len(),
                    updated_at: epoch_millis(&metadata),
                });
            }
            if files.is_empty() {
                continue;
            }
            files.sort_by(|left, right| {
                left.kind
                    .cmp(&right.kind)
                    .then_with(|| crate::text::locale_en_cmp(&left.name, &right.name))
            });
            let updated_at = files
                .iter()
                .map(|file| file.updated_at)
                .fold(f64::NEG_INFINITY, f64::max);
            workspaces.push(json!({
                "id": workspace_id,
                "label": resolve_workspace_label(&workspace_id),
                "updatedAt": updated_at,
                "files": files.iter().map(FileSummary::to_json).collect::<Vec<_>>(),
            }));
        }

        workspaces.sort_by(|left, right| {
            let left_updated = left["updatedAt"].as_f64().unwrap_or_default();
            let right_updated = right["updatedAt"].as_f64().unwrap_or_default();
            right_updated
                .partial_cmp(&left_updated)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    crate::text::locale_en_cmp(
                        left["id"].as_str().unwrap_or_default(),
                        right["id"].as_str().unwrap_or_default(),
                    )
                })
        });
        Ok(workspaces)
    }

    async fn read_project_memory_file(
        &self,
        workspace_id: &str,
        file_name: &str,
    ) -> Result<Value, ServiceError> {
        if !is_valid_path_segment(workspace_id)
            || !is_valid_path_segment(file_name)
            || !is_project_memory_file_name(file_name)
        {
            return Err(ServiceError::new("Invalid Project Memory path"));
        }
        let projects_root = self.projects_root();
        require_plain_directory(&projects_root).await?;
        let workspace_root = projects_root.join(workspace_id);
        let memory_root = workspace_root.join(PROJECT_MEMORY_DIRECTORY_NAME);
        require_plain_directory(&workspace_root).await?;
        require_plain_directory(&memory_root).await?;
        let file_path = require_exact_project_memory_file(&memory_root, file_name).await?;

        // Re-run every containment guard with the handle open, then read only from that
        // handle — `readFile(path)` after the checks would re-resolve a concurrently swapped
        // symlink.
        let pre_open = tokio::fs::symlink_metadata(&file_path).await?;
        if !pre_open.is_file() || pre_open.file_type().is_symlink() {
            return Err(ServiceError::new(format!(
                "Project Memory file is not a regular file: {}",
                file_path.display()
            )));
        }
        let handle = tokio::fs::File::open(&file_path).await?;
        let opened = handle.metadata().await?;
        require_plain_directory(&projects_root).await?;
        require_plain_directory(&workspace_root).await?;
        require_plain_directory(&memory_root).await?;
        require_exact_project_memory_file(&memory_root, file_name).await?;
        assert_contained_project_memory_path(&projects_root, &file_path).await?;
        let post_open = tokio::fs::symlink_metadata(&file_path).await?;
        if !opened.is_file()
            || !post_open.is_file()
            || post_open.file_type().is_symlink()
            || !same_file_snapshot(&opened, &pre_open)
            || !same_file_snapshot(&opened, &post_open)
        {
            return Err(file_changed_error(file_name));
        }
        if opened.len() > PROJECT_MEMORY_PREVIEW_MAX_BYTES {
            return Err(preview_limit_error(file_name));
        }
        let bytes = read_bounded(&handle, PROJECT_MEMORY_PREVIEW_MAX_BYTES + 1).await?;
        let final_metadata = handle.metadata().await?;
        if !same_file_snapshot(&opened, &final_metadata) {
            return Err(file_changed_error(file_name));
        }
        if (bytes.len() as u64) > PROJECT_MEMORY_PREVIEW_MAX_BYTES
            || final_metadata.len() > PROJECT_MEMORY_PREVIEW_MAX_BYTES
        {
            return Err(preview_limit_error(file_name));
        }
        let content = String::from_utf8_lossy(&bytes).into_owned();
        Ok(json!({
            "content": content,
            "updatedAt": epoch_millis(&final_metadata),
        }))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FileKind {
    Index,
    Item,
}

struct FileSummary {
    name: String,
    path: PathBuf,
    kind: FileKind,
    size: u64,
    updated_at: f64,
}

impl FileSummary {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "path": self.path.to_string_lossy(),
            "kind": match self.kind {
                FileKind::Index => "index",
                FileKind::Item => "item",
            },
            "size": self.size,
            "updatedAt": self.updated_at,
        })
    }
}

struct DirectoryEntry {
    name: String,
    is_directory: bool,
    is_file: bool,
}

/// `readdir(withFileTypes)` + the `isSymbolicLink()` guards: entry types come from the
/// directory listing, so a symlink never counts as a plain file or directory.
async fn read_directory_entries(path: &Path) -> Option<Vec<DirectoryEntry>> {
    let mut read_dir = tokio::fs::read_dir(path).await.ok()?;
    let mut entries = Vec::new();
    while let Ok(Some(entry)) = read_dir.next_entry().await {
        let Ok(file_type) = entry.metadata().await.map(|metadata| metadata.file_type()) else {
            continue;
        };
        entries.push(DirectoryEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_directory: file_type.is_dir() && !file_type.is_symlink(),
            is_file: file_type.is_file() && !file_type.is_symlink(),
        });
    }
    Some(entries)
}

async fn is_plain_directory(path: &Path) -> bool {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata.is_dir() && !metadata.file_type().is_symlink(),
        Err(_) => false,
    }
}

/// `requirePlainDirectory`, kept structured so `listProjectMemories` can tell "the projects root
/// does not exist yet" (empty catalogue) from "this is not a plain directory" (an error).
async fn require_plain_directory(path: &Path) -> Result<(), DirGuard> {
    let metadata = tokio::fs::symlink_metadata(path).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            DirGuard::Missing
        } else {
            DirGuard::Io(error)
        }
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(DirGuard::NotPlain(path.display().to_string()));
    }
    Ok(())
}

enum DirGuard {
    /// ENOENT: TypeScript surfaces the raw `ENOENT` error object, which `list` swallows.
    Missing,
    Io(std::io::Error),
    NotPlain(String),
}

impl From<DirGuard> for ServiceError {
    fn from(guard: DirGuard) -> Self {
        match guard {
            DirGuard::Missing => ServiceError::new("No such file or directory (os error 2)"),
            DirGuard::Io(error) => ServiceError::from(error),
            DirGuard::NotPlain(path) => ServiceError::new(format!(
                "Project Memory directory is not a regular directory: {path}"
            )),
        }
    }
}

async fn require_exact_project_memory_file(
    memory_root: &Path,
    file_name: &str,
) -> Result<PathBuf, ServiceError> {
    let entries = read_directory_entries(memory_root)
        .await
        .unwrap_or_default();
    let matched = entries.iter().find(|entry| entry.name == file_name);
    let requested = memory_root.join(file_name);
    match matched {
        None => {
            // A genuinely absent file must still surface the original ENOENT; only a
            // case-insensitive alias is rejected by name.
            tokio::fs::symlink_metadata(&requested).await?;
            Err(ServiceError::new(format!(
                "Project Memory file name does not match exactly: {file_name}"
            )))
        }
        Some(entry) if !entry.is_file => Err(ServiceError::new(format!(
            "Project Memory file is not a regular file: {file_name}"
        ))),
        Some(_) => Ok(requested),
    }
}

async fn assert_contained_project_memory_path(
    projects_root: &Path,
    target: &Path,
) -> Result<(), ServiceError> {
    let root_real = canonical_display(projects_root).await;
    let target_real = canonical_display(target).await;
    if !target_real.starts_with(&root_real) {
        return Err(ServiceError::new(format!(
            "Project Memory path is outside the local profile: {}",
            target.display()
        )));
    }
    Ok(())
}

/// `fs.realpath`, normalized so a Windows `\\?\` prefix cannot make both sides of a
/// containment check disagree.
async fn canonical_display(path: &Path) -> PathBuf {
    let resolved = tokio::fs::canonicalize(path)
        .await
        .unwrap_or_else(|_| path.to_path_buf());
    let text = resolved.to_string_lossy().into_owned();
    if let Some(stripped) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(stripped.to_string().replace('\\', "/"));
    }
    resolved
}

fn is_valid_path_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
}

fn is_project_memory_file_name(file_name: &str) -> bool {
    file_name == PROJECT_MEMORY_INDEX_FILE_NAME
        || (file_name.ends_with(".md") && file_name != PROJECT_MEMORY_INDEX_FILE_NAME)
}

/// `resolveWorkspaceLabel`: strip a trailing `-<16 hex>` key suffix when present.
fn resolve_workspace_label(workspace_id: &str) -> String {
    let bytes = workspace_id.as_bytes();
    if bytes.len() > WORKSPACE_ID_HASH_SUFFIX_LEN {
        let separator = bytes.len() - WORKSPACE_ID_HASH_SUFFIX_LEN;
        if workspace_id.as_bytes()[separator] == b'-' {
            let suffix = &workspace_id[separator + 1..];
            let is_hex_key =
                suffix.len() == 16 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit());
            if is_hex_key {
                let trimmed = workspace_id[..separator].trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }
    }
    workspace_id.to_string()
}

fn same_file_snapshot(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    file_identity(left) == file_identity(right)
}

/// Node compares `dev`/`ino`/`size`/`mtimeNs`/`ctimeNs`. Those inode fields do not exist on
/// Windows, so the portable identity is size + modified + created; on Unix the real device and
/// inode numbers are added.
#[derive(PartialEq, Eq)]
struct FileIdentity {
    size: u64,
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    #[cfg(unix)]
    dev_ino: Option<(u64, u64)>,
}

fn file_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    FileIdentity {
        size: metadata.len(),
        modified: metadata.modified().ok(),
        created: metadata.created().ok(),
        #[cfg(unix)]
        dev_ino: {
            use std::os::unix::fs::MetadataExt;
            Some((metadata.dev(), metadata.ino()))
        },
    }
}

fn epoch_millis(metadata: &std::fs::Metadata) -> f64 {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| (since.as_nanos() as f64) / 1_000_000.0)
        .unwrap_or_default()
}

async fn read_bounded(handle: &tokio::fs::File, max_bytes: u64) -> Result<Vec<u8>, ServiceError> {
    use tokio::io::AsyncReadExt;
    let mut buffer = Vec::new();
    let mut limited = handle.try_clone().await?.take(max_bytes);
    limited.read_to_end(&mut buffer).await?;
    Ok(buffer)
}

fn preview_limit_error(file_name: &str) -> ServiceError {
    ServiceError::new(format!(
        "Project Memory file exceeds the 5 MiB preview limit: {file_name}"
    ))
    .with_code(PROJECT_MEMORY_PREVIEW_LIMIT_EXCEEDED_ERROR_CODE)
}

fn file_changed_error(file_name: &str) -> ServiceError {
    ServiceError::new(format!(
        "Project Memory file changed during preview: {file_name}"
    ))
    .with_code(PROJECT_MEMORY_FILE_CHANGED_ERROR_CODE)
}

#[async_trait::async_trait]
impl Channel for MemoryService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        match command {
            "listProjectMemories" => {
                let workspaces = self.list_project_memories().await?;
                Ok(WireValue::Json(Value::Array(workspaces)))
            }
            "readProjectMemoryFile" => {
                let params = args.required_json(0, "params")?;
                let workspace_id = params
                    .get("workspaceId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ServiceError::invalid_params("params.workspaceId must be a string")
                    })?;
                let file_name =
                    params
                        .get("fileName")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            ServiceError::invalid_params("params.fileName must be a string")
                        })?;
                Ok(WireValue::Json(
                    self.read_project_memory_file(workspace_id, file_name)
                        .await?,
                ))
            }
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(tag: &str) -> (AppPaths, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "zcode-memory-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        (AppPaths::new(root.join("home"), root.join("data")), root)
    }

    async fn write_memory(root: &Path, workspace_id: &str, name: &str, body: &str) {
        let dir = root.join(workspace_id).join(PROJECT_MEMORY_DIRECTORY_NAME);
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join(name), body).await.unwrap();
    }

    #[tokio::test]
    async fn empty_when_the_projects_root_is_absent() {
        let (paths, root) = temp_paths("empty");
        let service = MemoryService::new(paths);
        assert_eq!(
            service.list_project_memories().await.unwrap(),
            Vec::<serde_json::Value>::new()
        );
        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn lists_only_plain_markdown_files_index_first() {
        let (paths, root) = temp_paths("list");
        let projects = MemoryService::new(paths.clone()).projects_root();
        tokio::fs::create_dir_all(&projects).await.unwrap();
        write_memory(
            &projects,
            "demo-workspace-0123456789abcdef",
            "MEMORY.md",
            "# index\n",
        )
        .await;
        write_memory(
            &projects,
            "demo-workspace-0123456789abcdef",
            "notes.md",
            "notes body",
        )
        .await;
        write_memory(
            &projects,
            "demo-workspace-0123456789abcdef",
            "skip.txt",
            "x",
        )
        .await;
        write_memory(
            &projects,
            "empty-workspace-0123456789abcdeg",
            "MEMORY.md",
            "x",
        )
        .await;
        // A workspace directory with no memory/ subdirectory contributes nothing.
        tokio::fs::create_dir_all(projects.join("bare-workspace"))
            .await
            .unwrap();

        let service = MemoryService::new(paths);
        let listed = service.list_project_memories().await.unwrap();
        assert_eq!(listed.len(), 2, "{listed:?}");
        // The sort rule is updatedAt descending, with id ascending only as a tiebreaker within the same millisecond (memoryService.ts:196);
        // the actual millisecond of the file write varies by machine, so assert the rule itself rather than a fixed ranking.
        let ids: Vec<&str> = listed
            .iter()
            .map(|entry| entry["id"].as_str().unwrap_or_default())
            .collect();
        assert!(ids.contains(&"demo-workspace-0123456789abcdef"), "{ids:?}");
        assert!(ids.contains(&"empty-workspace-0123456789abcdeg"), "{ids:?}");
        assert!(
            listed[0]["updatedAt"].as_f64().unwrap_or_default()
                >= listed[1]["updatedAt"].as_f64().unwrap_or_default(),
            "{listed:?}"
        );
        let demo = listed
            .iter()
            .find(|entry| entry["id"] == json!("demo-workspace-0123456789abcdef"))
            .expect("demo entry");
        assert_eq!(demo["label"], json!("demo-workspace"));
        let files = demo["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        // index must come before ordinary files (memoryService.ts:106-107).
        assert_eq!(files[0]["kind"], json!("index"));
        assert_eq!(files[0]["name"], json!("MEMORY.md"));
        assert_eq!(files[1]["name"], json!("notes.md"));
        assert_eq!(files[1]["size"], json!(10));
        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn reads_a_file_and_rejects_bad_names() {
        let (paths, root) = temp_paths("read");
        let projects = MemoryService::new(paths.clone()).projects_root();
        tokio::fs::create_dir_all(&projects).await.unwrap();
        write_memory(
            &projects,
            "ws-0123456789abcdef",
            "MEMORY.md",
            "# project memory\n",
        )
        .await;
        let service = MemoryService::new(paths);
        let read = service
            .read_project_memory_file("ws-0123456789abcdef", "MEMORY.md")
            .await
            .unwrap();
        assert_eq!(read["content"], json!("# project memory\n"));
        assert!(read["updatedAt"].as_f64().unwrap() > 0.0);
        assert_eq!(
            service
                .read_project_memory_file("ws-0123456789abcdef", "memory.md")
                .await
                .err()
                .unwrap()
                .message,
            "Project Memory file name does not match exactly: memory.md"
        );
        assert_eq!(
            service
                .read_project_memory_file("../elsewhere", "MEMORY.md")
                .await
                .err()
                .unwrap()
                .message,
            "Invalid Project Memory path"
        );
        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn oversized_file_reports_the_preview_limit_code() {
        let (paths, root) = temp_paths("oversized");
        let projects = MemoryService::new(paths.clone()).projects_root();
        tokio::fs::create_dir_all(&projects).await.unwrap();
        write_memory(
            &projects,
            "ws-0123456789abcdef",
            "big.md",
            &"x".repeat((PROJECT_MEMORY_PREVIEW_MAX_BYTES + 1) as usize),
        )
        .await;
        let service = MemoryService::new(paths);
        let error = service
            .read_project_memory_file("ws-0123456789abcdef", "big.md")
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.code,
            Some(Box::new(json!(
                PROJECT_MEMORY_PREVIEW_LIMIT_EXCEEDED_ERROR_CODE
            )))
        );
        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[test]
    fn label_suffix_rules_match_the_typescript_pattern() {
        assert_eq!(resolve_workspace_label("my-app-0123456789abcdef"), "my-app");
        assert_eq!(resolve_workspace_label("my-app-ABCDEF0123456789"), "my-app");
        assert_eq!(resolve_workspace_label("my-app-xyz"), "my-app-xyz");
        assert_eq!(
            resolve_workspace_label("0123456789abcdef"),
            "0123456789abcdef"
        );
        assert!(is_valid_path_segment("MEMORY.md"));
        assert!(!is_valid_path_segment("a/b"));
        assert!(!is_valid_path_segment("a\\b"));
        assert!(!is_project_memory_file_name("notes.markdown"));
    }
}
