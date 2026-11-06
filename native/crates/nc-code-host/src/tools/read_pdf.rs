//! PDF `pages` orchestration for the `Read` tool.
//!
//! Mirrors TS `core/src/tool/handlers/read-pdf.ts`: range parsing/validation,
//! page-count gating (20/request), and the `parts` output shape. The spawn +
//! image work lives in `pdf_render.rs` (Poppler via `std::process` +
//! `nc_code_proc::kill_tree`, JPEG resize via the `image` crate).

use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::AtomicBool;

use super::{ToolError, ToolErrorCode};

pub const READ_PDF_NATIVE_MAX_INPUT_BYTES: u64 = 20 * 1024 * 1024;
pub const READ_PDF_EXTRACT_MAX_INPUT_BYTES: u64 = 100 * 1024 * 1024;
pub const READ_PDF_NATIVE_MAX_PAGES: u32 = 10;
pub const READ_PDF_MAX_PAGES_PER_REQUEST: u32 = 20;
pub const READ_PDF_INFO_TIMEOUT_MS: u64 = 10_000;
pub const READ_PDF_AVAILABILITY_TIMEOUT_MS: u64 = 5_000;
pub const READ_PDF_RENDER_TIMEOUT_MS: u64 = 120_000;
pub const READ_PDF_TOOL_TIMEOUT_MS: u64 = READ_PDF_RENDER_TIMEOUT_MS + 30_000;

/// Parsed 1-indexed page range; `None` end means open-ended (`"1-"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRange {
    pub first: u32,
    pub last: Option<u32>,
}

/// Back-compat alias for call sites that used `RangeSnapshot`.
pub type RangeSnapshot = PageRange;

pub fn parse_page_range(value: &str) -> Option<PageRange> {
    let normalized = value.trim();
    if let Some(stripped) = normalized.strip_suffix('-') {
        if stripped.contains('-') || stripped.is_empty() {
            return None;
        }
        let first: u32 = stripped.parse().ok()?;
        if first < 1 {
            return None;
        }
        return Some(PageRange { first, last: None });
    }
    let mut parts = normalized.split('-');
    let first: u32 = parts.next()?.parse().ok()?;
    let last_raw = parts.next();
    if parts.next().is_some() {
        return None;
    }
    let last = match last_raw {
        Some(raw) => raw.parse::<u32>().ok()?,
        None => first,
    };
    if first < 1 || last < first {
        return None;
    }
    Some(PageRange {
        first,
        last: Some(last),
    })
}

pub fn range_page_count(range: PageRange) -> u32 {
    match range.last {
        Some(last) => last.saturating_sub(range.first) + 1,
        None => READ_PDF_MAX_PAGES_PER_REQUEST + 1,
    }
}

pub fn parse_pdfinfo_pages(stdout: &str) -> Option<u32> {
    for line in stdout.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed
            .strip_prefix("Pages:")
            .or_else(|| trimmed.strip_prefix("pages:"))
        else {
            continue;
        };
        if let Ok(count) = rest.trim().parse::<u32>() {
            return Some(count);
        }
    }
    None
}

