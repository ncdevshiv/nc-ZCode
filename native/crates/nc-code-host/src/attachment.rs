//! The v4 attachment subsystem: chunked upload transaction + content-addressed store + row/index authorized reads + media preview.
//!
//! What is ported is the **local** attachment path of TS `apps/nc-code-cli` (not the
//! "prompt-attachment transfer / conversation-share remote upload" surface that was removed with the claim boundary):
//!
//! - The upload transaction `begin|chunk|commit|abort`: mirrors the
//!   staging/committed state machine of `attachment-upload-registry.ts`—decode and account chunk by chunk, validate chunk order and byte total, and only land the artifact after commit validates the
//!   checksum. **The authority fact only produces a ref when the complete bytes plus the checksum hit.**
//! - Storage: content-addressed (named by the `sha256` digest), under `<data-dir>/sessions/<id>/attachments/`,
//!   sharing the session's lifecycle and restorable across restarts; ref = `nc-code-artifact://<hex>`, resolved against the session directory
//!   on read-back, which rules out treating any client-supplied string as a path (the same discipline as `attachmentRef.ts`'s comment).
//! - Read-back `read` (media preview image/video/pdf) and `previewSource` (the desktop local video source path):
//!   first the current session's authoritative userInput rows prove the ref's ownership by `target.rowId + entityId + attachmentIndex`,
//!   and only then are the bytes moved—
//!   the same authorization chain as `resolveReadableMediaAttachment` in `v4-gateway.ts`.
//!
//! Differences from TS recorded on file:
//! - Only the `nc-code-artifact://` references produced by commit are served; uncommitted bare filesystem paths do not enter this
//!   host's byte moving (the desktop upload path is begin/chunk/commit, which already covers the product's main path).
//! - The upload registry is reclaimed lazily by TTL (every transaction entry prunes first); there is no cross-process connection-level
//!   `clearConnection` hook (stdio is a single process with very few connections).
//! - The read cache's TTL/capacity reclamation (TS `binaryReadCache`) is not ported: every read here slices a chunk straight out of
//!   the content-addressed file, and under the 512 KiB cap a single read is cheap, so no second in-memory copy is introduced
//!   (avoiding a duplicated state owner).

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use nc_code_v4_wire::decode_wire_base64;
use serde_json::{json, Value};

use crate::content_hash::sha256_hex;
use crate::rpc::{RpcError, CODE_INVALID_PARAMS};
use crate::session::Session;
use crate::store::now_ms;
use crate::{session_not_found, HostServer};

// ── Protocol limits (aligned word for word with the attachment-related entries of
// `PROTOCOL_V4_LIMITS` in `packages/shared/src/nc-code-protocol-v4/core.ts`). Defined in one place, shared by transaction validation and the preview cap. ──

pub const ATTACHMENT_MAX_BYTES: u64 = 20 * 1024 * 1024;
pub const ATTACHMENT_CHUNK_MAX_BYTES: usize = 512 * 1024;
pub const ATTACHMENT_UPLOAD_MAX_CHUNKS: u64 = 64;
pub const ATTACHMENT_UPLOAD_MAX_CONCURRENT: usize = 16;
pub const ATTACHMENT_UPLOAD_MAX_STAGED_BYTES: usize = 64 * 1024 * 1024;
pub const ATTACHMENT_UPLOAD_TTL_MS: i64 = 5 * 60_000;
/// The fault prefix of the upload transaction (the same namespace as `fault.attachment.*` in `attachment-faults.ts`).
const FAULT: &str = "fault.attachment";

fn fault(code: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(CODE_INVALID_PARAMS, message)
        .with_data(json!({ "reasonCode": format!("{FAULT}.{code}") }))
}

/// The stable shape of a ref: `nc-code-artifact://<64 lowercase hexadecimal sha256>`.
const REF_PREFIX: &str = "zcode-artifact://";

fn ref_hex(ref_value: &str) -> Option<&str> {
    let hex = ref_value.strip_prefix(REF_PREFIX)?;
    if hex.len() == 64
        && hex
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        Some(hex)
    } else {
        None
    }
}

/// The MIMEs allowed for a media preview (the three families image/video/pdf, with the same
/// predicate as `isPreviewable` in `v4-gateway.ts`: strip parameters, lowercase, then compare prefixes).
fn base_mime(mime: &str) -> String {
    mime.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

fn is_preview_media(mime: &str) -> bool {
    let base = base_mime(mime);
    base.starts_with("image/") || base.starts_with("video/") || base == "application/pdf"
}

// ── Content-addressed store ──

/// The session-level content-addressed attachment store: `<root>/<session-id>/attachments/<hex>`.
///
/// The only write entry is [`AttachmentStore::write`] (atomically landed after a successful commit); the read side has
/// only `read`/`stat`/`path_for`, all of which validate the digest shape through `ref_hex` first, ruling out directory traversal.
pub struct AttachmentStore {
    root: PathBuf,
}

impl AttachmentStore {
    /// `root` = `<data-dir>/sessions` (the same root as `TranscriptStore`, so session directories are reused).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.root.join(session_id).join("attachments")
    }

    fn file_path(&self, session_id: &str, hex: &str) -> PathBuf {
        self.session_dir(session_id).join(hex)
    }

    /// Land the complete bytes and return the ref. When the file already exists (content addressing dedupes naturally) it is reused directly.
    pub fn write(&self, session_id: &str, bytes: &[u8]) -> Result<String, RpcError> {
        let hex = sha256_hex(bytes);
        let dir = self.session_dir(session_id);
        fs::create_dir_all(&dir).map_err(|error| {
            fault(
                "writeFailed",
                format!("create attachment dir failed: {error}"),
            )
        })?;
        let final_path = dir.join(&hex);
        if !final_path.exists() {
            let tmp = dir.join(format!("{hex}.tmp-{}", std::process::id()));
            let mut file = fs::File::create(&tmp)
                .map_err(|error| fault("writeFailed", format!("create temp failed: {error}")))?;
            file.write_all(bytes)
                .map_err(|error| fault("writeFailed", format!("write temp failed: {error}")))?;
            file.sync_all().ok();
            drop(file);
            fs::rename(&tmp, &final_path).map_err(|error| {
                let _ = fs::remove_file(&tmp);
                fault("writeFailed", format!("rename failed: {error}"))
            })?;
        }
        Ok(format!("{REF_PREFIX}{hex}"))
    }

    fn resolved(&self, session_id: &str, ref_value: &str) -> Result<PathBuf, RpcError> {
        let hex = ref_hex(ref_value).ok_or_else(|| {
            fault(
                "refMalformed",
                "attachment ref is not a committed content-addressed reference",
            )
        })?;
        Ok(self.file_path(session_id, hex))
    }

    pub fn read(&self, session_id: &str, ref_value: &str) -> Result<Vec<u8>, RpcError> {
        let path = self.resolved(session_id, ref_value)?;
        match fs::read(&path) {
            Ok(bytes) => Ok(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(fault(
                "statNotFound",
                "attachment bytes are not present in this session's store",
            )),
            Err(error) => Err(fault("readFailed", format!("read attachment: {error}"))),
        }
    }

    /// Read one attachment for the model context: a committed content-addressed ref
    /// (`zcode-artifact://<sha256>`) resolves inside this session's store, while a plain
    /// absolute path resolves as a local file. The UI sends every desktop attachment as
    /// `ref = localPath` (zero upload, `uploadComposerAttachment`), so refusing those here
    /// would strand each image/file the user attaches on the desktop.
    ///
    /// Only an absolute, readable path is honoured; a relative ref is still a malformed
    /// reference (it cannot be attributed to a session store key).
    pub fn read_for_model(&self, session_id: &str, ref_value: &str) -> Result<Vec<u8>, RpcError> {
        if ref_hex(ref_value).is_some() {
            return self.read(session_id, ref_value);
        }
        let path = Path::new(ref_value);
        if !path.is_absolute() || path.as_os_str().is_empty() {
            return Err(fault(
                "refMalformed",
                "attachment is neither a committed ref nor an absolute local path",
            ));
        }
        match fs::read(path) {
            Ok(bytes) => Ok(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(fault(
                "statNotFound",
                "local attachment path is not readable",
            )),
            Err(error) => Err(fault(
                "readFailed",
                format!("read local attachment: {error}"),
            )),
        }
    }

    /// The preview source path: the absolute path is offered only when the file really exists (desktop local video playback).
    pub fn path_for(&self, session_id: &str, ref_value: &str) -> Option<PathBuf> {
        let path = self.resolved(session_id, ref_value).ok()?;
        path.exists().then_some(path.canonicalize().unwrap_or(path))
    }

    /// Metadata-only stat for the share surface: byte length + mtime (ms)
    /// without loading the full bytes into the response.
    pub fn stat(&self, session_id: &str, ref_value: &str) -> Result<(u64, Option<u64>), RpcError> {
        let path = self.resolved(session_id, ref_value)?;
        let metadata = match fs::metadata(&path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(fault(
                    "statNotFound",
                    "attachment bytes are not present in this session's store",
                ))
            }
            Err(error) => return Err(fault("readFailed", format!("stat attachment: {error}"))),
        };
        let mtime_ms = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64);
        Ok((metadata.len(), mtime_ms))
    }
}

