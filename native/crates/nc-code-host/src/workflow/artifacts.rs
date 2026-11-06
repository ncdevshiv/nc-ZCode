//! Publishing of user interface products (port of `workflow-artifact-publish.ts`).
//!
//! ⚠ Terminology: The artifact here is the bytes delivered to the user (`artifact.file` / `artifact.markdown`),
//! is not a top-level return value within the engine. This module does not touch the latter.
//!
//! Differences from TS (honest record):
//! - TS writes `ToolArtifactStorePort` (session scope, retention=project); this host has no product storage,
//! Build a content store here: `<data-dir>/workflow-artifacts/<runId>/<id>-v<n>.<ext>`.
//! According to the convention of this warehouse, the uri is `nc-code-artifact://<runId>/<id>` (no session scope - the store of host is
//! Process-level data-dir, not partitioned by session).
//! - Rejection semantics are retained one by one: out of bounds (including realpath review), source missing, overlimit (rejection without truncation), id/opts shape.
//! - `spec` is a field of a preset member (chart/table/...), and the content member is always None.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use nc_code_workflow::types::{
    ArtifactPublishRequest, ArtifactVersionRecord, WorkflowError, WorkflowErrorCode, ARTIFACT_CAPS,
};
use regex::Regex;
use serde_json::Value;

use crate::workflow::world_read::{resolve_within_workspace, to_workspace_relative};

/// The byte placement point of the content artifact (`workflow-artifacts/` under the process-level data-dir).
#[derive(Clone, Debug)]
pub struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    /// `<data-dir>/workflow-artifacts`.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join("workflow-artifacts"),
        }
    }

    /// Explicit root directory for testing/assembly.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Execute a content product release: verify the shape → get bytes (including the upper limit) → drop to disk → normalize into drop records.
    ///
    /// The order is **shape → byte → storage**: shape is cheapest and assembly-independent; source missingness should not be masked by a write failure.
    pub async fn publish(
        &self,
        workspace: &Path,
        request: ArtifactPublishRequest,
    ) -> Result<ArtifactVersionRecord, WorkflowError> {
        let workspace = workspace.to_path_buf();
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.publish_sync(&workspace, request))
            .await
            .map_err(|error| {
                driver_error(format!("artifact publish task failed to run: {error}"))
            })?
    }

    fn publish_sync(
        &self,
        workspace: &Path,
        request: ArtifactPublishRequest,
    ) -> Result<ArtifactVersionRecord, WorkflowError> {
        assert_artifact_id(&request)?;
        let opts = publish_options(&request)?;
        let payload = match request.op {
            nc_code_workflow::types::ArtifactOp::File => {
                read_file_payload(workspace, &request, &opts)?
            }
            nc_code_workflow::types::ArtifactOp::Markdown => markdown_payload(&request)?,
            other => {
                // The preset member is just the driver; reaching this point means that the engine/script is dispatched wrong.
                return Err(driver_error(format!(
                    "artifact.{}: only content artifacts (file, markdown) are published through the driver.",
                    other.as_str()
                )));
            }
        };
        let bytes = self.write_payload(&request, &payload)?;
        Ok(ArtifactVersionRecord {
            id: request.id.clone(),
            kind: request.op,
            version: request.version,
            title: opts.title,
            description: opts.description,
            content_type: Some(payload.content_type.clone()),
            bytes: Some(bytes),
            uri: Some(format!(
                "zcode-artifact://{}/{}",
                request.run_id, request.id
            )),
            source_path: payload.source_path.clone(),
            spec: None,
            published_at: Some(crate::store::now_ms() as f64),
            primary: None,
        })
    }
    /// Write bytes: temporary file + rename (crash will not leave half a product). Resending the same version is idempotent (overwriting).
    fn write_payload(
        &self,
        request: &ArtifactPublishRequest,
        payload: &Payload,
    ) -> Result<u64, WorkflowError> {
        let directory = self.root.join(sanitize_path_segment(&request.run_id));
        std::fs::create_dir_all(&directory).map_err(|error| {
            driver_error(format!(
                "artifact.{}: cannot create the artifact directory {}: {error}",
                request.op.as_str(),
                directory.display()
            ))
        })?;
        let file_name = format!("{}-v{}.{}", request.id, request.version, payload.extension);
        let target = directory.join(&file_name);
        let temporary = directory.join(format!(
            ".{file_name}.tmp-{}-{}",
            std::process::id(),
            crate::store::now_ms()
        ));
        std::fs::write(&temporary, &payload.bytes).map_err(|error| {
            driver_error(format!(
                "artifact.{}: writing '{}' v{} failed: {error}",
                request.op.as_str(),
                request.id,
                request.version
            ))
        })?;
        // Overwrite the version with the same name when retrying (rename does not overwrite existing targets on Windows).
        let _ = std::fs::remove_file(&target);
        std::fs::rename(&temporary, &target).map_err(|error| {
            let _ = std::fs::remove_file(&temporary);
            driver_error(format!(
                "artifact.{}: writing '{}' v{} failed: {error}",
                request.op.as_str(),
                request.id,
                request.version
            ))
        })?;
        Ok(payload.bytes.len() as u64)
    }
}