#[derive(Debug, Clone)]
pub enum PdfError {
    Cancelled,
    Timeout(&'static str),
    Unavailable(String),
    Corrupted(String),
    PageOutOfRange(String),
    IoError(String),
    PermissionDenied(String),
    ProcessFailed(String),
}

impl PdfError {
    fn message(&self) -> String {
        match self {
            PdfError::Cancelled => "PDF page extraction was cancelled.".to_string(),
            PdfError::Timeout(what) => format!("PDF page extraction timed out after {what}."),
            PdfError::Unavailable(msg)
            | PdfError::Corrupted(msg)
            | PdfError::PageOutOfRange(msg)
            | PdfError::IoError(msg)
            | PdfError::PermissionDenied(msg)
            | PdfError::ProcessFailed(msg) => msg.clone(),
        }
    }
    pub fn to_tool_error(&self) -> ToolError {
        match self {
            PdfError::Cancelled => ToolError::new(ToolErrorCode::Cancelled, self.message()),
            PdfError::Timeout(_) => ToolError::new(ToolErrorCode::Timeout, self.message()),
            PdfError::Unavailable(_)
            | PdfError::IoError(_)
            | PdfError::PermissionDenied(_)
            | PdfError::ProcessFailed(_) => {
                ToolError::new(ToolErrorCode::ExecutionFailed, self.message())
            }
            PdfError::Corrupted(_) | PdfError::PageOutOfRange(_) => {
                ToolError::new(ToolErrorCode::InvalidInput, self.message())
            }
        }
    }
}

/// Whole-PDF native-read gate: when `pdfinfo` is installed and reports a page
/// count, enforce the 10-page native cap. Returns `Err(ToolError)` when the
/// document must be read with `pages`.
pub fn check_native_page_limit(
    pdf_path: &Path,
    cancelled: &std::sync::Arc<AtomicBool>,
) -> Result<(), ToolError> {
    if let Some(count) = super::pdf_render::pdf_page_count(pdf_path, cancelled) {
        if count > READ_PDF_NATIVE_MAX_PAGES {
            return Err(ToolError::new(
                ToolErrorCode::InvalidInput,
                format!(
                    "This PDF has {count} pages, which is too many to read at once. Use the pages parameter to read specific page ranges (e.g., pages: \"1-5\"). Maximum {READ_PDF_MAX_PAGES_PER_REQUEST} pages per request."
                ),
            ));
        }
    }
    Ok(())
}

/// Full `pages` branch: validation + render + image preparation.
/// Returns the parts JSON string consumed by the model (TS `ReadPartsOutput` shape).
pub fn read_pdf_pages_json(
    raw_path: &str,
    pdf_path: &Path,
    pages_input: &str,
    size_bytes: u64,
    cancelled: &std::sync::Arc<AtomicBool>,
) -> Result<String, ToolError> {
    if size_bytes > READ_PDF_EXTRACT_MAX_INPUT_BYTES {
        return Err(ToolError::new(
            ToolErrorCode::InvalidInput,
            format!(
                "PDF file exceeds maximum allowed size for text extraction ({} bytes).",
                READ_PDF_EXTRACT_MAX_INPUT_BYTES
            ),
        ));
    }
    let range = parse_page_range(pages_input).ok_or_else(|| {
        ToolError::new(
            ToolErrorCode::InvalidInput,
            format!(
                "Invalid pages parameter: \"{pages_input}\". Use formats like \"1-5\", \"3\", or \"10-20\". Pages are 1-indexed."
            ),
        )
    })?;
    if range_page_count(range) > READ_PDF_MAX_PAGES_PER_REQUEST {
        return Err(ToolError::new(
            ToolErrorCode::InvalidInput,
            format!(
                "Page range \"{pages_input}\" exceeds maximum of {READ_PDF_MAX_PAGES_PER_REQUEST} pages per request. Please use a smaller range."
            ),
        ));
    }
    let rendered = super::pdf_render::render_pages(pdf_path, range, cancelled)
        .map_err(|e| e.to_tool_error())?;
    let mut prepared_pages: Vec<Value> = Vec::new();
    let mut ordered = rendered;
    ordered.sort_by_key(|p| p.page_number);
    for page in ordered {
        let original_size = page.jpeg_bytes.len() as u64;
        let prepared = super::pdf_render::prepare_jpeg_for_model(&page.jpeg_bytes)?;
        let transformed_size = prepared.bytes.len() as u64;
        prepared_pages.push(json!({
            "type": "image",
            "pageNumber": page.page_number,
            "base64": super::read_image::base64_encode(&prepared.bytes),
            "mimeType": "image/jpeg",
            "originalSize": original_size,
            "transformedSize": transformed_size,
            "resized": prepared.resized,
            "compressed": prepared.compressed,
            "compressionStrategy": if prepared.resized { "resize" } else { "none" },
            "dimensions": {
                "originalWidth": prepared.original_width,
                "originalHeight": prepared.original_height,
                "displayWidth": prepared.display_width,
                "displayHeight": prepared.display_height,
            },
        }));
    }
    Ok(json!({
        "type": "parts",
        "filePath": raw_path,
        "numParts": prepared_pages.len(),
        "originalSize": size_bytes,
        "pages": prepared_pages,
    })
    .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn range_parsing_matches_ts() {
        assert_eq!(
            parse_page_range("1-5"),
            Some(PageRange {
                first: 1,
                last: Some(5)
            })
        );
        assert_eq!(
            parse_page_range("3"),
            Some(PageRange {
                first: 3,
                last: Some(3)
            })
        );
        assert_eq!(
            parse_page_range("10-20"),
            Some(PageRange {
                first: 10,
                last: Some(20)
            })
        );
        assert_eq!(
            parse_page_range("1-"),
            Some(PageRange {
                first: 1,
                last: None
            })
        );
        assert_eq!(
            parse_page_range("  2-4  "),
            Some(PageRange {
                first: 2,
                last: Some(4)
            })
        );
        assert_eq!(parse_page_range(""), None);
        assert_eq!(parse_page_range("0"), None);
        assert_eq!(parse_page_range("5-2"), None);
        assert_eq!(parse_page_range("1-2-3"), None);
        assert_eq!(
            range_page_count(PageRange {
                first: 1,
                last: None
            }),
            21
        );
        assert_eq!(
            range_page_count(PageRange {
                first: 1,
                last: Some(20)
            }),
            20
        );
    }
    #[test]
    fn pdfinfo_parsing_matches_poppler() {
        assert_eq!(parse_pdfinfo_pages("Title: x\nPages: 42\n"), Some(42));
        assert_eq!(parse_pdfinfo_pages("pages: 7\n"), Some(7));
        assert_eq!(parse_pdfinfo_pages("Title: x\n"), None);
        assert_eq!(parse_pdfinfo_pages("Pages: notanumber\n"), None);
    }
    #[test]
    fn pages_json_rejects_bad_range_and_oversize_before_spawn() {
        let dir = std::env::temp_dir().join(format!("zcode-pdf-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pdf = dir.join("doc.pdf");
        std::fs::write(&pdf, b"%PDF-1.4\n%%EOF\n").unwrap();
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let err = read_pdf_pages_json("doc.pdf", &pdf, "nope", 1024, &cancel).unwrap_err();
        assert!(matches!(err.code, ToolErrorCode::InvalidInput));
        assert!(err.message.contains("Invalid pages parameter"));
        let err = read_pdf_pages_json("doc.pdf", &pdf, "1-99", 1024, &cancel).unwrap_err();
        assert!(err.message.contains("exceeds maximum"));
        let err = read_pdf_pages_json("doc.pdf", &pdf, "1-", 1024, &cancel).unwrap_err();
        assert!(err.message.contains("exceeds maximum"));
        let err = read_pdf_pages_json(
            "doc.pdf",
            &pdf,
            "1-2",
            READ_PDF_EXTRACT_MAX_INPUT_BYTES + 1,
            &cancel,
        )
        .unwrap_err();
        assert!(err.message.contains("exceeds maximum allowed size"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