// ── Attachments → model content parts (specs/rust-native-crates.md S3 "attachments into the model context") ──
/// Mirrors `contracts/src/tools/read.ts`: READ_MAX_FILE_SIZE_BYTES / READ_DEFAULT_MAX_LINES.
const ATTACHMENT_TEXT_MAX_BYTES: usize = 256 * 1024;
const ATTACHMENT_TEXT_MAX_LINES: usize = 2_000;

/// The set of texts that may be inlined into the prompt according to the declared mime. TS sniffs by **path extension**
/// on the CLI side (`isTextLikePath`) because its input carries bare paths; v4's `attachmentRefSchema` mandates a
/// `mime`, so only the mime is consulted here and no second sniffing scheme is added.
fn is_text_like_mime(mime: &str) -> bool {
    let mime = mime.to_ascii_lowercase();
    mime.starts_with("text/")
        || matches!(
            mime.as_str(),
            "application/json"
                | "application/x-json"
                | "application/jsonld"
                | "application/javascript"
                | "application/typescript"
                | "application/x-yaml"
                | "application/yaml"
                | "application/toml"
                | "application/xml"
                | "application/sql"
                | "application/x-sh"
                | "application/x-shellscript"
                | "application/x-httpd-php"
        )
}

/// The two branches of `formatPathReferenceReason` (TS) that this host actually uses.
fn path_reference_reason_text(reason: &str) -> &'static str {
    match reason {
        "text_too_large" => "the text file is larger than the inline text budget",
        "image_too_large" => "the image is larger than the inline media budget",
        "deferred_clipboard_text" => {
            "it is a pasted-text temporary attachment that is deferred to keep the model context small"
        }
        // Unknown/other: TS's fallback wording (binary and non-inlinable media both take this branch).
        _ => "the file is not a known text attachment",
    }
}

/// The path-reference part (the three-line text shape of TS `resolvedPathReferenceAttachment`, aligned word for word).
fn path_reference_part(
    mime: &str,
    placeholder: &str,
    reason: &str,
) -> nc_code_provider::request::ContentPart {
    nc_code_provider::request::ContentPart::Text {
        text: [
            format!("Attached {mime}: {placeholder}"),
            format!(
                "The file was sent by local path because {}.",
                path_reference_reason_text(reason)
            ),
            "Use the available file reading tools if you need to inspect the file contents."
                .to_string(),
        ]
        .join("\n"),
    }
}

/// The placeholder part (TS `resolvedPlaceholderAttachment`'s `[Attached <mime>: <placeholder>]`).
fn placeholder_part(mime: &str, placeholder: &str) -> nc_code_provider::request::ContentPart {
    nc_code_provider::request::ContentPart::Text {
        text: format!("[Attached {mime}: {placeholder}]"),
    }
}

/// The body of a resolved text part: over the cap take the first N lines (same as TS: the truncation detail goes into
/// preview metadata, never as a hint appended to the model's text).
fn inline_text_body(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if bytes.len() <= ATTACHMENT_TEXT_MAX_BYTES {
        return text.into_owned();
    }
    text.lines()
        .take(ATTACHMENT_TEXT_MAX_LINES)
        .collect::<Vec<&str>>()
        .join("\n")
}

