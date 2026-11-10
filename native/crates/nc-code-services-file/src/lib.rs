//! `file` channel — file system operations for workspace.

/// Handoff skeleton (pure data/types): serde persistence structs, port trait, Send/Sync markers.
pub mod handoff;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::{ServiceArgs, ServiceError};
use serde_json::{json, Value};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

#[derive(Default)]
pub struct FileService {
    base_dir: Option<PathBuf>,
}

impl FileService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { base_dir: None })
    }

    pub fn with_base_dir(base_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            base_dir: Some(base_dir),
        })
    }

    fn resolve_path(&self, path: &str) -> PathBuf {
        let p = PathBuf::from(path);
        if p.is_absolute() {
            p
        } else if let Some(base) = &self.base_dir {
            base.join(p)
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(p)
        }
    }

    async fn readdir(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let path_str = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let include_hidden = params
            .get("includeHidden")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let path = self.resolve_path(path_str);
        let mut entries = Vec::new();
        let mut read_dir = fs::read_dir(&path)
            .await
            .map_err(|e| ServiceError::new(e.to_string()))?;
        while let Some(entry) = read_dir
            .next_entry()
            .await
            .map_err(|e| ServiceError::new(e.to_string()))?
        {
            let name = entry.file_name().to_string_lossy().to_string();
            if !include_hidden && name.starts_with('.') {
                continue;
            }
            let file_type = entry
                .file_type()
                .await
                .map_err(|e| ServiceError::new(e.to_string()))?;
            let metadata = entry.metadata().await.ok();
            let is_dir = file_type.is_dir();
            let is_file = file_type.is_file();
            let is_symlink = file_type.is_symlink();
            let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = metadata
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            entries.push(json!({
                "name": name,
                "path": entry.path().to_string_lossy().to_string(),
                "isDirectory": is_dir,
                "isFile": is_file,
                "isSymbolicLink": is_symlink,
                "size": size,
                "mtime": mtime,
            }));
        }
        entries.sort_by(|a, b| {
            let a_dir = a
                .get("isDirectory")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let b_dir = b
                .get("isDirectory")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if a_dir != b_dir {
                return b_dir.cmp(&a_dir);
            }
            let an = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let bn = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
            an.to_lowercase().cmp(&bn.to_lowercase())
        });
        Ok(WireValue::Json(Value::Array(entries)))
    }

    async fn stat(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let path_str = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let path = self.resolve_path(path_str);
        let metadata = fs::metadata(&path)
            .await
            .map_err(|e| ServiceError::new(e.to_string()))?;
        let is_dir = metadata.is_dir();
        let is_file = metadata.is_file();
        let is_symlink = metadata.is_symlink();
        let size = metadata.len();
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Ok(WireValue::Json(json!({
            "path": path.to_string_lossy().to_string(),
            "isDirectory": is_dir,
            "isFile": is_file,
            "isSymbolicLink": is_symlink,
            "size": size,
            "mtime": mtime,
            "exists": true,
        })))
    }

    async fn check_files_exist(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let paths = params
            .get("paths")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut results = Vec::new();
        for p in paths {
            let path_str = p.as_str().unwrap_or("");
            let resolved = self.resolve_path(path_str);
            let exists = fs::metadata(&resolved).await.is_ok();
            results.push(json!({ "path": path_str, "exists": exists }));
        }
        Ok(WireValue::Json(Value::Array(results)))
    }

    async fn resolve_path_method(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let path_str = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let resolved = self.resolve_path(path_str);
        let canonical = fs::canonicalize(&resolved).await.unwrap_or(resolved);
        Ok(WireValue::Json(Value::String(
            canonical.to_string_lossy().to_string(),
        )))
    }

    async fn ensure_conversation_workspace(&self) -> Result<WireValue, ServiceError> {
        let dir = std::env::temp_dir().join("zcode-conversations");
        fs::create_dir_all(&dir)
            .await
            .map_err(|e| ServiceError::new(e.to_string()))?;
        Ok(WireValue::Json(json!({
            "path": dir.to_string_lossy().to_string(),
            "created": true,
            "workspacePurpose": "conversation"
        })))
    }

    async fn create_default_workspace(&self) -> Result<WireValue, ServiceError> {
        let dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Ok(WireValue::Json(
            json!({ "path": dir.to_string_lossy().to_string() }),
        ))
    }

    async fn create_scratch_workspace(
        &self,
        args: &ServiceArgs,
    ) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("scratch");
        let dir = std::env::temp_dir().join("zcode-scratch").join(name);
        fs::create_dir_all(&dir)
            .await
            .map_err(|e| ServiceError::new(e.to_string()))?;
        Ok(WireValue::Json(
            json!({ "path": dir.to_string_lossy().to_string() }),
        ))
    }

    async fn read_text_file(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let path_str = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let offset = params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let length = params
            .get("length")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        let path = self.resolve_path(path_str);
        let content = fs::read_to_string(&path)
            .await
            .map_err(|e| ServiceError::new(e.to_string()))?;
        let chars: Vec<char> = content.chars().collect();
        let start = offset.min(chars.len());
        let end = if let Some(l) = length {
            (start + l).min(chars.len())
        } else {
            chars.len()
        };
        let slice: String = chars[start..end].iter().collect();
        Ok(WireValue::Json(json!({
            "content": slice,
            "offset": start,
            "length": end - start,
            "totalLength": chars.len(),
            "eof": end >= chars.len(),
        })))
    }

    async fn read_file_range(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let path_str = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let offset = params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);
        let length = params.get("length").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let path = self.resolve_path(path_str);
        let mut file = fs::File::open(&path)
            .await
            .map_err(|e| ServiceError::new(e.to_string()))?;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| ServiceError::new(e.to_string()))?;
        let mut buf = vec![0u8; length];
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| ServiceError::new(e.to_string()))?;
        buf.truncate(n);
        Ok(WireValue::Bytes(buf))
    }

    async fn search_workspace_files(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let root = params
            .get("rootPath")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
        let root_path = self.resolve_path(root);
        let mut results = Vec::new();
        let mut stack = vec![root_path.clone()];
        while let Some(dir) = stack.pop() {
            if results.len() >= limit {
                break;
            }
            let mut entries = match fs::read_dir(&dir).await {
                Ok(e) => e,
                Err(_) => continue,
            };
            while let Some(entry) = entries.next_entry().await.ok().flatten() {
                if results.len() >= limit {
                    break;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') || name == "node_modules" || name == "target" {
                    continue;
                }
                let path = entry.path();
                let rel = path
                    .strip_prefix(&root_path)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                let path_str = path.to_string_lossy().to_string();
                if is_dir {
                    stack.push(path);
                }
                if query.is_empty()
                    || name.to_lowercase().contains(&query)
                    || rel.to_lowercase().contains(&query)
                {
                    results.push(json!({
                        "path": path_str,
                        "relativePath": rel.replace('\\', "/"),
                        "name": name,
                        "isDirectory": is_dir,
                    }));
                }
            }
        }
        Ok(WireValue::Json(Value::Array(results)))
    }

    async fn read_media_preview(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let path_str = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let max_bytes = params
            .get("maxBytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(10_000_000) as usize;
        let path = self.resolve_path(path_str);
        let data = fs::read(&path)
            .await
            .map_err(|e| ServiceError::new(e.to_string()))?;
        let truncated = data.len() > max_bytes;
        let slice = if truncated {
            &data[..max_bytes]
        } else {
            &data[..]
        };
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(slice);
        let mime = mime_guess(path.to_string_lossy().as_ref());
        Ok(WireValue::Json(json!({
            "path": path.to_string_lossy().to_string(),
            "mimeType": mime,
            "data": b64,
            "size": data.len(),
            "truncated": truncated,
        })))
    }

    async fn read_binary_preview(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let path_str = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let max_bytes = params
            .get("maxBytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(4096) as usize;
        let path = self.resolve_path(path_str);
        let mut file = fs::File::open(&path)
            .await
            .map_err(|e| ServiceError::new(e.to_string()))?;
        let mut buf = vec![0u8; max_bytes];
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| ServiceError::new(e.to_string()))?;
        buf.truncate(n);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&buf);
        Ok(WireValue::Json(json!({
            "path": path.to_string_lossy().to_string(),
            "data": b64,
            "size": n,
            "truncated": n >= max_bytes,
        })))
    }

    async fn list_workspace_files_length(
        &self,
        args: &ServiceArgs,
    ) -> Result<WireValue, ServiceError> {
        let packed = self.pack_workspace_files(args).await?;
        Ok(WireValue::Json(Value::Number(serde_json::Number::from(
            packed.len() as u64,
        ))))
    }

    async fn pack_workspace_files(&self, args: &ServiceArgs) -> Result<String, ServiceError> {
        let params = args.required_json(0, "params")?;
        let root = params
            .get("rootPath")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let root_path = self.resolve_path(root);
        let mut entries = Vec::new();
        let mut stack = vec![root_path.clone()];
        while let Some(dir) = stack.pop() {
            let mut rd = match fs::read_dir(&dir).await {
                Ok(e) => e,
                Err(_) => continue,
            };
            while let Some(entry) = rd.next_entry().await.ok().flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') || name == "node_modules" || name == "target" {
                    continue;
                }
                let path = entry.path();
                let rel = path
                    .strip_prefix(&root_path)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                if is_dir {
                    stack.push(path.clone());
                }
                entries.push(format!("{}\t{}", rel, if is_dir { "d" } else { "f" }));
            }
        }
        entries.sort();
        Ok(entries.join("\n"))
    }

    async fn list_workspace_files_range(
        &self,
        args: &ServiceArgs,
    ) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let offset = params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let length = params.get("length").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let packed = self.pack_workspace_files(args).await?;
        let chars: Vec<char> = packed.chars().collect();
        let start = offset.min(chars.len());
        let end = (start + length).min(chars.len());
        let slice: String = chars[start..end].iter().collect();
        Ok(WireValue::Json(Value::String(slice)))
    }

    async fn read_workspace_file_search_ignore(
        &self,
        args: &ServiceArgs,
    ) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let root = params
            .get("rootPath")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let path = self.resolve_path(root).join(".zcodeignore");
        match fs::read_to_string(&path).await {
            Ok(content) => Ok(WireValue::Json(
                json!({ "content": content, "source": "file" }),
            )),
            Err(_) => Ok(WireValue::Json(json!({
                "content": "# ZCode workspace file search ignore\nnode_modules/\ntarget/\n.git/\n",
                "source": "template"
            }))),
        }
    }

    async fn apply_workspace_file_search_ignore_transform(
        &self,
        args: &ServiceArgs,
    ) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let root = params
            .get("rootPath")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let transform = params
            .get("transform")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let root_path = self.resolve_path(root);
        let ignore_path = root_path.join(".zcodeignore");
        let current = fs::read_to_string(&ignore_path).await.unwrap_or_default();
        let content = if transform == "sync-gitignore" {
            let gitignore = fs::read_to_string(root_path.join(".gitignore"))
                .await
                .unwrap_or_default();
            format!("{}\n# --- gitignore sync ---\n{}\n", current, gitignore)
        } else {
            format!(
                "{}\n# --- defaults reset ---\nnode_modules/\ntarget/\n.git/\n",
                current
            )
        };
        Ok(WireValue::Json(json!({ "content": content })))
    }

    async fn write_workspace_file_search_ignore(
        &self,
        args: &ServiceArgs,
    ) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let root = params
            .get("rootPath")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let content = params.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let path = self.resolve_path(root).join(".zcodeignore");
        fs::write(&path, content)
            .await
            .map_err(|e| ServiceError::new(e.to_string()))?;
        Ok(WireValue::Json(json!(null)))
    }
}

