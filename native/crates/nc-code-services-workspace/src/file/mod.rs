//! `file` channel — filesystem reads, workspace creation, file index/search and
//! `.zcodeignore` editing. Paths are absolute; no path is rewritten.

pub mod ignore;
pub mod index;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::{AppPaths, ServiceArgs, ServiceError};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use index::IndexCache;

const DEFAULT_TEXT_READ_BYTES: u64 = 128 * 1024;
const MAX_TEXT_READ_BYTES: u64 = 256 * 1024;
const DEFAULT_MEDIA_PREVIEW_BYTES: u64 = 4 * 1024 * 1024;
const MAX_MEDIA_PREVIEW_BYTES: u64 = 8 * 1024 * 1024;
const DEFAULT_BINARY_READ_BYTES: u64 = 256 * 1024;
const MAX_BINARY_READ_BYTES: u64 = 1024 * 1024;
const MAX_BINARY_PREVIEW_BYTES: u64 = 25 * 1024 * 1024;
const MAX_EXISTENCE_BATCH: usize = 15;
const SCRATCH_ROOT_NAME: &str = "ZCodeProject";

pub struct FileService {
    paths: AppPaths,
    index: IndexCache,
}

impl FileService {
    pub fn new(paths: AppPaths) -> Arc<Self> {
        Arc::new(Self {
            paths,
            index: IndexCache::default(),
        })
    }

    async fn call_inner(
        &self,
        command: &str,
        args: Vec<WireValue>,
    ) -> Result<WireValue, ServiceError> {
        let args = ServiceArgs::new(args);
        match command {
            "readdir" => self.readdir(&args.required_json(0, "params")?).await,
            "stat" => self.stat(&args.required_json(0, "params")?).await,
            "checkFilesExist" => {
                self.check_files_exist(&args.required_json(0, "params")?)
                    .await
            }
            "resolvePath" => {
                let params = args.required_json(0, "params")?;
                let path = param_string(&params, "path")?;
                let resolved = tokio::fs::canonicalize(&path)
                    .await
                    .map_err(|error| ServiceError::new(format!("resolve path failed: {error}")))?;
                Ok(WireValue::String(resolved.to_string_lossy().into_owned()))
            }
            "ensureConversationWorkspace" => {
                let directory = self.paths.conversation_workspace_dir();
                let existed = directory.exists();
                tokio::fs::create_dir_all(&directory).await?;
                let metadata = tokio::fs::metadata(&directory).await?;
                if !metadata.is_dir() {
                    return Err(ServiceError::new(format!(
                        "Workspace path is not a directory: {}",
                        directory.display()
                    )));
                }
                Ok(WireValue::Json(json!({
                    "path": directory.to_string_lossy(),
                    "created": !existed,
                    "workspacePurpose": "conversation",
                })))
            }
            "createDefaultWorkspace" => {
                let directory = self.paths.home_dir().join(SCRATCH_ROOT_NAME);
                tokio::fs::create_dir_all(&directory).await?;
                let metadata = tokio::fs::metadata(&directory).await?;
                if !metadata.is_dir() {
                    return Err(ServiceError::new(format!(
                        "Workspace path is not a directory: {}",
                        directory.display()
                    )));
                }
                Ok(WireValue::Json(
                    json!({"path": directory.to_string_lossy()}),
                ))
            }
            "createScratchWorkspace" => {
                let params = args.required_json(0, "params")?;
                let name = param_string(&params, "name")?;
                let name = name.trim();
                if name.is_empty() {
                    return Err(ServiceError::new("Workspace name is required."));
                }
                if name.contains('\\') || name.contains('/') {
                    return Err(ServiceError::new(
                        "Workspace name cannot contain path separators.",
                    ));
                }
                let directory = self.paths.home_dir().join(SCRATCH_ROOT_NAME).join(name);
                tokio::fs::create_dir_all(&directory).await?;
                Ok(WireValue::Json(
                    json!({"path": directory.to_string_lossy()}),
                ))
            }
            "readTextFile" => self.read_text_file(&args.required_json(0, "params")?).await,
            "readFileRange" => {
                self.read_file_range(&args.required_json(0, "params")?)
                    .await
            }
            "readMediaPreview" => {
                self.read_media_preview(&args.required_json(0, "params")?)
                    .await
            }
            "readBinaryPreview" => {
                self.read_binary_preview(&args.required_json(0, "params")?)
                    .await
            }
            "searchWorkspaceFiles" => {
                self.search_workspace_files(&args.required_json(0, "params")?)
                    .await
            }
            "listWorkspaceFilesLength" => {
                let params = args.required_json(0, "params")?;
                let root = param_string(&params, "rootPath")?;
                let index = self.index.ensure(Path::new(&root), false).await?;
                Ok(WireValue::Int(index.packed.len() as u32))
            }
            "listWorkspaceFilesRange" => {
                let params = args.required_json(0, "params")?;
                let root = param_string(&params, "rootPath")?;
                let offset = param_i64(&params, "offset").unwrap_or(0).max(0) as usize;
                let length = param_i64(&params, "length").unwrap_or(0).max(0) as usize;
                let index = self.index.ensure(Path::new(&root), false).await?;
                let text = index.packed.get(offset..).unwrap_or("");
                let slice = &text[..text.len().min(length)];
                Ok(WireValue::String(slice.to_string()))
            }
            "readWorkspaceFileSearchIgnore" => {
                let params = args.required_json(0, "params")?;
                let root = param_string(&params, "rootPath")?;
                Ok(WireValue::Json(
                    ignore::read_content(Path::new(&root)).await?,
                ))
            }
            "applyWorkspaceFileSearchIgnoreTransform" => {
                let params = args.required_json(0, "params")?;
                let root = param_string(&params, "rootPath")?;
                let transform = param_string(&params, "transform")?;
                Ok(WireValue::Json(
                    ignore::transform(Path::new(&root), &transform).await?,
                ))
            }
            "writeWorkspaceFileSearchIgnore" => {
                let params = args.required_json(0, "params")?;
                let root = param_string(&params, "rootPath")?;
                let content = param_string(&params, "content")?;
                ignore::write(Path::new(&root), &content).await?;
                Ok(WireValue::Undefined)
            }
            other => Err(ServiceError::message(format!("Method not found: {other}"))),
        }
    }