/// Resolve the payload's ref attachments into model content parts (order preserved, failures never silently dropped).
pub fn model_parts_for_attachments(
    store: &AttachmentStore,
    session_id: &str,
    attachments: &[Value],
) -> Vec<nc_code_provider::request::ContentPart> {
    let mut parts = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        let Some(ref_value) = attachment.get("ref").and_then(Value::as_str) else {
            // The row projection layer (projectable_attachment_refs) would not accept this one either; a voice is kept here
            // because the model must know "the user attached something but it did not enter the context", otherwise it assumes the attachment arrived.
            parts.push(placeholder_part("text/plain", "attachment_unresolved"));
            continue;
        };
        let file_name = attachment
            .get("fileName")
            .and_then(Value::as_str)
            .unwrap_or(ref_value);
        let mime = attachment
            .get("mime")
            .and_then(Value::as_str)
            .unwrap_or("application/octet-stream");
        let bytes = match store.read_for_model(session_id, ref_value) {
            Ok(bytes) => bytes,
            Err(_) => {
                parts.push(placeholder_part(mime, "attachment_read_failed"));
                continue;
            }
        };
        if mime.to_ascii_lowercase().starts_with("image/") {
            parts.push(nc_code_provider::request::ContentPart::Image {
                media_type: mime.to_string(),
                data: crate::tools::read_image::base64_encode(&bytes),
            });
            continue;
        }
        if is_text_like_mime(mime) {
            match String::from_utf8(bytes.clone()) {
                Ok(text) => parts.push(nc_code_provider::request::ContentPart::Text {
                    text: inline_text_body(text.as_bytes()),
                }),
                // Declared as text but the bytes are not UTF-8: treat it as binary, never feed a lossy result to the model as file content.
                Err(_) => parts.push(path_reference_part(mime, file_name, "binary_file")),
            }
            continue;
        }
        // pdf/video/unknown binary: TS also hands only a path reference and lets the model fetch it with the tools.
        parts.push(path_reference_part(mime, file_name, "unsupported_media"));
    }
    parts
}

// ── The upload transaction state machine ──

#[derive(Clone, Debug)]
struct UploadMeta {
    file_name: String,
    mime: String,
    total_bytes: u64,
    total_chunks: u64,
    checksum: String,
}

struct Staged {
    meta: UploadMeta,
    chunks: Vec<Vec<u8>>,
    received_bytes: u64,
    expires_at: i64,
}

struct Committed {
    next_chunk_index: u64,
    ref_value: String,
    meta: UploadMeta,
    expires_at: i64,
}

fn upload_key(connection_id: &str, session_id: &str, upload_id: &str) -> String {
    format!("{connection_id}\u{0}{session_id}\u{0}{upload_id}")
}

/// The in-process upload authority table (host level, one instance per host). staging lives in memory only; commit is what lands a content-addressed file.
pub struct UploadRegistry {
    staged: HashMap<String, Staged>,
    committed: HashMap<String, Committed>,
    staged_bytes: usize,
}

impl UploadRegistry {
    pub fn new() -> Self {
        Self {
            staged: HashMap::new(),
            committed: HashMap::new(),
            staged_bytes: 0,
        }
    }

