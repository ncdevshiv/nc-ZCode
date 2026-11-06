//! `media-preview` channel.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::{ServiceArgs, ServiceError};
use serde_json::json;
use tokio::fs;

pub struct MediaPreviewService;

impl MediaPreviewService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }

    async fn prepare(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let path_str = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let path = Path::new(path_str);
        let metadata = fs::metadata(path)
            .await
            .map_err(|e| ServiceError::new(e.to_string()))?;
        let size = metadata.len();
        let max_bytes = 8 * 1024 * 1024u64;
        if size > max_bytes {
            return Err(ServiceError::new(format!(
                "Media file is too large for inline preview: {}",
                path_str
            )));
        }
        let data = fs::read(path)
            .await
            .map_err(|e| ServiceError::new(e.to_string()))?;
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
        let mime = mime_from_path(path_str);
        Ok(WireValue::Json(json!({
            "kind": "inline",
            "dataBase64": b64,
            "mediaType": mime,
            "path": path_str,
            "size": size,
        })))
    }
}

fn mime_from_path(path: &str) -> &'static str {
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
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

impl Default for MediaPreviewService {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl Channel for MediaPreviewService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        match command {
            "prepare" => self.prepare(&args).await.map_err(ChannelFailure::from),
            "refreshPlaybackUrl" => Ok(WireValue::Json(json!({"url":"","expiresAt":0}))),
            "release" => Ok(WireValue::Json(json!(null))),
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;
    use tempfile::tempdir;

    fn ctx() -> ChannelContext {
        ChannelContext::new("media-preview", ClientMode::DesktopContinuous)
    }

    #[tokio::test]
    async fn prepare_returns_inline_for_small_file() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("a.png");
        std::fs::write(&p, [137u8, 80, 78, 71]).unwrap();
        let svc = MediaPreviewService::new();
        let r = svc
            .call(
                &ctx(),
                "prepare",
                vec![WireValue::Json(
                    json!({"path": p.to_string_lossy(), "expectedKind": "image"}),
                )],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = r else {
            panic!("expected json")
        };
        assert_eq!(v["kind"], "inline");
        assert_eq!(v["mediaType"], "image/png");
        assert!(v["dataBase64"]
            .as_str()
            .map(|s| !s.is_empty())
            .unwrap_or(false));
        assert_eq!(v["size"], 4);
        assert!(v["path"].is_string());
    }

    #[tokio::test]
    async fn prepare_rejects_missing_file() {
        let svc = MediaPreviewService::new();
        let err = svc
            .call(
                &ctx(),
                "prepare",
                vec![WireValue::Json(
                    json!({"path": "C:/__no_such_media_file_12345__.png", "expectedKind": "image"}),
                )],
                CancelToken::new(),
            )
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn refresh_and_release_contract() {
        let svc = MediaPreviewService::new();
        let r = svc
            .call(
                &ctx(),
                "refreshPlaybackUrl",
                vec![WireValue::Json(json!({"path":"x"}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = r else {
            panic!("json")
        };
        assert!(v.get("url").is_some());
        assert!(v.get("expiresAt").is_some());

        let rel = svc
            .call(
                &ctx(),
                "release",
                vec![WireValue::Json(json!({"path":"x"}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(
            rel,
            WireValue::Json(serde_json::Value::Null) | WireValue::Undefined | WireValue::Json(_)
        ));
    }

    #[tokio::test]
    async fn unknown_method_fails() {
        let svc = MediaPreviewService::new();
        assert!(svc
            .call(&ctx(), "nope", vec![], CancelToken::new())
            .await
            .is_err());
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use nc_code_channels::ClientMode;
    #[tokio::test]
    async fn crate_compiles_and_channel_contract() {
        let svc = MediaPreviewService::new();
        let ctx = ChannelContext::new("media-preview", ClientMode::DesktopContinuous);
        let rel = svc
            .call(&ctx, "release", vec![], CancelToken::new())
            .await
            .unwrap();
        let _ = rel;
    }
}