fn mime_guess(path: &str) -> String {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "txt" | "md" => "text/plain",
        "html" => "text/html",
        _ => "application/octet-stream",
    }
    .to_string()
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
        let args = ServiceArgs::new(args);
        let result = match command {
            "searchWorkspaceFiles" => self.search_workspace_files(&args).await,
            "readdir" => self.readdir(&args).await,
            "stat" => self.stat(&args).await,
            "checkFilesExist" => self.check_files_exist(&args).await,
            "resolvePath" => self.resolve_path_method(&args).await,
            "ensureConversationWorkspace" => self.ensure_conversation_workspace().await,
            "createDefaultWorkspace" => self.create_default_workspace().await,
            "createScratchWorkspace" => self.create_scratch_workspace(&args).await,
            "readTextFile" => self.read_text_file(&args).await,
            "readMediaPreview" => self.read_media_preview(&args).await,
            "readFileRange" => self.read_file_range(&args).await,
            "readBinaryPreview" => self.read_binary_preview(&args).await,
            "listWorkspaceFilesLength" => self.list_workspace_files_length(&args).await,
            "listWorkspaceFilesRange" => self.list_workspace_files_range(&args).await,
            "readWorkspaceFileSearchIgnore" => self.read_workspace_file_search_ignore(&args).await,
            "applyWorkspaceFileSearchIgnoreTransform" => {
                self.apply_workspace_file_search_ignore_transform(&args)
                    .await
            }
            "writeWorkspaceFileSearchIgnore" => {
                self.write_workspace_file_search_ignore(&args).await
            }
            other => return Err(ChannelFailure::method_not_found(other)),
        };
        result.map_err(ChannelFailure::from)
    }
}