    fn prune_expired(&mut self, now: i64) {
        let expired: Vec<String> = self
            .staged
            .iter()
            .filter(|(_, upload)| upload.expires_at <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for key in expired {
            self.drop_staged(&key);
        }
        self.committed.retain(|_, upload| upload.expires_at > now);
    }

    fn drop_staged(&mut self, key: &str) {
        if let Some(upload) = self.staged.remove(key) {
            self.staged_bytes = self
                .staged_bytes
                .saturating_sub(upload.received_bytes as usize);
        }
    }

    /// begin: registers metadata only, it does not stage a payload. Idempotent (re-submitting the same metadata returns the same state).
    pub fn begin(&mut self, params: &Value, now: i64) -> Result<Value, RpcError> {
        self.prune_expired(now);
        let connection_id = require_str(params, "connectionId")?;
        let session_id = require_session_id(params)?;
        let upload_id = require_upload_id(params)?;
        let meta = read_upload_meta(params)?;
        let key = upload_key(connection_id, session_id, upload_id);

        if let Some(committed) = self.committed.get(&key) {
            if !same_meta(&committed.meta, &meta) {
                return Err(fault("beginConflict", "committed upload metadata mismatch"));
            }
            return Ok(json!({
                "uploadId": upload_id,
                "state": "committed",
                "nextChunkIndex": committed.next_chunk_index,
                "ref": committed.ref_value,
            }));
        }
        if let Some(existing) = self.staged.get_mut(&key) {
            if !same_meta(&existing.meta, &meta) {
                return Err(fault("beginConflict", "staged upload metadata mismatch"));
            }
            existing.expires_at = now + ATTACHMENT_UPLOAD_TTL_MS;
            return Ok(json!({
                "uploadId": upload_id,
                "state": "staging",
                "nextChunkIndex": existing.chunks.len(),
            }));
        }
        if self.staged.len() >= ATTACHMENT_UPLOAD_MAX_CONCURRENT {
            return Err(fault("tooManyUploads", "concurrent upload limit reached"));
        }
        if meta.total_chunks > 0
            && meta.total_bytes > meta.total_chunks * ATTACHMENT_CHUNK_MAX_BYTES as u64
        {
            return Err(fault(
                "chunkCountInsufficient",
                "declared chunk count cannot carry the declared byte total",
            ));
        }
        self.staged.insert(
            key,
            Staged {
                meta,
                chunks: Vec::new(),
                received_bytes: 0,
                expires_at: now + ATTACHMENT_UPLOAD_TTL_MS,
            },
        );
        Ok(json!({
            "uploadId": upload_id,
            "state": "staging",
            "nextChunkIndex": 0,
        }))
    }

    /// chunk: decode one piece and append it. chunkIndex must be consecutive, in range, never conflict on a repeat, and never empty.
    pub fn chunk(&mut self, params: &Value, now: i64) -> Result<Value, RpcError> {
        self.prune_expired(now);
        let connection_id = require_str(params, "connectionId")?;
        let session_id = require_session_id(params)?;
        let upload_id = require_upload_id(params)?;
        let key = upload_key(connection_id, session_id, upload_id);
        let chunk_index = params
            .get("chunkIndex")
            .and_then(Value::as_u64)
            .filter(|value| *value <= u64::from(u32::MAX))
            .ok_or_else(|| fault("invalidChunkIndex", "chunkIndex: required non-negative int"))?;
        let data_base64 = params
            .get("dataBase64")
            .and_then(Value::as_str)
            .ok_or_else(|| fault("invalidChunk", "dataBase64: required string"))?;

        let upload = self
            .staged
            .get_mut(&key)
            .ok_or_else(|| fault("uploadNotFound", "no staged upload for this key"))?;
        // A zero-length base64 = empty bytes; anything else must decode.
        let bytes = if data_base64.is_empty() {
            Vec::new()
        } else {
            decode_wire_base64(data_base64)
                .ok_or_else(|| fault("invalidBase64", "chunk data is not valid base64"))?
        };
        if bytes.len() > ATTACHMENT_CHUNK_MAX_BYTES {
            return Err(fault(
                "chunkTooLarge",
                "chunk exceeds the decoded byte limit",
            ));
        }
        if (chunk_index as usize) < upload.chunks.len() {
            if upload.chunks[chunk_index as usize] != bytes {
                return Err(fault(
                    "chunkConflict",
                    "re-sent chunk differs from staged bytes",
                ));
            }
            return Ok(json!({
                "uploadId": upload_id,
                "nextChunkIndex": upload.chunks.len(),
            }));
        }
        if chunk_index as usize > upload.chunks.len() {
            return Err(fault(
                "chunkGap",
                "chunk index leaves a gap in the sequence",
            ));
        }
        if chunk_index >= upload.meta.total_chunks {
            return Err(fault(
                "tooManyChunks",
                "chunk index exceeds declared totalChunks",
            ));
        }
        if bytes.is_empty() {
            return Err(fault("emptyChunk", "a non-zero chunk must carry bytes"));
        }
        if upload.received_bytes + bytes.len() as u64 > upload.meta.total_bytes {
            return Err(fault(
                "totalBytesExceeded",
                "received bytes exceed declared total",
            ));
        }
        if self.staged_bytes + bytes.len() > ATTACHMENT_UPLOAD_MAX_STAGED_BYTES {
            return Err(fault(
                "stagingCapacityExceeded",
                "process-wide staging capacity exceeded",
            ));
        }
        upload.chunks.push(bytes.clone());
        upload.received_bytes += bytes.len() as u64;
        upload.expires_at = now + ATTACHMENT_UPLOAD_TTL_MS;
        self.staged_bytes += bytes.len();
        Ok(json!({
            "uploadId": upload_id,
            "nextChunkIndex": upload.chunks.len(),
        }))
    }

    /// commit: validate completeness + checksum; only on a hit does it land the content-addressed file and turn committed.
    pub fn commit(
        &mut self,
        store: &AttachmentStore,
        params: &Value,
        now: i64,
    ) -> Result<Value, RpcError> {
        self.prune_expired(now);
        let connection_id = require_str(params, "connectionId")?;
        let session_id = require_session_id(params)?;
        let upload_id = require_upload_id(params)?;
        let key = upload_key(connection_id, session_id, upload_id);

        if let Some(committed) = self.committed.get(&key) {
            return Ok(json!({ "ref": committed.ref_value }));
        }
        // Borrow the immutable upload to compute everything needed for the write, then end the borrow before touching the table:
        // `store` is a separate argument (the handler passes self.attachments), so it does not conflict with self.
        let (assembled, meta, next_chunk_index) = {
            let upload = self
                .staged
                .get(&key)
                .ok_or_else(|| fault("uploadNotFound", "no staged upload to commit"))?;
            if upload.chunks.len() as u64 != upload.meta.total_chunks
                || upload.received_bytes != upload.meta.total_bytes
            {
                return Err(fault(
                    "uploadIncomplete",
                    "commit rejected: chunks or bytes incomplete",
                ));
            }
            let mut assembled = Vec::with_capacity(upload.meta.total_bytes as usize);
            for chunk in &upload.chunks {
                assembled.extend_from_slice(chunk);
            }
            let digest = format!("sha256:{}", sha256_hex(&assembled));
            if digest != upload.meta.checksum {
                return Err(fault(
                    "checksumMismatch",
                    "commit rejected: sha256 of received bytes does not match declared checksum",
                ));
            }
            (assembled, upload.meta.clone(), upload.chunks.len() as u64)
        };
        let ref_value = store.write(session_id, &assembled)?;
        // The table moves only after the write succeeded: staging is released (including the staged_bytes accounting) and committed is created.
        self.drop_staged(&key);
        self.committed.insert(
            key,
            Committed {
                next_chunk_index,
                ref_value: ref_value.clone(),
                meta,
                expires_at: now + ATTACHMENT_UPLOAD_TTL_MS,
            },
        );
        Ok(json!({ "ref": ref_value }))
    }

    /// abort: discard staging (committed is untouched; content-addressed bytes already on disk live as long as the session).
    pub fn abort(&mut self, params: &Value, now: i64) -> Result<(), RpcError> {
        self.prune_expired(now);
        let connection_id = require_str(params, "connectionId")?;
        let session_id = require_session_id(params)?;
        let upload_id = require_upload_id(params)?;
        let key = upload_key(connection_id, session_id, upload_id);
        self.drop_staged(&key);
        Ok(())
    }
}

impl Default for UploadRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn same_meta(left: &UploadMeta, right: &UploadMeta) -> bool {
    left.file_name == right.file_name
        && left.mime == right.mime
        && left.total_bytes == right.total_bytes
        && left.total_chunks == right.total_chunks
        && left.checksum == right.checksum
}

// ── Small parameter-validation helpers ──

fn require_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            RpcError::new(
                CODE_INVALID_PARAMS,
                format!("params.{key}: required string"),
            )
        })
}

fn require_session_id(params: &Value) -> Result<&str, RpcError> {
    let session_id = require_str(params, "sessionId")?;
    if !crate::store::is_safe_session_id(session_id) {
        return Err(RpcError::new(
            CODE_INVALID_PARAMS,
            "params.sessionId: unsafe session id",
        ));
    }
    Ok(session_id)
}

fn require_upload_id(params: &Value) -> Result<&str, RpcError> {
    let upload_id = require_str(params, "uploadId")?;
    let ok = upload_id.len() <= 128
        && upload_id
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && upload_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'));
    if !ok {
        return Err(fault("invalidUploadId", "uploadId has an illegal shape"));
    }
    Ok(upload_id)
}

fn read_upload_meta(params: &Value) -> Result<UploadMeta, RpcError> {
    let file_name = require_str(params, "fileName")?;
    if file_name.len() > 255 || file_name.chars().any(|c| matches!(c, '\0' | '\r' | '\n')) {
        return Err(fault("invalidFileName", "fileName has an illegal shape"));
    }
    let mime = require_str(params, "mime")?;
    if !is_valid_mime(mime) {
        return Err(fault("invalidMime", "mime has an illegal shape"));
    }
    let total_bytes = params
        .get("totalBytes")
        .and_then(Value::as_u64)
        .ok_or_else(|| fault("invalidTotalBytes", "totalBytes: required non-negative int"))?;
    if total_bytes > ATTACHMENT_MAX_BYTES {
        return Err(fault(
            "totalTooLarge",
            "totalBytes exceeds the attachment byte cap",
        ));
    }
    let total_chunks = params
        .get("totalChunks")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            fault(
                "invalidTotalChunks",
                "totalChunks: required non-negative int",
            )
        })?;
    if total_chunks > ATTACHMENT_UPLOAD_MAX_CHUNKS {
        return Err(fault(
            "tooManyChunks",
            "totalChunks exceeds the upload chunk cap",
        ));
    }
    if (total_bytes == 0) != (total_chunks == 0) {
        return Err(fault(
            "zeroByteChunkCount",
            "a zero-byte upload must declare zero chunks",
        ));
    }
    let checksum = require_str(params, "checksum")?;
    if !is_valid_checksum(checksum) {
        return Err(fault(
            "invalidChecksum",
            "checksum must be `sha256:<64 hex>`",
        ));
    }
    Ok(UploadMeta {
        file_name: file_name.to_string(),
        mime: mime.to_string(),
        total_bytes,
        total_chunks,
        checksum: checksum.to_string(),
    })
}

