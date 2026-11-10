//! `media-preview` channel — port of
//! `packages/services/src/media-preview/mediaPreview.ts`.
//!
//! The TS service returns a `local-url` when the platform can mint a local media
//! URL (Electron custom protocol), otherwise it falls back to an `inline` base64
//! payload capped at 8 MiB. The Tauri shell has no custom media protocol, so this
//! port implements the **inline** branch with the same format gate, the same
//! `expectedKind` check, the same stat/type validation and the same size limit —
//! no fabricated URL. `refreshPlaybackUrl`/`release` only exist for host-range
//! leases, which this host does not mint; they answer with the TS service's own
//! lease semantics (unknown id = expired / no-op) instead of pretending to rotate.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::{ServiceArgs, ServiceError};

/// `MEDIA_PREVIEW_FORMATS` (`packages/shared/src/media-preview.ts`), lowercased
/// extensions with the same `kind`/`mediaType` mapping the renderer validates.
const MEDIA_PREVIEW_FORMATS: &[(&str, &str, &str)] = &[
    (".mp4", "video", "video/mp4"),
    (".mov", "video", "video/quicktime"),
    (".webm", "video", "video/webm"),
    (".m4v", "video", "video/x-m4v"),
    (".mp3", "audio", "audio/mpeg"),
    (".wav", "audio", "audio/wav"),
    (".m4a", "audio", "audio/mp4"),
    (".ogg", "audio", "audio/ogg"),
    (".opus", "audio", "audio/opus"),
    (".flac", "audio", "audio/flac"),
    (".weba", "audio", "audio/webm"),
];

/// `createMediaPreviewService` default `inlineMaxBytes`.
const INLINE_MAX_BYTES: u64 = 8 * 1024 * 1024;

fn media_preview_format(path: &str) -> Option<(&'static str, &'static str)> {
    let normalized = path.replace('\\', "/").to_lowercase();
    MEDIA_PREVIEW_FORMATS
        .iter()
        .find(|(extension, _, _)| normalized.ends_with(extension))
        .map(|(_, kind, media_type)| (*kind, *media_type))
}

pub struct MediaPreviewService;