    async fn readdir(&self, params: &Value) -> Result<WireValue, ServiceError> {
        let path = param_string(params, "path")?;
        let include_hidden = param_bool(params, "includeHidden").unwrap_or(false);
        let mut reader = tokio::fs::read_dir(&path)
            .await
            .map_err(|error| ServiceError::new(format!("readdir failed: {error}")))?;
        let mut entries = Vec::new();
        while let Some(entry) = reader
            .next_entry()
            .await
            .map_err(|error| ServiceError::new(format!("readdir failed: {error}")))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !include_hidden && name.starts_with('.') {
                continue;
            }
            let entry_path = entry.path();
            let file_type = entry.file_type().await.ok();
            let is_symlink = file_type
                .as_ref()
                .is_some_and(|file_type| file_type.is_symlink());
            let is_directory = if let Some(file_type) = file_type {
                if file_type.is_dir() {
                    true
                } else if file_type.is_symlink() {
                    tokio::fs::metadata(&entry_path)
                        .await
                        .map(|metadata| metadata.is_dir())
                        .unwrap_or(false)
                } else {
                    false
                }
            } else {
                false
            };
            entries.push(json!({
                "name": name,
                "path": entry_path.to_string_lossy(),
                "type": if is_directory { "directory" } else { "file" },
                "isSymbolicLink": is_symlink,
            }));
        }
        entries.sort_by(|left, right| {
            let left_dir = left["type"] == json!("directory");
            let right_dir = right["type"] == json!("directory");
            right_dir
                .cmp(&left_dir)
                .then_with(|| left["name"].as_str().cmp(&right["name"].as_str()))
        });
        Ok(WireValue::Json(Value::Array(entries)))
    }

    async fn stat(&self, params: &Value) -> Result<WireValue, ServiceError> {
        let path = param_string(params, "path")?;
        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|error| ServiceError::new(format!("stat failed: {error}")))?;
        if metadata.is_dir() {
            return Ok(WireValue::Json(json!({
                "path": path,
                "type": "directory",
            })));
        }
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        Ok(WireValue::Json(json!({
            "path": path,
            "type": "file",
            "size": metadata.len(),
            "mtimeMs": modified,
        })))
    }

    async fn check_files_exist(&self, params: &Value) -> Result<WireValue, ServiceError> {
        let paths = params
            .get("paths")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| ServiceError::invalid_params("paths is required"))?;
        if paths.len() > MAX_EXISTENCE_BATCH {
            return Err(ServiceError::new(format!(
                "File existence check supports at most {MAX_EXISTENCE_BATCH} paths."
            )));
        }
        let mut results = Vec::new();
        for path in paths {
            let path = path
                .as_str()
                .ok_or_else(|| ServiceError::invalid_params("paths must be strings"))?;
            let exists = tokio::fs::metadata(path)
                .await
                .map(|metadata| metadata.is_file())
                .unwrap_or(false);
            results.push(json!({"path": path, "exists": exists}));
        }
        Ok(WireValue::Json(Value::Array(results)))
    }

    async fn read_text_file(&self, params: &Value) -> Result<WireValue, ServiceError> {
        let path = param_string(params, "path")?;
        let offset = param_i64(params, "offset").unwrap_or(0).max(0) as u64;
        let requested = param_i64(params, "length").unwrap_or(DEFAULT_TEXT_READ_BYTES as i64);
        let length = (requested as u64).clamp(1, MAX_TEXT_READ_BYTES);
        let metadata = tokio::fs::metadata(&path).await?;
        if !metadata.is_file() {
            return Err(ServiceError::new(format!("Path is not a file: {path}")));
        }
        if offset >= metadata.len() {
            return Ok(WireValue::Json(json!({
                "path": path,
                "content": "",
                "offset": offset,
                "bytesRead": 0,
                "totalBytes": metadata.len(),
                "truncated": false,
                "isBinary": false,
            })));
        }
        let read_length = length.min(metadata.len() - offset);
        let bytes = read_range(&path, offset, read_length).await?;
        let is_binary = is_probably_binary(&bytes);
        let content = if is_binary {
            String::new()
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };
        let truncated = offset + (bytes.len() as u64) < metadata.len();
        Ok(WireValue::Json(json!({
            "path": path,
            "content": content,
            "offset": offset,
            "bytesRead": bytes.len(),
            "totalBytes": metadata.len(),
            "truncated": truncated,
            "isBinary": is_binary,
        })))
    }

    async fn read_file_range(&self, params: &Value) -> Result<WireValue, ServiceError> {
        let path = param_string(params, "path")?;
        let offset = param_i64(params, "offset").unwrap_or(0).max(0) as u64;
        let requested = param_i64(params, "length").unwrap_or(DEFAULT_BINARY_READ_BYTES as i64);
        let length = (requested as u64).clamp(1, MAX_BINARY_READ_BYTES);
        let metadata = tokio::fs::metadata(&path).await?;
        if !metadata.is_file() {
            return Err(ServiceError::new(format!("Path is not a file: {path}")));
        }
        if offset >= metadata.len() {
            return Ok(WireValue::Bytes(Vec::new()));
        }
        let bytes = read_range(&path, offset, length.min(metadata.len() - offset)).await?;
        Ok(WireValue::Bytes(bytes))
    }

    async fn read_media_preview(&self, params: &Value) -> Result<WireValue, ServiceError> {
        let path = param_string(params, "path")?;
        let max_bytes = param_i64(params, "maxBytes")
            .unwrap_or(DEFAULT_MEDIA_PREVIEW_BYTES as i64)
            .max(1) as u64;
        let max_bytes = max_bytes.min(MAX_MEDIA_PREVIEW_BYTES);
        let metadata = tokio::fs::metadata(&path).await?;
        if !metadata.is_file() {
            return Err(ServiceError::new(format!("Path is not a file: {path}")));
        }
        if metadata.len() > max_bytes {
            return Err(ServiceError::new(format!(
                "File is too large to preview: {path}"
            )));
        }
        let bytes = tokio::fs::read(&path).await?;
        Ok(WireValue::Json(json!({
            "path": path,
            "mediaType": infer_media_type(&path),
            "dataBase64": base64::engine::general_purpose::STANDARD.encode(&bytes),
            "totalBytes": metadata.len(),
        })))
    }

    async fn read_binary_preview(&self, params: &Value) -> Result<WireValue, ServiceError> {
        let path = param_string(params, "path")?;
        let max_bytes = param_i64(params, "maxBytes")
            .unwrap_or(MAX_BINARY_PREVIEW_BYTES as i64)
            .max(1) as u64;
        let metadata = tokio::fs::metadata(&path).await?;
        if !metadata.is_file() {
            return Err(ServiceError::new(format!("Path is not a file: {path}")));
        }
        if metadata.len() > max_bytes.min(MAX_BINARY_PREVIEW_BYTES) {
            return Err(ServiceError::new(format!(
                "File is too large to preview: {path}"
            )));
        }
        let bytes = tokio::fs::read(&path).await?;
        Ok(WireValue::Json(json!({
            "path": path,
            "dataBase64": base64::engine::general_purpose::STANDARD.encode(&bytes),
            "totalBytes": metadata.len(),
        })))
    }

    async fn search_workspace_files(&self, params: &Value) -> Result<WireValue, ServiceError> {
        let root = param_string(params, "rootPath")?;
        let query = param_string(params, "query")?;
        let limit = param_i64(params, "limit")
            .map(|value| value.max(0) as usize)
            .unwrap_or(1000)
            .min(1000);
        let refresh = param_bool(params, "refresh").unwrap_or(false);
        if limit == 0 {
            return Ok(WireValue::Json(json!([])));
        }
        let index = self.index.ensure(Path::new(&root), refresh).await?;
        let entries = index::search(&index, &query, limit);
        Ok(WireValue::Json(Value::Array(
            entries.iter().map(|entry| entry.to_json()).collect(),
        )))
    }
}