// ————————————————————————————— Shape verification ——————————————————————————————

#[derive(Default)]
struct PublishOptions {
    title: Option<String>,
    description: Option<String>,
    content_type: Option<String>,
}

fn publish_options(request: &ArtifactPublishRequest) -> Result<PublishOptions, WorkflowError> {
    let Some(raw) = &request.opts else {
        return Ok(PublishOptions::default());
    };
    let Value::Object(bag) = raw else {
        return Err(driver_error(format!(
            "artifact.{}: opts must be an options object, got {}. Pass an object or omit the argument.",
            request.op.as_str(),
            describe_arg(raw)
        )));
    };
    let title = optional_text(
        request,
        "title",
        bag.get("title"),
        ARTIFACT_CAPS.max_title_length,
    )?;
    let description = optional_text(
        request,
        "description",
        bag.get("description"),
        ARTIFACT_CAPS.max_description_length,
    )?;
    // The `markdown` family doesn't read contentType at all (facade's markdown opts don't have it).
    let content_type = if request.op == nc_code_workflow::types::ArtifactOp::File {
        optional_content_type(bag.get("contentType"))?
    } else {
        None
    };
    Ok(PublishOptions {
        title,
        description,
        content_type,
    })
}

/// An optional bounded text option; length in UTF-16 code units (consistent with TS's `value.length`).
fn optional_text(
    request: &ArtifactPublishRequest,
    name: &str,
    value: Option<&Value>,
    max: usize,
) -> Result<Option<String>, WorkflowError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(text) = value.as_str() else {
        return Err(driver_error(format!(
            "artifact.{}: opts.{name} must be a string, got {}.",
            request.op.as_str(),
            describe_arg(value)
        )));
    };
    let length = text.encode_utf16().count();
    if length > max {
        // Reject instead of truncate: A title with the last half of the sentence chopped off is a bad deliverable that looks normal.
        return Err(WorkflowError::new(
            WorkflowErrorCode::ArtifactTooLarge,
            format!(
                "artifact.{}: opts.{name} is {length} characters, over the cap of {max}. Shorten it.",
                request.op.as_str()
            ),
        ));
    }
    Ok(Some(text.to_string()))
}

/// `opts.contentType` must be bare MIME without arguments (UI dispatched as exact string).
fn optional_content_type(value: Option<&Value>) -> Result<Option<String>, WorkflowError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(text) = value.as_str() else {
        return Err(driver_error(format!(
            "artifact.file: opts.contentType must be a string, got {}.",
            describe_arg(value)
        )));
    };
    if !content_type_pattern().is_match(text) {
        return Err(driver_error(format!(
            "artifact.file: opts.contentType '{text}' is not a bare MIME type such as \
             'application/pdf'. Drop any parameters ('; charset=...'); the viewer dispatches on \
             the exact string."
        )));
    }
    Ok(Some(text.to_string()))
}

