//! 解码、几何变换与编码 —— 几何公式镜像 jimp（plugin-resize/plugin-cover/plugin-crop），
//! 但重采样与编码器实现不同（CatmullRom vs BICUBIC、image 编码器 vs @jimp/*）：
//! 只保证尺寸/格式/质量参数语义一致，不承诺字节级一致（见 crate README）。

use base64::Engine;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::codecs::webp::WebPEncoder;
use image::{imageops, ExtendedColorType, ImageEncoder, ImageFormat, RgbaImage};

/// 重采样核：jimp 走 BICUBIC，image 侧选最接近的 Catmull-Rom 样条。
const RESAMPLE: imageops::FilterType = imageops::FilterType::CatmullRom;

/// 输出容器（仅三种：与 sidecar 契约一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Jpeg,
    Png,
    Webp,
}

impl OutputFormat {
    /// 对应 MIME。
    pub fn mime(self) -> &'static str {
        match self {
            OutputFormat::Jpeg => "image/jpeg",
            OutputFormat::Png => "image/png",
            OutputFormat::Webp => "image/webp",
        }
    }
}

/// `info` 的解码结果。
pub struct ImageMeta {
    pub width: u32,
    pub height: u32,
    pub format: &'static str,
    pub mime: &'static str,
}

/// 单个几何 op。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Op {
    pub kind: OpKind,
    pub box_w: u32,
    pub box_h: u32,
}

/// 几何变换类型（jimp 语义子集）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    /// 装入 box 保持比例（jimp scaleToFit；TS 侧唯一实际调用的变换）。
    Contain,
    /// 放大到完全覆盖 box 后中心裁切（jimp cover）。
    Cover,
    /// 拉伸到 box 尺寸（jimp fill/resize 忽略比例）。
    Fill,
}

/// base64 → 字节：空数据与非法 base64 报可读错误（全部映射 400）。
pub fn decode_base64(data: &str) -> Result<Vec<u8>, String> {
    if data.is_empty() {
        // 与 TS createImageProcessorError("empty") 文案一致。
        return Err("Image file is empty (0 bytes)".to_string());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| "invalid base64 image data".to_string())?;
    if bytes.is_empty() {
        return Err("Image file is empty (0 bytes)".to_string());
    }
    Ok(bytes)
}

/// `info`：解码 + 魔数识别 → 尺寸/格式。
pub fn info(data: &str) -> Result<ImageMeta, String> {
    let bytes = decode_base64(data)?;
    let format = image::guess_format(&bytes).map_err(|_| decode_error_text())?;
    let image = image::load_from_memory(&bytes).map_err(|_| decode_error_text())?;
    let (label, mime) = format_labels(format);
    Ok(ImageMeta {
        width: image.width(),
        height: image.height(),
        format: label,
        mime,
    })
}

/// base64 → RGBA8 位图（统一归一到 jimp bitmap 的 8 位 RGBA 模型；
/// 16 位/浮点源在此降位，镜像 jimp 能力边界）。
pub fn decode_rgba(data: &str) -> Result<RgbaImage, String> {
    let bytes = decode_base64(data)?;
    let image = image::load_from_memory(&bytes).map_err(|_| decode_error_text())?;
    Ok(image.to_rgba8())
}

/// 依次施加几何 op；空列表 = 原尺寸（仅重编码）。
pub fn apply_ops(mut image: RgbaImage, ops: &[Op]) -> RgbaImage {
    for op in ops {
        let (src_w, src_h) = image.dimensions();
        image = match op.kind {
            OpKind::Contain => {
                let (w, h) = fit_contain(src_w, src_h, op.box_w, op.box_h);
                imageops::resize(&image, w, h, RESAMPLE)
            }
            OpKind::Cover => {
                let (w, h) = fit_cover(src_w, src_h, op.box_w, op.box_h);
                let resized = imageops::resize(&image, w, h, RESAMPLE);
                let x = crop_offset(w, op.box_w);
                let y = crop_offset(h, op.box_h);
                imageops::crop_imm(&resized, x, y, op.box_w, op.box_h).to_image()
            }
            OpKind::Fill => imageops::resize(&image, op.box_w, op.box_h, RESAMPLE),
        };
    }
    image
}

