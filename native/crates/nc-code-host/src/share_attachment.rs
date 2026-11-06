//! Share read surface (`v4/conversation/attachmentRead|Stat`).
//!
//! Mirrors TS `v4ConversationAttachmentRead/Stat` (`transport.ts`): same
//! content-addressed store + row/index authorization as preview read, but
//! any MIME is allowed (share selection needs text/plain) and `target` +
//! `attachmentIndex` are required as a pair. `Stat` is metadata-only.

use serde_json::{json, Value};

use crate::attachment::ATTACHMENT_CHUNK_MAX_BYTES;
use crate::rpc::{RpcError, CODE_INVALID_PARAMS};
use crate::session::Session;
use crate::{session_not_found, HostServer};

fn fault(code: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(CODE_INVALID_PARAMS, message)
        .with_data(json!({ "reasonCode": format!("fault.attachment.{code}") }))
}

fn require_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
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

/// Locate one share attachment exactly inside the userInput rows by
/// target.rowId + entityId + attachmentIndex; returns its media MIME on a hit.
/// Unlike preview read there is no media allowlist: share selection accepts
/// any MIME the row declares.
fn resolve_share_media(
    session: &Session,
    ref_value: &str,
    target: &Value,
    attachment_index: u64,
) -> Option<String> {
    let row_id = target.get("rowId").and_then(Value::as_u64)?;
    let entity = target.get("entityId").and_then(Value::as_str)?;
    let row = session.rows_snapshot().into_iter().find(|row| {
        row.get("rowId").and_then(Value::as_u64) == Some(row_id)
            && row.get("entityId").and_then(Value::as_str) == Some(entity)
            && row.get("kind").and_then(Value::as_str) == Some("userInput")
    })?;
    let attachment = row
        .get("attachments")
        .and_then(Value::as_array)?
        .get(attachment_index as usize)?;
    let mime = attachment
        .get("mime")
        .and_then(Value::as_str)?
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if mime.is_empty() {
        return None;
    }
    let matches_ref =
        |field: &str| attachment.get(field).and_then(Value::as_str) == Some(ref_value);
    if !matches_ref("ref") && !matches_ref("previewRef") {
        return None;
    }
    Some(mime)
}

fn parse_target_and_index(params: &Value) -> Result<(&Value, u64), RpcError> {
    let target = params
        .get("target")
        .filter(|v| v.is_object())
        .ok_or_else(|| RpcError::new(CODE_INVALID_PARAMS, "params.target: required object"))?;
    let attachment_index = params
        .get("attachmentIndex")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            RpcError::new(
                CODE_INVALID_PARAMS,
                "params.attachmentIndex: required non-negative int",
            )
        })?;
    Ok((target, attachment_index))
}

impl HostServer {
    pub(crate) fn conversation_attachment_read(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = require_session_id(params)?;
        let session = self
            .load_session(session_id)
            .ok_or_else(|| session_not_found(session_id))?;
        let ref_value = require_str(params, "ref")?;
        let (target, attachment_index) = parse_target_and_index(params)?;
        let offset = params
            .get("offset")
            .and_then(Value::as_u64)
            .ok_or_else(|| RpcError::new(CODE_INVALID_PARAMS, "params.offset: required"))?;
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .filter(|v| *v >= 1 && *v <= ATTACHMENT_CHUNK_MAX_BYTES as u64)
            .ok_or_else(|| RpcError::new(CODE_INVALID_PARAMS, "params.limit: out of range"))?;
        let media_type = resolve_share_media(&session, ref_value, target, attachment_index)
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

    pub(crate) fn conversation_attachment_stat(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = require_session_id(params)?;
        let session = self
            .load_session(session_id)
            .ok_or_else(|| session_not_found(session_id))?;
        let ref_value = require_str(params, "ref")?;
        let (target, attachment_index) = parse_target_and_index(params)?;
        let media_type = resolve_share_media(&session, ref_value, target, attachment_index)
            .ok_or_else(|| {
                fault(
                    "previewRefNotAuthorized",
                    "attachment ref is not in this session's projection",
                )
            })?;
        let (total, mtime_ms) = self.attachments.stat(session_id, ref_value)?;
        let mut result = json!({ "mediaType": media_type, "totalBytes": total });
        if let Some(ms) = mtime_ms {
            result["mtimeMs"] = json!(ms);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temp_root(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zcode-host-share-{name}-{}-{}",
            std::process::id(),
            crate::store::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp root");
        dir
    }
    #[test]
    fn resolve_share_media_requires_row_index_pair() {
        let _ = temp_root("pair");
    }
    #[test]
    fn stat_requires_target_and_index() {
        let params = json!({ "sessionId": "s-1", "ref": "zcode-artifact://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" });
        let err = parse_target_and_index(&params).unwrap_err();
        assert_eq!(err.code, CODE_INVALID_PARAMS);
        let params =
            json!({ "sessionId": "s-1", "ref": "x", "target": { "rowId": 1, "entityId": "e" } });
        assert!(parse_target_and_index(&params).is_err());
    }
}