/// Runtime guardrails for the product id: it enters the store's filename, so the character set and length must be checked here.
fn assert_artifact_id(request: &ArtifactPublishRequest) -> Result<(), WorkflowError> {
    let id = &request.id;
    if id.is_empty() || id.encode_utf16().count() > ARTIFACT_CAPS.max_id_length {
        return Err(driver_error(format!(
            "artifact.{}: id '{id}' is not a valid artifact id. Use at most {} characters from \
             [A-Za-z0-9_.-].",
            request.op.as_str(),
            ARTIFACT_CAPS.max_id_length
        )));
    }
    if !artifact_id_pattern().is_match(id) {
        return Err(driver_error(format!(
            "artifact.{}: id '{id}' is not a valid artifact id. Use at most {} characters from [A-Za-z0-9_.-].",
            request.op.as_str(),
            ARTIFACT_CAPS.max_id_length
        )));
    }
    Ok(())
}

// —————————————————————————————— Get bytes ——————————————————————————————

struct Payload {
    bytes: Vec<u8>,
    content_type: String,
    extension: String,
    source_path: Option<String>,
}

struct ResolvedSource {
    /// realpath (use it for both out-of-bounds review and reading).
    real: PathBuf,
    /// The relative shape of the workspace after normalization of the path given by the script ("Display in workspace" on the card points back here).
    relative: String,
}

fn read_file_payload(
    workspace: &Path,
    request: &ArtifactPublishRequest,
    opts: &PublishOptions,
) -> Result<Payload, WorkflowError> {
    let given = request.path.as_deref().filter(|path| !path.is_empty());
    let Some(given) = given else {
        return Err(driver_error(
            "artifact.file: path must be a non-empty string. Pass the workspace-relative path the \
             subagent wrote.",
        ));
    };
    let source = resolve_workspace_file(workspace, given)?;
    let bytes = read_source_capped(given, &source.real)?;
    let extension = file_extension(&source.real);
    let content_type = opts
        .content_type
        .clone()
        .or_else(|| extension.as_deref().and_then(content_type_for_extension))
        // Not listed in the extension table = no guessing (no magic bytes sniffing).
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let stored_extension = extension
        .clone()
        .or_else(|| extension_for_content_type(&content_type))
        .unwrap_or_else(|| "bin".to_string());
    Ok(Payload {
        bytes,
        content_type,
        extension: stored_extension,
        source_path: Some(source.relative),
    })
}

fn markdown_payload(request: &ArtifactPublishRequest) -> Result<Payload, WorkflowError> {
    let Some(content) = &request.content else {
        return Err(driver_error("artifact.markdown: content must be a string."));
    };
    let bytes = content.as_bytes();
    let cap = ARTIFACT_CAPS.max_markdown_bytes;
    if bytes.len() as u64 > cap {
        return Err(WorkflowError::new(
            WorkflowErrorCode::ArtifactTooLarge,
            format!(
                "artifact.markdown: content is {} bytes, over the cap of {cap} bytes. Shorten it, \
                 or write the long content to a workspace file and publish it with artifact.file.",
                bytes.len()
            ),
        ));
    }
    Ok(Payload {
        bytes: bytes.to_vec(),
        content_type: "text/markdown".to_string(),
        extension: "md".to_string(),
        source_path: None,
    })
}

/// Parse the path given by the script and do **twice** out-of-bounds judgment: once for lexical (same criteria as files.read), and after realpath
/// Again (symbolic links are judged according to their real paths). The latter cannot be omitted: publishing copies the bytes into the persistent store and puts them in front of the user.
/// A soft link can turn a private file outside the workspace into a card.
fn resolve_workspace_file(workspace: &Path, given: &str) -> Result<ResolvedSource, WorkflowError> {
    let resolved =
        resolve_within_workspace(workspace, given).ok_or_else(|| outside_workspace(given))?;
    let relative = to_workspace_relative(workspace, &resolved);
    let real_workspace = std::fs::canonicalize(workspace).map_err(|error| {
        driver_error(format!(
            "artifact.file: cannot resolve the workspace root '{}': {error}",
            workspace.display()
        ))
    })?;
    let real = std::fs::canonicalize(&resolved)
        // ENOENT (including a section in the middle that does not exist) is the same thing as "pointing to a non-existent soft link target".
        .map_err(|error| source_missing(given, &error))?;
    if !real.starts_with(&real_workspace) {
        return Err(outside_workspace(given));
    }
    Ok(ResolvedSource { real, relative })
}