/// jimp scaleToFit/contain：装入 `(box_w, box_h)` 保持比例，取约束边；
/// 输出按 `round(src*f) || 1` 取整（镜像 jimp 的防零下限）。
pub fn fit_contain(src_w: u32, src_h: u32, box_w: u32, box_h: u32) -> (u32, u32) {
    let factor = if ratio(box_w, box_h) > ratio(src_w, src_h) {
        f64::from(box_h) / f64::from(src_h)
    } else {
        f64::from(box_w) / f64::from(src_w)
    };
    (scale(src_w, factor), scale(src_h, factor))
}

/// jimp cover：放大到恰好覆盖 box（一边贴合、另一边溢出），返回缩放后尺寸。
pub fn fit_cover(src_w: u32, src_h: u32, box_w: u32, box_h: u32) -> (u32, u32) {
    let factor = if ratio(box_w, box_h) > ratio(src_w, src_h) {
        f64::from(box_w) / f64::from(src_w)
    } else {
        f64::from(box_h) / f64::from(src_h)
    };
    (scale(src_w, factor), scale(src_h, factor))
}

/// 中心裁切偏移：`round((scaled - target) / 2)`（镜像 jimp plugin-cover）。
fn crop_offset(scaled: u32, target: u32) -> u32 {
    let gap = f64::from(scaled.saturating_sub(target));
    (gap / 2.0).round() as u32
}

fn ratio(width: u32, height: u32) -> f64 {
    f64::from(width) / f64::from(height)
}

/// `round(src*f)` 且下限 1（jimp 公式里的 `|| 1`）。
fn scale(src: u32, factor: f64) -> u32 {
    ((f64::from(src) * factor).round() as u32).max(1)
}

/// 编码：jpeg 丢 alpha（jimp/jpeg-js 同样只写 RGB，无 matte 合成）；
/// png 固定 RGBA8 + 压缩 Level 9（对齐 TS deflateLevel 9）；webp 无损 RGBA8。
pub fn encode(image: &RgbaImage, output: OutputFormat, quality: u8) -> Result<Vec<u8>, String> {
    let (width, height) = image.dimensions();
    let mut buffer = Vec::new();
    match output {
        OutputFormat::Jpeg => {
            let rgb = image::DynamicImage::ImageRgba8(image.clone()).to_rgb8();
            JpegEncoder::new_with_quality(&mut buffer, quality)
                .encode_image(&rgb)
                .map_err(|err| format!("jpeg encode failed: {err}"))?;
        }
        OutputFormat::Png => {
            PngEncoder::new_with_quality(
                &mut buffer,
                CompressionType::Level(9),
                FilterType::Adaptive,
            )
            .write_image(image.as_raw(), width, height, ExtendedColorType::Rgba8)
            .map_err(|err| format!("png encode failed: {err}"))?;
        }
        OutputFormat::Webp => {
            WebPEncoder::new_lossless(&mut buffer)
                .encode(image.as_raw(), width, height, ExtendedColorType::Rgba8)
                .map_err(|err| format!("webp encode failed: {err}"))?;
        }
    }
    Ok(buffer)
}

/// 解码失败文案（镜像 TS "Unable to decode image data"）。
fn decode_error_text() -> String {
    "Unable to decode image data".to_string()
}