impl MediaPreviewService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }

    async fn prepare(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let path = params
            .get("path")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ServiceError::invalid_params("params.path is required"))?;
        let expected_kind = params
            .get("expectedKind")
            .and_then(|value| value.as_str())
            .ok_or_else(|| ServiceError::invalid_params("params.expectedKind is required"))?;
        let (kind, media_type) = media_preview_format(path).ok_or_else(|| {
            ServiceError::new(format!("Unsupported media preview format: {path}"))
        })?;
        if kind != expected_kind {
            return Err(ServiceError::new(format!(
                "Unsupported media preview format: {path}"
            )));
        }
        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|error| ServiceError::new(format!("resolve media preview failed: {error}")))?;
        if !metadata.is_file() {
            return Err(ServiceError::new(format!(
                "Path is not a media file: {path}"
            )));
        }
        if metadata.len() > INLINE_MAX_BYTES {
            return Err(ServiceError::new(format!(
                "Media file is too large for inline preview: {path}"
            )));
        }
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|error| ServiceError::new(format!("read media preview failed: {error}")))?;
        Ok(WireValue::Json(serde_json::json!({
            "kind": "inline",
            "dataBase64": base64::engine::general_purpose::STANDARD.encode(&bytes),
            "mediaType": media_type,
            "path": path,
            "size": metadata.len(),
        })))
    }

    fn refresh_playback_url() -> Result<WireValue, ServiceError> {
        // No host-range lease is minted on this host (inline only), so every id is
        // expired — the same answer the TS proxy gives for an unknown/expired lease.
        Err(ServiceError::new("Media preview lease expired"))
    }

    fn release() -> WireValue {
        // TS `release` removes a lease if present and returns void otherwise; an
        // inline preview owns no lease, so releasing is an honest no-op.
        WireValue::Undefined
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
            "refreshPlaybackUrl" => Self::refresh_playback_url().map_err(ChannelFailure::from),
            "release" => Ok(Self::release()),
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;

    fn ctx() -> ChannelContext {
        ChannelContext::new("media-preview", ClientMode::DesktopContinuous)
    }

    fn temp_workspace(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zcode-media-preview-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("workspace");
        dir
    }

    async fn call(
        service: &Arc<MediaPreviewService>,
        method: &str,
        param: WireValue,
    ) -> Result<WireValue, ChannelFailure> {
        service
            .call(&ctx(), method, vec![param], CancelToken::new())
            .await
    }

    #[tokio::test]
    async fn prepare_returns_the_real_bytes_for_an_audio_file() {
        let root = temp_workspace("audio");
        let path = root.join("clip.mp3");
        std::fs::write(&path, b"ID3-not-a-real-codec-but-the-bytes-are").expect("write");
        let service = MediaPreviewService::new();
        let result = call(
            &service,
            "prepare",
            WireValue::Json(serde_json::json!({
                "path": path.to_string_lossy(),
                "expectedKind": "audio",
            })),
        )
        .await
        .expect("prepare");
        let WireValue::Json(value) = result else {
            panic!("expected JSON");
        };
        assert_eq!(value["kind"], serde_json::json!("inline"));
        assert_eq!(value["mediaType"], serde_json::json!("audio/mpeg"));
        assert_eq!(value["size"], serde_json::json!(38));
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(value["dataBase64"].as_str().expect("base64"))
            .expect("valid base64");
        assert_eq!(decoded, b"ID3-not-a-real-codec-but-the-bytes-are");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn prepare_rejects_the_wrong_kind_unknown_formats_and_directories() {
        let root = temp_workspace("reject");
        let audio = root.join("clip.mp3");
        std::fs::write(&audio, b"bytes").expect("write");
        let notes = root.join("notes.txt");
        std::fs::write(&notes, b"text").expect("write");
        let service = MediaPreviewService::new();
        let wrong_kind = call(
            &service,
            "prepare",
            WireValue::Json(serde_json::json!({
                "path": audio.to_string_lossy(),
                "expectedKind": "video",
            })),
        )
        .await
        .expect_err("kind mismatch must fail");
        assert!(wrong_kind
            .to_string()
            .contains("Unsupported media preview format"));
        let unknown = call(
            &service,
            "prepare",
            WireValue::Json(serde_json::json!({
                "path": notes.to_string_lossy(),
                "expectedKind": "audio",
            })),
        )
        .await
        .expect_err("unknown extension must fail");
        assert!(unknown
            .to_string()
            .contains("Unsupported media preview format"));
        let directory = call(
            &service,
            "prepare",
            WireValue::Json(serde_json::json!({
                "path": root.to_string_lossy(),
                "expectedKind": "audio",
            })),
        )
        .await
        .expect_err("a directory is not a media file");
        assert!(directory
            .to_string()
            .contains("Unsupported media preview format"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn prepare_caps_inline_at_eight_megabytes() {
        let root = temp_workspace("caps");
        let path = root.join("big.mp4");
        std::fs::write(&path, vec![0u8; 8 * 1024 * 1024 + 1]).expect("write");
        let service = MediaPreviewService::new();
        let error = call(
            &service,
            "prepare",
            WireValue::Json(serde_json::json!({
                "path": path.to_string_lossy(),
                "expectedKind": "video",
            })),
        )
        .await
        .expect_err("oversize must fail");
        assert!(
            error.to_string().contains("too large for inline preview"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn lease_members_answer_with_the_ts_semantics() {
        let service = MediaPreviewService::new();
        let released = call(&service, "release", WireValue::Undefined)
            .await
            .expect("release is a no-op for inline previews");
        assert!(matches!(released, WireValue::Undefined));
        let refresh = call(
            &service,
            "refreshPlaybackUrl",
            WireValue::Json(serde_json::json!({"previewId": "missing"})),
        )
        .await
        .expect_err("no host-range lease exists on this host");
        assert!(refresh.to_string().contains("Media preview lease expired"));
    }
}
