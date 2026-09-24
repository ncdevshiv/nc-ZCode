//! ReadImage：把图片文件读成模型可消费的 base64（替代 TS
//! `core/src/tool/handlers/read-image.ts`）。
//!
//! 契约对齐 `contracts/tools/read.ts` 的 READ_IMAGE_* 常量：输入 ≤20 MiB，
//! 边长 ≤2000，base64 载荷 ≤5 MiB（对应 ~3.75 MiB 原图）；webp 直通不重编码
//! （与 TS `resizeToFit` 的 webp 分支一致——webp 已是模型支持的紧凑格式）。
//! 其它格式超预算时经 zcode-image 降采样（model 视觉输入以尺寸语义为准，字节
//! parity 非本工具契约，crate README 差异表记录）。

use serde_json::{json, Value};

use crate::tools::fs_tools::resolve_workspace_path;
use crate::tools::{string_schema, Tool, ToolContext, ToolContract, ToolError, ToolErrorCode};

const READ_IMAGE_MAX_INPUT_BYTES: u64 = 20 * 1024 * 1024;
const READ_IMAGE_MAX_BASE64_BYTES: usize = 5 * 1024 * 1024;
const READ_IMAGE_MAX_DIMENSION: u32 = 2_000;

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::new(ToolErrorCode::InvalidInput, message)
}

pub struct ReadImageTool;

impl Tool for ReadImageTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "ReadImage".to_string(),
            description: "Read an image file (png/jpeg/webp/gif) and return it as a model-ready base64 payload, downsampled when it exceeds the model byte budget.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file_path": string_schema("path relative to the workspace root"),
                },
                "required": ["file_path"],
            }),
            read_only: true,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: crate::tools::SCOPE_NONE,
            timeout_ms: 20_000,
            max_output_bytes: 8 * 1024 * 1024,
        }
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        if ctx.is_cancelled() {
            return Err(ToolError::new(ToolErrorCode::Cancelled, "turn cancelled"));
        }
        let raw = input
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("file_path: required string"))?;
        let path = resolve_workspace_path(ctx, raw)?;
        let metadata =
            std::fs::metadata(&path).map_err(|error| invalid(format!("stat {raw}: {error}")))?;
        if metadata.len() > READ_IMAGE_MAX_INPUT_BYTES {
            return Err(invalid(format!(
                "{raw} exceeds the {READ_IMAGE_MAX_INPUT_BYTES} byte image input limit"
            )));
        }
        let bytes =
            std::fs::read(&path).map_err(|error| invalid(format!("read {raw}: {error}")))?;
        let mime = detect_mime(&bytes)
            .ok_or_else(|| invalid(format!("{raw}: unsupported image format")))?;

        // webp 直通（TS 同分支）：不重编码、不降采样。
        if mime == "image/webp" {
            return Ok(image_payload(&bytes, mime, metadata.len()));
        }

        // 预算内直通；超限才降采样。
        if base64_len(bytes.len()) <= READ_IMAGE_MAX_BASE64_BYTES {
            return Ok(image_payload(&bytes, mime, bytes.len() as u64));
        }
        downsample(&bytes, mime, raw)
    }
}

fn image_payload(bytes: &[u8], mime: &str, size_bytes: u64) -> String {
    json!({
        "data": base64_encode(bytes),
        "mimeType": mime,
        "sizeBytes": size_bytes,
    })
    .to_string()
}

fn base64_len(raw_len: usize) -> usize {
    raw_len.div_ceil(3) * 4
}

/// 魔数识别（不引依赖；与 TS 的 mime 判定同结果面）。
fn detect_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    None
}

fn downsample(bytes: &[u8], mime: &str, raw: &str) -> Result<String, ToolError> {
    // 直接用 Rust image 解码降采样（host 本身是原生进程，不走 sidecar 往返）。
    let source = image::load_from_memory(bytes)
        .map_err(|error| invalid(format!("decode {raw}: {error}")))?;
    let (width, height) = (source.width(), source.height());
    if width > READ_IMAGE_MAX_DIMENSION || height > READ_IMAGE_MAX_DIMENSION {
        // 等比缩到 2000 以内。
        let scale = READ_IMAGE_MAX_DIMENSION as f32 / width.max(height) as f32;
        let target_w = ((width as f32 * scale).round() as u32).max(1);
        let target_h = ((height as f32 * scale).round() as u32).max(1);
        let resized = image::imageops::resize(
            &source,
            target_w,
            target_h,
            image::imageops::FilterType::Lanczos3,
        );
        return encode_image(&image::DynamicImage::ImageRgba8(resized), mime, raw);
    }
    encode_image(&source, mime, raw)
}

fn encode_image(dynamic: &image::DynamicImage, mime: &str, raw: &str) -> Result<String, ToolError> {
    let mut buffer = std::io::Cursor::new(Vec::new());
    let format = match mime {
        "image/png" => image::ImageFormat::Png,
        "image/jpeg" => image::ImageFormat::Jpeg,
        "image/gif" => image::ImageFormat::Gif,
        other => return Err(invalid(format!("{raw}: cannot re-encode {other}"))),
    };
    dynamic
        .write_to(&mut buffer, format)
        .map_err(|error| invalid(format!("encode {raw}: {error}")))?;
    let bytes = buffer.into_inner();
    if base64_len(bytes.len()) > READ_IMAGE_MAX_BASE64_BYTES {
        return Err(invalid(format!(
            "{raw}: even after downsample the model byte budget is exceeded"
        )));
    }
    Ok(image_payload(&bytes, mime, bytes.len() as u64))
}

/// 标准 base64（模型载荷编码；不引依赖，标准查表）。
pub fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(triple >> 18) as usize & 63] as char);
        out.push(TABLE[(triple >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(triple >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[triple as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_supported_formats_and_rejects_others() {
        assert_eq!(
            detect_mime(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0]),
            Some("image/png")
        );
        assert_eq!(detect_mime(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(detect_mime(b"GIF89a...."), Some("image/gif"));
        assert_eq!(detect_mime(b"not an image at all"), None);
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_length_matches_encoding() {
        for raw in [0usize, 1, 2, 3, 4, 5] {
            let encoded = base64_encode(&vec![b'x'; raw]);
            assert_eq!(encoded.len(), base64_len(raw));
        }
    }
}