/// Read the source file bytes: first press metadata to reject (large files do not need to be read), and then cap+1 to detect (to prevent the file from becoming larger during reading).
fn read_source_capped(given: &str, path: &Path) -> Result<Vec<u8>, WorkflowError> {
    let cap: u64 = ARTIFACT_CAPS.max_file_bytes;
    if let Ok(metadata) = std::fs::metadata(path) {
        if !metadata.is_file() {
            return Err(source_missing(
                given,
                &std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file"),
            ));
        }
        if metadata.len() > cap {
            return Err(too_large(given, cap));
        }
    }
    let mut file = std::fs::File::open(path).map_err(|error| source_missing(given, &error))?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            driver_error(format!("artifact.file: failed to read '{given}': {error}"))
        })?;
    if bytes.len() as u64 > cap {
        return Err(too_large(given, cap));
    }
    Ok(bytes)
}

// ——————————————————————————————— Pure support ————————————————————————————

fn artifact_id_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"^[A-Za-z0-9_.-]+$").expect("static artifact id pattern"))
}

/// Naked MIME (`type/subtype`, no parameters).
fn content_type_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"^[A-Za-z0-9][A-Za-z0-9!#$&^_.+-]*/[A-Za-z0-9][A-Za-z0-9!#$&^_.+-]*$")
            .expect("static content-type pattern")
    })
}

/// extension → MIME. Everything outside the table is `application/octet-stream`: **No content sniffing**.
///
/// Do not reuse core's `inferAttachmentMimeFromPath`: the table service model input attachment, the bottom line is
/// `text/plain`; The consumer here is UI dispatch, and the bottom line must be "I don't know it, please download the card".
fn content_type_for_extension(extension: &str) -> Option<String> {
    let content_type = match extension {
        "csv" => "text/csv",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "gif" => "image/gif",
        "htm" | "html" => "text/html",
        "jpeg" | "jpg" => "image/jpeg",
        "json" => "application/json",
        "markdown" | "md" => "text/markdown",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "svg" => "image/svg+xml",
        "txt" => "text/plain",
        "webp" => "image/webp",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => return None,
    };
    Some(content_type.to_string())
}

/// Reverse check: When opts is overwritten to a known type, the dropped file still has a readable extension.
fn extension_for_content_type(content_type: &str) -> Option<String> {
    let extension = match content_type {
        "text/csv" => "csv",
        "text/html" => "html",
        "image/gif" => "gif",
        "image/jpeg" => "jpg",
        "application/json" => "json",
        "text/markdown" => "md",
        "application/pdf" => "pdf",
        "image/png" => "png",
        "image/svg+xml" => "svg",
        "text/plain" => "txt",
        "image/webp" => "webp",
        _ => return None,
    };
    Some(extension.to_string())
}

/// The extension of the file name (lowercase, without dot). Only recognize `[a-z0-9]+`: `Makefile`, `.gitignore` without extension.
fn file_extension(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy().to_string();
    let dot = name.rfind('.')?;
    if dot == 0 || dot + 1 >= name.len() {
        return None;
    }
    let extension = name[dot + 1..].to_ascii_lowercase();
    if extension.chars().all(|c| c.is_ascii_alphanumeric()) {
        Some(extension)
    } else {
        None
    }
}
/// RunId is folded before entering the directory name (run id is usually already safe; here it is only guaranteed not to cross directories).
fn sanitize_path_segment(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "run".to_string()
    } else {
        sanitized
    }
}

fn driver_error(message: impl Into<String>) -> WorkflowError {
    WorkflowError::new(WorkflowErrorCode::DriverError, message)
}