fn is_valid_mime(mime: &str) -> bool {
    let bytes = mime.as_bytes();
    if bytes.len() < 3 || bytes.len() > 255 {
        return false;
    }
    let Some((type_part, sub_part)) = mime.split_once('/') else {
        return false;
    };
    let token_ok = |token: &str| {
        !token.is_empty()
            && token.len() <= 128
            && token.chars().enumerate().all(|(index, c)| {
                if index == 0 {
                    c.is_ascii_alphanumeric()
                } else {
                    c.is_ascii_alphanumeric()
                        || matches!(c, '!' | '#' | '$' | '&' | '^' | '_' | '.' | '+' | '-')
                }
            })
    };
    token_ok(type_part) && token_ok(sub_part)
}

fn is_valid_checksum(checksum: &str) -> bool {
    let Some(hex) = checksum.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64
        && hex
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn attachment_matches(attachment: &Value, ref_value: &str) -> bool {
    let matches_ref =
        |field: &str| attachment.get(field).and_then(Value::as_str) == Some(ref_value);
    matches_ref("ref") || matches_ref("previewRef")
}

fn attachment_mime(attachment: &Value) -> Option<String> {
    attachment
        .get("mime")
        .and_then(Value::as_str)
        .map(base_mime)
        .filter(|mime| !mime.is_empty())
}

/// Locate one preview attachment exactly inside the userInput rows by target.rowId + entityId + attachmentIndex,
/// returning that attachment's media MIME on a hit; None when there is no hit (caller → `previewRefNotAuthorized`).
fn resolve_readable_media(
    session: &Session,
    ref_value: &str,
    target: Option<&Value>,
    attachment_index: Option<u64>,
) -> Option<String> {
    let rows = session.rows_snapshot();
    if let (Some(target), Some(index)) = (target, attachment_index) {
        let row_id = target.get("rowId").and_then(Value::as_u64)?;
        let entity = target.get("entityId").and_then(Value::as_str)?;
        let row = rows.iter().find(|row| {
            row.get("rowId").and_then(Value::as_u64) == Some(row_id)
                && row.get("entityId").and_then(Value::as_str) == Some(entity)
                && row.get("kind").and_then(Value::as_str) == Some("userInput")
        })?;
        let attachment = row
            .get("attachments")
            .and_then(Value::as_array)
            .and_then(|items| items.get(index as usize))?;
        let mime = attachment_mime(attachment)?;
        if !is_preview_media(&mime) || !attachment_matches(attachment, ref_value) {
            return None;
        }
        return Some(mime);
    }
    // No target: only durable preview references pass (previewRef ?? ref); a mutable original path is never accepted.
    for row in &rows {
        if row.get("kind").and_then(Value::as_str) != Some("userInput") {
            continue;
        }
        let Some(items) = row.get("attachments").and_then(Value::as_array) else {
            continue;
        };
        for attachment in items {
            let durable = attachment
                .get("previewRef")
                .and_then(Value::as_str)
                .or_else(|| attachment.get("ref").and_then(Value::as_str));
            if durable != Some(ref_value) {
                continue;
            }
            let Some(mime) = attachment_mime(attachment) else {
                continue;
            };
            if is_preview_media(&mime) {
                return Some(mime);
            }
        }
    }
    None
}

/// Normalize the `attachments` (`AttachmentRef[]`) inside a `sendText` / `editUserQuery` payload.
///
/// Only correctly shaped items survive (`ref`/`fileName`/`mime` non-empty strings, `bytes` a non-negative integer,
/// `previewRef` an optional string), reshaped into the field set the userInput row projects. An illegal item is **dropped**
/// rather than rejecting the whole command—TS already validates at the command schema layer with zod, and this host's field-targeted validation
/// sits in the same tier as `require_prompt_input`; a valid body must not be rejected wholesale because of one bad attachment.
pub(crate) fn projectable_attachment_refs(payload: &Value) -> Vec<Value> {
    let Some(items) = payload.get("attachments").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let ref_value = item.get("ref").and_then(Value::as_str)?;
            let file_name = item.get("fileName").and_then(Value::as_str)?;
            let mime = item.get("mime").and_then(Value::as_str)?;
            let bytes = item.get("bytes").and_then(Value::as_u64)?;
            if ref_value.is_empty() || file_name.is_empty() || mime.is_empty() {
                return None;
            }
            let mut projected = json!({
                "ref": ref_value,
                "fileName": file_name,
                "mime": mime,
                "bytes": bytes,
            });
            if let Some(preview_ref) = item.get("previewRef").and_then(Value::as_str) {
                if !preview_ref.is_empty() {
                    projected["previewRef"] = json!(preview_ref);
                }
            }
            Some(projected)
        })
        .collect()
}

impl HostServer {
    // ── The upload transaction's RPC surface ──

    pub(crate) fn attachment_begin(&self, params: &Value) -> Result<Value, RpcError> {
        let mut registry = self
            .upload_registry
            .lock()
            .map_err(|_| RpcError::new(CODE_INVALID_PARAMS, "upload registry lock poisoned"))?;
        registry.begin(params, now_ms())
    }

    pub(crate) fn attachment_chunk(&self, params: &Value) -> Result<Value, RpcError> {
        let mut registry = self
            .upload_registry
            .lock()
            .map_err(|_| RpcError::new(CODE_INVALID_PARAMS, "upload registry lock poisoned"))?;
        registry.chunk(params, now_ms())
    }

    pub(crate) fn attachment_commit(&self, params: &Value) -> Result<Value, RpcError> {
        let mut registry = self
            .upload_registry
            .lock()
            .map_err(|_| RpcError::new(CODE_INVALID_PARAMS, "upload registry lock poisoned"))?;
        registry.commit(&self.attachments, params, now_ms())
    }

    pub(crate) fn attachment_abort(&self, params: &Value) -> Result<Value, RpcError> {
        let mut registry = self
            .upload_registry
            .lock()
            .map_err(|_| RpcError::new(CODE_INVALID_PARAMS, "upload registry lock poisoned"))?;
        registry.abort(params, now_ms())?;
        Ok(json!({}))
    }

    // ── Media preview read-back ──