/// 格式标签：`(format, mime)`；未列出的格式回退二进制 MIME。
fn format_labels(format: ImageFormat) -> (&'static str, &'static str) {
    match format {
        ImageFormat::Jpeg => ("jpeg", "image/jpeg"),
        ImageFormat::Png => ("png", "image/png"),
        ImageFormat::Gif => ("gif", "image/gif"),
        ImageFormat::WebP => ("webp", "image/webp"),
        ImageFormat::Bmp => ("bmp", "image/bmp"),
        ImageFormat::Tiff => ("tiff", "image/tiff"),
        ImageFormat::Ico => ("ico", "image/x-icon"),
        ImageFormat::Tga => ("tga", "application/octet-stream"),
        ImageFormat::Pnm => ("pnm", "image/x-portable-anymap"),
        ImageFormat::Qoi => ("qoi", "image/qoi"),
        ImageFormat::Avif => ("avif", "image/avif"),
        ImageFormat::OpenExr => ("exr", "image/x-exr"),
        ImageFormat::Hdr => ("hdr", "image/vnd.radiance"),
        ImageFormat::Dds => ("dds", "image/vnd-ms.dds"),
        _ => ("bin", "application/octet-stream"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_contain_matches_jimp_binding_edge_math() {
        // 盒更“宽”→ 高度贴合：100x100 装 1920x1080 → 100x56（round 56.25）。
        assert_eq!(fit_contain(1920, 1080, 100, 100), (100, 56));
        // 源更“宽”→ 宽度贴合：100x200 装 100x100 → 50x100。
        assert_eq!(fit_contain(100, 200, 100, 100), (50, 100));
        // 非整比例取整：3x2 装 2x2 → 2x1（round 1.333）。
        assert_eq!(fit_contain(3, 2, 2, 2), (2, 1));
        // `|| 1` 防零：1000x1 装 4x4 → 高度 round(0.004)=0 → 1。
        assert_eq!(fit_contain(1000, 1, 4, 4), (4, 1));
        // 等比放大（cover/contain 不做界内保护，镜像 jimp 原始 op 数学）。
        assert_eq!(fit_contain(10, 10, 40, 40), (40, 40));
    }

    #[test]
    fn fit_cover_overshoots_then_center_crops_to_box() {
        // 源 100x200（更“高”）覆盖 50x50 → 宽贴合 50x100，纵向 gap=50 → 偏移 25。
        assert_eq!(fit_cover(100, 200, 50, 50), (50, 100));
        assert_eq!(crop_offset(100, 50), 25);
        // 源 8x4（更“宽”）覆盖 4x4 → 高贴合 8x4？不：宽贴合 8x4→ 比例 2>1
        // → factor = 4/8 → 缩放 4x2？cover 必须两边 ≥ box：8x4 覆盖 4x4
        // → ratio_box(1) > ratio_src(2)? 否 → factor = box_h/src_h = 1 → 8x4，
        // 横向溢出 4 → 偏移 2，裁出 4x4。
        assert_eq!(fit_cover(8, 4, 4, 4), (8, 4));
        assert_eq!(crop_offset(8, 4), 2);
        // 奇数溢出向上取整，保证裁切不越界。
        assert_eq!(crop_offset(9, 4), 3); // 3+4=7 ≤ 9
    }

    #[test]
    fn roundtrip_png_info_and_dimensions() {
        // 构造 5x3 RGBA 图 → png → info 尺寸/格式，→ fill op 后尺寸正确。
        let source = image::RgbaImage::from_fn(5, 3, |x, y| {
            image::Rgba([(x * 20) as u8, (y * 30) as u8, 64, 255])
        });
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(source.as_raw(), 5, 3, ExtendedColorType::Rgba8)
            .expect("png encode");
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);

        let meta = info(&b64).expect("info");
        assert_eq!((meta.width, meta.height), (5, 3));
        assert_eq!(meta.format, "png");
        assert_eq!(meta.mime, "image/png");

        let decoded = decode_rgba(&b64).expect("decode");
        let out = apply_ops(
            decoded,
            &[Op {
                kind: OpKind::Fill,
                box_w: 7,
                box_h: 2,
            }],
        );
        assert_eq!(out.dimensions(), (7, 2));

        let jpeg = encode(&out, OutputFormat::Jpeg, 42).expect("jpeg");
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "jpeg 魔数");
        let webp = encode(&out, OutputFormat::Webp, 42).expect("webp");
        assert_eq!(&webp[..4], b"RIFF");
        assert_eq!(&webp[8..12], b"WEBP");
    }

    #[test]
    fn decode_errors_are_readable_messages() {
        assert_eq!(
            decode_base64("").unwrap_err(),
            "Image file is empty (0 bytes)"
        );
        assert_eq!(
            decode_base64("!!!not-base64!!!").unwrap_err(),
            "invalid base64 image data"
        );
        // 合法 base64 但不是图片。
        let hello = base64::engine::general_purpose::STANDARD.encode("hello");
        assert_eq!(
            decode_rgba(&hello).unwrap_err(),
            "Unable to decode image data"
        );
        // 合法 base64 解出 0 字节。
        let empty = base64::engine::general_purpose::STANDARD.encode("");
        assert_eq!(
            decode_base64(&empty).unwrap_err(),
            "Image file is empty (0 bytes)"
        );
    }
}