#[async_trait]
impl Channel for FileService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        self.call_inner(command, args).await.map_err(Into::into)
    }
}

fn param_string(params: &Value, key: &str) -> Result<String, ServiceError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ServiceError::invalid_params(format!("params.{key} is required")))
}

fn param_i64(params: &Value, key: &str) -> Option<i64> {
    params.get(key).and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
    })
}

fn param_bool(params: &Value, key: &str) -> Option<bool> {
    params.get(key).and_then(Value::as_bool)
}

async fn read_range(path: &str, offset: u64, length: u64) -> Result<Vec<u8>, ServiceError> {
    let mut file = tokio::fs::File::open(path).await?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut buffer = vec![0u8; length as usize];
    let mut read = 0usize;
    while read < buffer.len() {
        let count = file.read(&mut buffer[read..]).await?;
        if count == 0 {
            break;
        }
        read += count;
    }
    buffer.truncate(read);
    Ok(buffer)
}

fn is_probably_binary(buffer: &[u8]) -> bool {
    if buffer.is_empty() {
        return false;
    }
    let mut suspicious = 0usize;
    for &value in buffer {
        if value == 0 {
            return true;
        }
        let whitespace = matches!(value, 9 | 10 | 12 | 13);
        let control = matches!(value, 1..=8 | 14..=31 | 127);
        if !whitespace && control {
            suspicious += 1;
        }
    }
    suspicious as f64 / buffer.len() as f64 > 0.3
}

