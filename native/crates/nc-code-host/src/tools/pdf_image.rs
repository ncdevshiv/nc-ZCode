//! JPEG budget preparation for rendered PDF pages (model byte/dimension caps).
//!
//! Split from `pdf_render.rs` (spawn/render) to keep each file under 400 lines.

use super::{ToolError, ToolErrorCode};

pub const READ_IMAGE_MAX_BASE64_BYTES: usize = 5 * 1024 * 1024;
pub const READ_IMAGE_MAX_DIMENSION: u32 = 2_000;

#[derive(Debug, Clone)]
pub struct PreparedImage {
    pub bytes: Vec<u8>,
    pub original_width: u32,
    pub original_height: u32,
    pub display_width: u32,
    pub display_height: u32,
    pub resized: bool,
    pub compressed: bool,
}

pub fn prepare_jpeg_for_model(bytes: &[u8]) -> Result<PreparedImage, ToolError> {
    let decoded = image::load_from_memory(bytes).map_err(|e| {
        ToolError::new(
            ToolErrorCode::InvalidInput,
            format!("decode rendered page: {e}"),
        )
    })?;
    let (w, h) = (decoded.width(), decoded.height());
    if bytes.len().div_ceil(3) * 4 <= READ_IMAGE_MAX_BASE64_BYTES
        && w <= READ_IMAGE_MAX_DIMENSION
        && h <= READ_IMAGE_MAX_DIMENSION
    {
        return Ok(PreparedImage {
            compressed: false,
            bytes: bytes.to_vec(),
            original_width: w,
            original_height: h,
            display_width: w,
            display_height: h,
            resized: false,
        });
    }
    let (target_w, target_h, resized_img) =
        if w > READ_IMAGE_MAX_DIMENSION || h > READ_IMAGE_MAX_DIMENSION {
            let scale = READ_IMAGE_MAX_DIMENSION as f32 / w.max(h) as f32;
            let tw = ((w as f32 * scale).round() as u32).max(1);
            let th = ((h as f32 * scale).round() as u32).max(1);
            let resized =
                image::imageops::resize(&decoded, tw, th, image::imageops::FilterType::Lanczos3);
            (tw, th, resized)
        } else {
            (w, h, decoded.to_rgba8())
        };
    let dyn_img = image::DynamicImage::ImageRgba8(resized_img);
    let mut buf = std::io::Cursor::new(Vec::new());
    dyn_img
        .write_to(&mut buf, image::ImageFormat::Jpeg)
        .map_err(|e| {
            ToolError::new(
                ToolErrorCode::InvalidInput,
                format!("encode rendered page: {e}"),
            )
        })?;
    let out_bytes = buf.into_inner();
    if out_bytes.len().div_ceil(3) * 4 > READ_IMAGE_MAX_BASE64_BYTES {
        return Err(ToolError::new(
            ToolErrorCode::InvalidInput,
            "rendered PDF page still exceeds the model byte budget after downsample",
        ));
    }
    let compressed = out_bytes.len() < bytes.len();
    Ok(PreparedImage {
        bytes: out_bytes,
        original_width: w,
        original_height: h,
        display_width: target_w,
        display_height: target_h,
        resized: target_w != w || target_h != h,
        compressed,
    })
}