    pub(crate) fn attachment_read(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = require_session_id(params)?;
        let session = self
            .load_session(session_id)
            .ok_or_else(|| session_not_found(session_id))?;
        let ref_value = require_str(params, "ref")?;
        let offset = params
            .get("offset")
            .and_then(Value::as_u64)
            .ok_or_else(|| RpcError::new(CODE_INVALID_PARAMS, "params.offset: required"))?;
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .filter(|value| *value >= 1 && *value <= ATTACHMENT_CHUNK_MAX_BYTES as u64)
            .ok_or_else(|| RpcError::new(CODE_INVALID_PARAMS, "params.limit: out of range"))?;
        let target = params.get("target");
        let attachment_index = params.get("attachmentIndex").and_then(Value::as_u64);
        // target and attachmentIndex must come as a pair (the same discipline as the schema's superRefine).
        if target.is_some() != attachment_index.is_some() {
            return Err(RpcError::new(
                CODE_INVALID_PARAMS,
                "target and attachmentIndex must be provided together",
            ));
        }
        let media_type = resolve_readable_media(&session, ref_value, target, attachment_index)
            .ok_or_else(|| {
                fault(
                    "previewRefNotAuthorized",
                    "attachment ref is not in this session's projection",
                )
            })?;

        let bytes = self.attachments.read(session_id, ref_value)?;
        let total = bytes.len();
        let offset = usize::try_from(offset)
            .map_err(|_| fault("previewRangeInvalid", "offset too large"))?;
        if offset > total {
            return Err(fault(
                "previewRangeInvalid",
                "offset exceeds attachment size",
            ));
        }
        let end = total.min(offset + limit as usize);
        let chunk = &bytes[offset..end];
        Ok(json!({
            "dataBase64": crate::tools::read_image::base64_encode(chunk),
            "mediaType": media_type,
            "totalBytes": total,
            "nextOffset": if end < total { json!(end) } else { Value::Null },
        }))
    }

    /// `previewSource`: a desktop local video gets the file's absolute path for direct reading; everything else (remote/Web/PDF/image included)
    /// always answers `{kind:"chunked"}` and `read` moves the bytes in chunks.
    pub(crate) fn attachment_preview_source(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = require_session_id(params)?;
        let session = self
            .load_session(session_id)
            .ok_or_else(|| session_not_found(session_id))?;
        let ref_value = require_str(params, "ref")?;
        let client_mode = require_str(params, "clientMode")?;
        if client_mode != "desktop-continuous" && client_mode != "web-remote-replayable" {
            return Err(RpcError::new(
                CODE_INVALID_PARAMS,
                format!("unknown clientMode: {client_mode}"),
            ));
        }
        let target = params.get("target");
        let attachment_index = params.get("attachmentIndex").and_then(Value::as_u64);
        if target.is_some() != attachment_index.is_some() {
            return Err(RpcError::new(
                CODE_INVALID_PARAMS,
                "target and attachmentIndex must be provided together",
            ));
        }
        let media_type = resolve_readable_media(&session, ref_value, target, attachment_index)
            .ok_or_else(|| {
                fault(
                    "previewRefNotAuthorized",
                    "attachment ref is not in this session's projection",
                )
            })?;
        if client_mode != "desktop-continuous" || !media_type.starts_with("video/") {
            return Ok(json!({ "kind": "chunked" }));
        }
        match self.attachments.path_for(session_id, ref_value) {
            Some(path) => Ok(json!({
                "kind": "local_path",
                "path": path.to_string_lossy(),
                "mediaType": media_type,
            })),
            None => Ok(json!({ "kind": "chunked" })),
        }
    }
}