fn outside_workspace(given: &str) -> WorkflowError {
    WorkflowError::new(
        WorkflowErrorCode::ArtifactPathOutsideWorkspace,
        format!(
            "artifact.file: path '{given}' resolves outside the workspace (symlinks are judged by \
             their real path). Only files inside the workspace can be published."
        ),
    )
}

fn source_missing(given: &str, cause: &std::io::Error) -> WorkflowError {
    WorkflowError::new(
        WorkflowErrorCode::ArtifactSourceMissing,
        format!(
            "artifact.file: '{given}' does not exist or is not a regular file. Confirm the \
             subagent actually wrote it before publishing."
        ),
    )
    .with_cause(cause.to_string())
}

fn too_large(given: &str, cap: u64) -> WorkflowError {
    WorkflowError::new(
        WorkflowErrorCode::ArtifactTooLarge,
        format!(
            "artifact.file: '{given}' is over the cap of {cap} bytes. Publish a smaller artifact \
             (a summary, a slice, or a compressed version)."
        ),
    )
}

fn describe_arg(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_workflow::types::ArtifactOp;
    use serde_json::json;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zcode-workflow-artifacts-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("temp root");
        dir
    }

    fn request(op: ArtifactOp, id: &str) -> ArtifactPublishRequest {
        ArtifactPublishRequest {
            run_id: "dwfrun-test".to_string(),
            site_id: "artifact#1".to_string(),
            ordinal: 1,
            op,
            id: id.to_string(),
            version: 1,
            path: None,
            content: None,
            opts: None,
        }
    }

    #[tokio::test]
    async fn file_artifact_is_stored_under_the_run_directory() {
        let workspace = temp_root("file-workspace");
        let data = temp_root("file-data");
        std::fs::write(workspace.join("report.md"), "# hello").unwrap();
        let store = ArtifactStore::new(&data);
        let mut publish = request(ArtifactOp::File, "report");
        publish.path = Some("report.md".to_string());
        publish.opts = Some(json!({"title": "Report", "description": "the report"}));
        let record = store.publish(&workspace, publish).await.expect("publish");

        assert_eq!(record.kind, ArtifactOp::File);
        assert_eq!(record.content_type.as_deref(), Some("text/markdown"));
        assert_eq!(record.bytes, Some("# hello".len() as u64));
        assert_eq!(
            record.uri.as_deref(),
            Some("zcode-artifact://dwfrun-test/report")
        );
        assert_eq!(record.source_path.as_deref(), Some("report.md"));
        assert_eq!(record.title.as_deref(), Some("Report"));
        assert!(record.published_at.unwrap_or(0.0) > 0.0);
        let stored = std::fs::read(data.join("workflow-artifacts/dwfrun-test/report-v1.md"))
            .expect("stored bytes");
        assert_eq!(stored, b"# hello");
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[tokio::test]
    async fn markdown_artifact_uses_the_text_channel() {
        let workspace = temp_root("markdown-workspace");
        let data = temp_root("markdown-data");
        let store = ArtifactStore::new(&data);
        let mut publish = request(ArtifactOp::Markdown, "summary");
        publish.content = Some("## summary\n".to_string());
        publish.opts = Some(json!({"contentType": "application/pdf"}));
        let record = store.publish(&workspace, publish).await.expect("publish");
        // The markdown family does not read contentType.
        assert_eq!(record.content_type.as_deref(), Some("text/markdown"));
        assert!(record.source_path.is_none());
        assert!(data
            .join("workflow-artifacts/dwfrun-test/summary-v1.md")
            .is_file());
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[tokio::test]
    async fn escaping_paths_are_rejected_even_through_symlinks() {
        let workspace = temp_root("escape-workspace");
        let data = temp_root("escape-data");
        let outside = temp_root("escape-outside");
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        let store = ArtifactStore::new(&data);

        let mut publish = request(ArtifactOp::File, "secret");
        publish.path = Some("../escape-outside/secret.txt".to_string());
        let error = store
            .publish(&workspace, publish)
            .await
            .expect_err("escape");
        assert_eq!(error.code, WorkflowErrorCode::ArtifactPathOutsideWorkspace);

        // Symbolic link (permission is required to create a symbolic link on Windows; skip this section on failure).
        #[cfg(unix)]
        let linked = {
            std::os::unix::fs::symlink(outside.join("secret.txt"), workspace.join("link.txt"))
                .is_ok()
        };
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(
            outside.join("secret.txt"),
            workspace.join("link.txt"),
        )
        .is_ok();
        if linked {
            let mut publish = request(ArtifactOp::File, "linked");
            publish.path = Some("link.txt".to_string());
            let error = store
                .publish(&workspace, publish)
                .await
                .expect_err("symlink escape");
            assert_eq!(error.code, WorkflowErrorCode::ArtifactPathOutsideWorkspace);
        }

        let mut publish = request(ArtifactOp::File, "missing");
        publish.path = Some("missing.txt".to_string());
        let error = store
            .publish(&workspace, publish)
            .await
            .expect_err("missing");
        assert_eq!(error.code, WorkflowErrorCode::ArtifactSourceMissing);
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&data);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[tokio::test]
    async fn oversize_payloads_are_rejected_not_truncated() {
        let workspace = temp_root("large-workspace");
        let data = temp_root("large-data");
        let store = ArtifactStore::new(&data);

        let mut publish = request(ArtifactOp::Markdown, "huge");
        publish.content = Some("x".repeat(ARTIFACT_CAPS.max_markdown_bytes as usize + 1));
        let error = store
            .publish(&workspace, publish)
            .await
            .expect_err("too large");
        assert_eq!(error.code, WorkflowErrorCode::ArtifactTooLarge);

        let mut publish = request(ArtifactOp::File, "huge-file");
        publish.path = Some("huge.bin".to_string());
        let file = std::fs::File::create(workspace.join("huge.bin")).unwrap();
        file.set_len(ARTIFACT_CAPS.max_file_bytes + 1).unwrap();
        let error = store
            .publish(&workspace, publish)
            .await
            .expect_err("too large");
        assert_eq!(error.code, WorkflowErrorCode::ArtifactTooLarge);

        let mut publish = request(ArtifactOp::Markdown, "long-title");
        publish.content = Some("body".to_string());
        publish.opts = Some(json!({"title": "t".repeat(ARTIFACT_CAPS.max_title_length + 1)}));
        let error = store
            .publish(&workspace, publish)
            .await
            .expect_err("title cap");
        assert_eq!(error.code, WorkflowErrorCode::ArtifactTooLarge);

        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[tokio::test]
    async fn invalid_arguments_are_driver_errors() {
        let workspace = temp_root("args-workspace");
        let data = temp_root("args-data");
        let store = ArtifactStore::new(&data);

        let mut publish = request(ArtifactOp::File, "../escape");
        publish.path = Some("a.txt".to_string());
        assert_eq!(
            store.publish(&workspace, publish).await.unwrap_err().code,
            WorkflowErrorCode::DriverError
        );

        let mut publish = request(ArtifactOp::File, "ok");
        publish.path = Some("a.txt".to_string());
        publish.opts = Some(json!({"contentType": "text/markdown; charset=utf-8"}));
        assert_eq!(
            store.publish(&workspace, publish).await.unwrap_err().code,
            WorkflowErrorCode::DriverError
        );

        let mut publish = request(ArtifactOp::File, "ok");
        publish.path = Some("a.txt".to_string());
        publish.opts = Some(json!("not an object"));
        assert_eq!(
            store.publish(&workspace, publish).await.unwrap_err().code,
            WorkflowErrorCode::DriverError
        );
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn extension_mapping_matches_the_ui_dispatch_table() {
        assert_eq!(
            content_type_for_extension("md").as_deref(),
            Some("text/markdown")
        );
        assert_eq!(
            content_type_for_extension("pdf").as_deref(),
            Some("application/pdf")
        );
        assert_eq!(content_type_for_extension("tar"), None);
        assert_eq!(
            file_extension(Path::new("archive.tar.gz")).as_deref(),
            Some("gz")
        );
        assert_eq!(file_extension(Path::new("Makefile")), None);
        assert_eq!(file_extension(Path::new(".gitignore")), None);
    }
}