fn infer_media_type(path: &str) -> &'static str {
    let extension = PathBuf::from(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "apng" => "image/apng",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "jpeg" | "jpg" => "image/jpeg",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zcode-file-test-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn service(root: &Path) -> Arc<FileService> {
        FileService::new(AppPaths::new(root.join("home"), root.join("data")))
    }

    #[tokio::test]
    async fn reads_files_ranges_and_binary_detection() {
        let root = temp_dir("read");
        let file = root.join("hello.txt");
        std::fs::write(&file, "hello world").expect("write");
        let service = service(&root);
        let text = service
            .call_inner(
                "readTextFile",
                vec![WireValue::Json(json!({"path": file.to_string_lossy()}))],
            )
            .await
            .expect("read");
        match text {
            WireValue::Json(value) => {
                assert_eq!(value["content"], json!("hello world"));
                assert_eq!(value["isBinary"], json!(false));
            }
            other => panic!("{other:?}"),
        }
        let bytes = service
            .call_inner(
                "readFileRange",
                vec![WireValue::Json(json!({
                    "path": file.to_string_lossy(), "offset": 6, "length": 5
                }))],
            )
            .await
            .expect("range");
        assert_eq!(bytes, WireValue::Bytes(b"world".to_vec()));
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn index_search_skips_ignored_directories() {
        let root = temp_dir("index");
        std::fs::create_dir_all(root.join("src")).expect("src");
        std::fs::create_dir_all(root.join("node_modules")).expect("nm");
        std::fs::write(root.join("src/app.ts"), "x").expect("write");
        std::fs::write(root.join("node_modules/dep.ts"), "x").expect("write");
        std::fs::write(root.join(".zcodeignore"), "node_modules/\n").expect("ignore");
        let service = service(&root);
        let result = service
            .call_inner(
                "searchWorkspaceFiles",
                vec![WireValue::Json(json!({
                    "rootPath": root.to_string_lossy(), "query": "app"
                }))],
            )
            .await
            .expect("search");
        match result {
            WireValue::Json(value) => {
                let entries = value.as_array().expect("array");
                assert!(entries.iter().any(|entry| entry["name"] == json!("app.ts")));
                assert!(!entries.iter().any(|entry| entry["name"] == json!("dep.ts")));
            }
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(root).ok();
    }
}