// ── Unit tests: the upload transaction state machine + content addressing + authorization ──

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zcode-host-attachment-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp root");
        dir
    }

    fn begin_params(upload_id: &str, bytes: &[u8]) -> Value {
        json!({
            "connectionId": "conn-1",
            "sessionId": "sess-1",
            "uploadId": upload_id,
            "fileName": "photo.png",
            "mime": "image/png",
            "totalBytes": bytes.len(),
            "totalChunks": 1,
            "checksum": format!("sha256:{}", sha256_hex(bytes)),
        })
    }

    #[test]
    fn store_round_trips_content_addressed_bytes() {
        let root = temp_root("store");
        let store = AttachmentStore::new(&root);
        let bytes = b"the quick brown fox".to_vec();
        let ref_value = store.write("sess-1", &bytes).expect("write");
        assert!(ref_value.starts_with(REF_PREFIX));
        assert_eq!(store.read("sess-1", &ref_value).expect("read"), bytes);
        // Writing the same content again → the same ref (content addressing dedupes naturally).
        assert_eq!(store.write("sess-1", &bytes).expect("rewrite"), ref_value);
        // Cross-session isolation: another session cannot read it (the file is not in its directory).
        assert!(store.read("sess-2", &ref_value).is_err());
        // stat/AttachmentStat was removed together with conversationAttachmentStatV4, which went away at the claim boundary
        // (see specs/rust-backend-replacement.md §4.5): only the byte-count check of a real read-back stays here.
        assert_eq!(
            store.read("sess-1", &ref_value).expect("reread").len(),
            bytes.len()
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn store_rejects_malformed_refs_without_touching_disk() {
        let root = temp_root("malformed");
        let store = AttachmentStore::new(&root);
        for bad in [
            "/etc/passwd",
            "zcode-artifact://../../secrets",
            "zcode-artifact://nothex",
            "zcode-artifact://DEADBEEF", // uppercase rejected
        ] {
            assert!(
                store.read("sess-1", bad).is_err(),
                "malformed ref must be rejected: {bad}"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn commit_writes_and_is_idempotent() {
        let root = temp_root("commit");
        let store = AttachmentStore::new(&root);
        let mut registry = UploadRegistry::new();
        let bytes = b"hello-native-attachment".to_vec();
        let begin = begin_params("up-1", &bytes);
        let staged = registry.begin(&begin, 0).expect("begin");
        assert_eq!(staged["state"], json!("staging"));
        assert_eq!(staged["nextChunkIndex"], json!(0));

        let chunk = json!({
            "connectionId": "conn-1", "sessionId": "sess-1", "uploadId": "up-1",
            "chunkIndex": 0, "dataBase64": crate::tools::read_image::base64_encode(&bytes),
        });
        let advanced = registry.chunk(&chunk, 0).expect("chunk");
        assert_eq!(advanced["nextChunkIndex"], json!(1));

        let commit_params = json!({
            "connectionId": "conn-1", "sessionId": "sess-1", "uploadId": "up-1",
        });
        let committed = registry.commit(&store, &commit_params, 0).expect("commit");
        let ref_value = committed["ref"].as_str().expect("ref").to_string();
        assert_eq!(store.read("sess-1", &ref_value).expect("read"), bytes);

        // Re-submitting begin → the committed state answers the ref straight away; re-submitting commit is idempotent.
        let again = registry.begin(&begin, 0).expect("re-begin");
        assert_eq!(again["state"], json!("committed"));
        assert_eq!(again["ref"], json!(ref_value));
        let recommit = registry
            .commit(&store, &commit_params, 0)
            .expect("re-commit");
        assert_eq!(recommit["ref"], json!(ref_value));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn commit_rejects_incomplete_and_bad_checksum() {
        let root = temp_root("reject");
        let store = AttachmentStore::new(&root);
        let mut registry = UploadRegistry::new();
        let bytes = b"payload".to_vec();
        registry
            .begin(&begin_params("up-2", &bytes), 0)
            .expect("begin");
        let commit_params = json!({
            "connectionId": "conn-1", "sessionId": "sess-1", "uploadId": "up-2",
        });
        // No chunk at all → incomplete.
        let err = registry
            .commit(&store, &commit_params, 0)
            .expect_err("incomplete commit must fail");
        assert!(err.data.is_some());

        // Checksum mismatch: declare a different hash.
        let mut registry2 = UploadRegistry::new();
        let mut begin = begin_params("up-3", &bytes);
        begin["checksum"] =
            json!("sha256:0000000000000000000000000000000000000000000000000000000000000000");
        registry2.begin(&begin, 0).expect("begin");
        registry2
            .chunk(
                &json!({
                    "connectionId": "conn-1", "sessionId": "sess-1", "uploadId": "up-3",
                    "chunkIndex": 0, "dataBase64": crate::tools::read_image::base64_encode(&bytes),
                }),
                0,
            )
            .expect("chunk");
        let err = registry2
            .commit(
                &store,
                &json!({"connectionId": "conn-1", "sessionId": "sess-1", "uploadId": "up-3"}),
                0,
            )
            .expect_err("checksum mismatch must fail");
        assert!(err.data.is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn chunk_ordering_is_enforced() {
        let root = temp_root("order");
        let store = AttachmentStore::new(&root);
        let _ = store;
        let mut registry = UploadRegistry::new();
        let bytes = b"abcdefgh".to_vec();
        let mut begin = begin_params("up-4", &bytes);
        begin["totalChunks"] = json!(2);
        begin["checksum"] = json!(format!("sha256:{}", sha256_hex(&bytes)));
        registry.begin(&begin, 0).expect("begin");
        // chunk 1 before 0 → gap。
        let err = registry
            .chunk(
                &json!({
                    "connectionId": "conn-1", "sessionId": "sess-1", "uploadId": "up-4",
                    "chunkIndex": 1, "dataBase64": crate::tools::read_image::base64_encode(b"efgh"),
                }),
                0,
            )
            .expect_err("gap must fail");
        assert!(err.data.is_some());
        // chunk 0 ok; re-sending the same chunk identically → idempotent; differently → conflict.
        registry
            .chunk(
                &json!({
                    "connectionId": "conn-1", "sessionId": "sess-1", "uploadId": "up-4",
                    "chunkIndex": 0, "dataBase64": crate::tools::read_image::base64_encode(b"abcd"),
                }),
                0,
            )
            .expect("chunk 0");
        registry
            .chunk(
                &json!({
                    "connectionId": "conn-1", "sessionId": "sess-1", "uploadId": "up-4",
                    "chunkIndex": 0, "dataBase64": crate::tools::read_image::base64_encode(b"abcd"),
                }),
                0,
            )
            .expect("idempotent chunk");
        let conflict = registry.chunk(
            &json!({
                "connectionId": "conn-1", "sessionId": "sess-1", "uploadId": "up-4",
                "chunkIndex": 0, "dataBase64": crate::tools::read_image::base64_encode(b"XXXX"),
            }),
            0,
        );
        assert!(conflict.is_err(), "differing re-sent chunk must conflict");
    }

    #[test]
    fn validation_rejects_bad_shapes() {
        let root = temp_root("validate");
        let _ = &root;
        let mut registry = UploadRegistry::new();
        // Zero bytes yet a declared chunk count.
        let err = registry
            .begin(
                &json!({
                    "connectionId": "conn-1", "sessionId": "sess-1", "uploadId": "u",
                    "fileName": "f", "mime": "image/png", "totalBytes": 0, "totalChunks": 1,
                    "checksum": format!("sha256:{}", sha256_hex(b"")),
                }),
                0,
            )
            .expect_err("zero-byte with chunks must fail");
        assert!(err.data.is_some());
        // Illegal uploadId (starts with a separator).
        let err = registry
            .begin(
                &json!({
                    "connectionId": "conn-1", "sessionId": "sess-1", "uploadId": "_bad",
                    "fileName": "f", "mime": "image/png", "totalBytes": 1, "totalChunks": 1,
                    "checksum": format!("sha256:{}", sha256_hex(b"x")),
                }),
                0,
            )
            .expect_err("uploadId shape");
        assert!(err.data.is_some());
        // Illegal mime.
        assert!(!is_valid_mime("not a mime"));
        assert!(is_valid_mime("application/pdf"));
        assert!(!is_valid_checksum("md5:deadbeef"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn preview_media_detection_matches_ts() {
        assert!(is_preview_media("image/png"));
        assert!(is_preview_media("VIDEO/mp4"));
        assert!(is_preview_media("application/pdf"));
        assert!(is_preview_media("application/pdf; charset=binary"));
        assert!(!is_preview_media("text/plain"));
        assert!(!is_preview_media("application/json"));
    }
}

#[cfg(test)]
mod model_parts_tests {
    use super::*;
    use nc_code_provider::request::ContentPart;

    /// Each test gets an **exclusive** temporary root. `pid + now_ms()` alone is not enough: two tests started
    /// in the same millisecond would take the same directory, and each one's `remove_dir_all` would delete the other's
    /// temp file mid atomic rename, surfacing as `rename failed: os error 2` (reproduced with the whole suite running in parallel).
    static STORE_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn store() -> AttachmentStore {
        let sequence = STORE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "zcode-host-attachment-parts-{pid}-{now}-{sequence}",
            pid = std::process::id(),
            now = now_ms()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp root");
        AttachmentStore::new(dir)
    }

    fn attachment(ref_value: &str, file_name: &str, mime: &str, bytes: u64) -> Value {
        json!({"ref": ref_value, "fileName": file_name, "mime": mime, "bytes": bytes})
    }

    fn text_of(part: &ContentPart) -> String {
        match part {
            ContentPart::Text { text } => text.clone(),
            other => panic!("expected a text part, got {other:?}"),
        }
    }

    /// Only three outcomes: an image part, an inlined text part, or a reference/placeholder text part carrying the declared value.
    #[test]
    fn image_refs_become_base64_image_parts() {
        let store = store();
        // A known RFC 4648 vector: b"abc" → "YWJj" (so the assertion does not depend on the encoder under test).
        let ref_value = store.write("s-1", b"abc").expect("write");
        let parts = model_parts_for_attachments(
            &store,
            "s-1",
            &[attachment(&ref_value, "a.png", "image/png", 3)],
        );
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            ContentPart::Image { media_type, data } => {
                assert_eq!(
                    media_type, "image/png",
                    "mime uses the value declared by the uploader and does not sniff twice"
                );
                assert_eq!(data, "YWJj");
            }
            other => panic!("expected an image part, got {other:?}"),
        }
    }

    #[test]
    fn text_like_mimes_inline_their_content() {
        let store = store();
        let ref_value = store.write("s-1", b"hello model").expect("write");
        for mime in ["text/plain", "application/json", "text/x-rust"] {
            let parts = model_parts_for_attachments(
                &store,
                "s-1",
                &[attachment(&ref_value, "f", mime, 11)],
            );
            assert_eq!(
                text_of(&parts[0]),
                "hello model",
                "{mime} must inline as text"
            );
        }
    }

    #[test]
    fn oversized_text_is_cut_at_the_ts_line_cap() {
        let store = store();
        let body = format!("{}\n", "x".repeat(200)).repeat(ATTACHMENT_TEXT_MAX_LINES + 500);
        assert!(
            body.len() > ATTACHMENT_TEXT_MAX_BYTES,
            "fixture must exceed 256 KiB"
        );
        let ref_value = store.write("s-1", body.as_bytes()).expect("write");
        let parts = model_parts_for_attachments(
            &store,
            "s-1",
            &[attachment(
                &ref_value,
                "big.txt",
                "text/plain",
                body.len() as u64,
            )],
        );
        let text = text_of(&parts[0]);
        assert_eq!(
            text.lines().count(),
            ATTACHMENT_TEXT_MAX_LINES,
            "oversized text keeps the first READ_DEFAULT_MAX_LINES lines"
        );
        assert!(
            !text.contains("truncated"),
            "TS puts truncation detail in preview metadata, not into the model text"
        );
    }

    #[test]
    fn declared_text_that_is_not_utf8_is_treated_as_binary() {
        let store = store();
        let bytes = [0xff_u8, 0xfe, 0x00, 0x80];
        let ref_value = store.write("s-1", &bytes).expect("write");
        let parts = model_parts_for_attachments(
            &store,
            "s-1",
            &[attachment(&ref_value, "blob.txt", "text/plain", 4)],
        );
        let text = text_of(&parts[0]);
        assert!(text.starts_with("Attached text/plain: blob.txt"), "{text}");
        assert!(
            text.contains("the file is not a known text attachment"),
            "must say why it was not inlined: {text}"
        );
        assert!(
            !text.contains('\u{fffd}'),
            "a lossy decode must never be passed to the model as file content: {text}"
        );
    }

    #[test]
    fn unsupported_media_hands_the_model_a_reference_with_the_file_name() {
        let store = store();
        let ref_value = store.write("s-1", b"%PDF-1.4").expect("write");
        let parts = model_parts_for_attachments(
            &store,
            "s-1",
            &[attachment(&ref_value, "spec.pdf", "application/pdf", 8)],
        );
        let text = text_of(&parts[0]);
        assert!(
            text.starts_with("Attached application/pdf: spec.pdf"),
            "{text}"
        );
        assert!(
            text.contains("Use the available file reading tools"),
            "TS's path-reference block tells the model how to fetch it: {text}"
        );
    }

    #[test]
    fn failures_are_reported_in_place_and_never_dropped() {
        let store = store();
        let good = store.write("s-1", b"abc").expect("write");
        let missing = format!("zcode-artifact://{:0>64}", "1");
        let parts = model_parts_for_attachments(
            &store,
            "s-1",
            &[
                attachment("not-a-ref", "ghost.png", "image/png", 3),
                attachment(&missing, "gone.png", "image/png", 3),
                json!({"fileName": "refless.png", "mime": "image/png", "bytes": 3}),
                attachment(&good, "a.png", "image/png", 3),
            ],
        );
        assert_eq!(parts.len(), 4, "one part per attachment, in payload order");
        // The ref exists but its shape is not a content-addressed reference → store cannot read bytes, so read_failed;
        // only a completely missing ref field is unresolved. Both speak in place, neither is silently dropped.
        assert_eq!(
            text_of(&parts[0]),
            "[Attached image/png: attachment_read_failed]"
        );
        assert_eq!(
            text_of(&parts[1]),
            "[Attached image/png: attachment_read_failed]"
        );
        assert_eq!(
            text_of(&parts[2]),
            "[Attached text/plain: attachment_unresolved]"
        );
        assert!(
            matches!(&parts[3], ContentPart::Image { .. }),
            "a resolvable attachment next to a failed one must still be delivered"
        );
    }

    /// Cross-session isolation: session A's ref does not resolve inside session B, and another session's bytes must never be read.
    #[test]
    fn refs_do_not_resolve_across_sessions() {
        let store = store();
        let ref_value = store.write("session-a", b"secret").expect("write");
        let parts = model_parts_for_attachments(
            &store,
            "session-b",
            &[attachment(&ref_value, "a.png", "image/png", 6)],
        );
        assert_eq!(
            text_of(&parts[0]),
            "[Attached image/png: attachment_read_failed]"
        );
    }

    /// The UI sends desktop attachments as `ref = <absolute local path>` (zero upload).
    /// The model resolver must read that file, and must still reject a relative or missing
    /// path instead of inventing content.
    #[test]
    fn local_path_refs_are_read_for_the_model_and_bad_paths_fail_closed() {
        let store = store();
        let file = std::env::temp_dir().join(format!(
            "zcode-local-ref-{}-{}.txt",
            std::process::id(),
            now_ms()
        ));
        fs::write(&file, b"LOCAL-PATH-CONTENT").expect("write local file");
        let path_ref = file.to_string_lossy().to_string();
        let parts = model_parts_for_attachments(
            &store,
            "s-1",
            &[attachment(&path_ref, "pasted.txt", "text/plain", 17)],
        );
        let text = text_of(&parts[0]);
        assert!(
            text.contains("LOCAL-PATH-CONTENT"),
            "a local-path text attachment is inlined from the real file: {text}"
        );
        assert!(
            store.read_for_model("s-1", "relative/path.txt").is_err(),
            "a relative ref is not a store key and must fail closed"
        );
        let missing = std::env::temp_dir().join("zcode-does-not-exist-attachment.bin");
        assert!(
            store
                .read_for_model("s-1", &missing.to_string_lossy())
                .is_err(),
            "a missing absolute path must fail closed"
        );
        let _ = fs::remove_file(&file);
    }
}
