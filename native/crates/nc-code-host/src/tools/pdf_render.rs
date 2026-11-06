//! Poppler spawn + JPEG preparation for PDF `pages` rendering.
//!
//! Split from `read_pdf.rs` (range parsing + orchestration) to keep each file
//! under the 400-line limit. Spawn discipline mirrors `tools/bash.rs`: piped
//! capture threads + watchdog that kills the tree (`nc_code_proc::kill_tree`)
//! on timeout/cancel.

use std::io::Read as IoRead;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::read_pdf::{
    PageRange, PdfError, READ_PDF_AVAILABILITY_TIMEOUT_MS, READ_PDF_INFO_TIMEOUT_MS,
    READ_PDF_MAX_PAGES_PER_REQUEST, READ_PDF_RENDER_TIMEOUT_MS,
};

const PDF_RENDER_DPI: &str = "100";
const POPPLER_MISSING: &str = "pdftoppm is not installed. Install poppler-utils (e.g. `brew install poppler` or `apt-get install poppler-utils`) to enable PDF page rendering.";
static PDF_TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

struct CmdOutput {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn spawn_reader(
    pipe: impl IoRead + Send + 'static,
    buf: Arc<Mutex<Vec<u8>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut pipe = pipe;
        let mut chunk = [0u8; 8 * 1024];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut g) = buf.lock() {
                        g.extend_from_slice(&chunk[..n]);
                    }
                }
                Err(_) => break,
            }
        }
    })
}

fn run_with_timeout(
    file: &str,
    args: &[&str],
    timeout_ms: u64,
    cancelled: &Arc<AtomicBool>,
) -> Result<CmdOutput, PdfError> {
    let mut child = Command::new(file)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| PdfError::Unavailable(POPPLER_MISSING.to_string()))?;
    let pid = child.id();
    let stdout_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut readers = Vec::new();
    if let Some(pipe) = child.stdout.take() {
        readers.push(spawn_reader(pipe, Arc::clone(&stdout_buf)));
    }
    if let Some(pipe) = child.stderr.take() {
        readers.push(spawn_reader(pipe, Arc::clone(&stderr_buf)));
    }
    let timed_out = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&timed_out);
    let cancel_ref = Arc::clone(cancelled);
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if cancel_ref.load(Ordering::SeqCst) || Instant::now() >= deadline {
                flag.store(true, Ordering::SeqCst);
                let _ = nc_code_proc::kill_tree(pid, 2_000);
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    let status = child
        .wait()
        .map_err(|e| PdfError::IoError(format!("wait {file}: {e}")))?;
    for r in readers {
        let _ = r.join();
    }
    let _ = nc_code_proc::kill_tree(pid, 2_000);
    if cancelled.load(Ordering::SeqCst) {
        return Err(PdfError::Cancelled);
    }
    let stdout = String::from_utf8_lossy(&stdout_buf.lock().map(|g| g.clone()).unwrap_or_default())
        .into_owned();
    let stderr = String::from_utf8_lossy(&stderr_buf.lock().map(|g| g.clone()).unwrap_or_default())
        .into_owned();
    if timed_out.load(Ordering::SeqCst) {
        return Err(PdfError::Timeout("operation"));
    }
    Ok(CmdOutput {
        exit_code: status.code(),
        stdout,
        stderr,
    })
}

fn is_available_probe(out: &CmdOutput) -> bool {
    match out.exit_code {
        Some(0) => true,
        Some(code) => code != 127 && !out.stderr.is_empty(),
        None => false,
    }
}

fn ensure_poppler_available(cancelled: &Arc<AtomicBool>) -> Result<(), PdfError> {
    static CONFIRMED: std::sync::OnceLock<AtomicBool> = std::sync::OnceLock::new();
    if CONFIRMED.get().is_some_and(|f| f.load(Ordering::SeqCst)) {
        return Ok(());
    }
    let out = run_with_timeout(
        "pdftoppm",
        &["-v"],
        READ_PDF_AVAILABILITY_TIMEOUT_MS,
        cancelled,
    )?;
    if !is_available_probe(&out) {
        return Err(PdfError::Unavailable(POPPLER_MISSING.to_string()));
    }
    CONFIRMED
        .get_or_init(|| AtomicBool::new(false))
        .store(true, Ordering::SeqCst);
    Ok(())
}

pub fn pdf_page_count(pdf_path: &Path, cancelled: &Arc<AtomicBool>) -> Option<u32> {
    let pdf_str = pdf_path.to_string_lossy().into_owned();
    let out = run_with_timeout(
        "pdfinfo",
        &[pdf_str.as_str()],
        READ_PDF_INFO_TIMEOUT_MS,
        cancelled,
    )
    .ok()?;
    if out.exit_code != Some(0) {
        return None;
    }
    super::read_pdf::parse_pdfinfo_pages(&out.stdout)
}

#[derive(Debug, Clone)]
pub struct RenderedPage {
    pub page_number: u32,
    pub jpeg_bytes: Vec<u8>,
}

pub fn classify_render_error(
    stderr: &str,
    first: u32,
    last: Option<u32>,
    file_path: &str,
) -> PdfError {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("password") {
        return PdfError::ProcessFailed(
            "PDF is password-protected. Please provide an unprotected version.".to_string(),
        );
    }
    if let Some(idx) = lower.find("last page (") {
        let tail = &lower[idx + "last page (".len()..];
        let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(total) = digits.parse::<u32>() {
            if total == 0 {
                return PdfError::Corrupted(
                    "PDF reports 0 pages (empty page tree). The PDF may be invalid.".to_string(),
                );
            }
            let req = match last {
                Some(l) if l == first => format!("page {first}"),
                Some(l) => format!("pages {first}-{l}"),
                None => format!("pages {first}-end"),
            };
            let example_end = total.min(READ_PDF_MAX_PAGES_PER_REQUEST);
            return PdfError::PageOutOfRange(format!(
                "Requested {req} is outside the document (PDF has {total} pages). Use a range within 1-{total}, maximum {READ_PDF_MAX_PAGES_PER_REQUEST} pages per request (e.g. pages: \"1-{example_end}\")."
            ));
        }
    }
    if lower.contains("permission error") && !lower.contains("command line error") {
        return PdfError::PermissionDenied(format!("Could not render PDF: {file_path}"));
    }
    if lower.contains("i/o error") && !lower.contains("command line error") {
        return PdfError::IoError(format!("Could not render PDF: {file_path}"));
    }
    if lower.contains("damaged")
        || lower.contains("corrupt")
        || lower.contains("invalid")
        || (lower.contains("syntax error") && lower.contains("couldn't"))
    {
        return PdfError::Corrupted("PDF file is corrupted or invalid.".to_string());
    }
    let detail = stderr.trim();
    let msg = if detail.is_empty() {
        "pdftoppm failed.".to_string()
    } else {
        format!("pdftoppm failed: {detail}")
    };
    PdfError::ProcessFailed(msg)
}

/// Error for a render that produced no pages. `job_spawn` discards
/// pdftoppm's stderr, so the wrong-range case is proven from the same
/// poppler path instead: `pdfinfo` page count vs the requested range.
/// Range beyond the document → `PageOutOfRange` (spec error); 0 pages →
/// `Corrupted` (empty page tree); unknown count → generic `Corrupted`
/// (never a fake success). `classify_render_error` stays the stderr
/// classifier for callers that capture process output.
fn empty_render_error(
    pdf_path: &Path,
    range: PageRange,
    cancelled: &Arc<AtomicBool>,
) -> PdfError {
    match pdf_page_count(pdf_path, cancelled) {
        Some(0) => PdfError::Corrupted(
            "PDF reports 0 pages (empty page tree). The PDF may be invalid.".to_string(),
        ),
        Some(total) if range.first > total => {
            let req = match range.last {
                Some(l) if l == range.first => format!("page {}", range.first),
                Some(l) => format!("pages {}-{l}", range.first),
                None => format!("pages {}-end", range.first),
            };
            let example_end = total.min(READ_PDF_MAX_PAGES_PER_REQUEST);
            PdfError::PageOutOfRange(format!(
                "Requested {req} is outside the document (PDF has {total} pages). Use a range within 1-{total}, maximum {READ_PDF_MAX_PAGES_PER_REQUEST} pages per request (e.g. pages: \"1-{example_end}\")."
            ))
        }
        _ => PdfError::Corrupted(
            "pdftoppm produced no output pages. The PDF may be invalid.".to_string(),
        ),
    }
}

pub fn rendered_page_number(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    if !lower.ends_with(".jpg") {
        return None;
    }
    let stem = &lower[..lower.len() - 4];
    let (_, digits) = stem.rsplit_once('-')?;
    let page: u32 = digits.parse().ok()?;
    if page >= 1 {
        Some(page)
    } else {
        None
    }
}

pub fn render_pages(
    pdf_path: &Path,
    range: PageRange,
    cancelled: &Arc<AtomicBool>,
) -> Result<Vec<RenderedPage>, PdfError> {
    ensure_poppler_available(cancelled)?;
    let seq = PDF_TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("zcode-read-pdf-{}-{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| PdfError::IoError(format!("mk temp dir: {e}")))?;
    struct Guard(std::path::PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _guard = Guard(dir.clone());
    let first = range.first.to_string();
    let last = range
        .last
        .map(|v| v.to_string())
        .unwrap_or_else(|| u32::MAX.to_string());
    let prefix = dir.join("page");
    let pdf_str = pdf_path.to_string_lossy().into_owned();
    let prefix_str = prefix.to_string_lossy().into_owned();
    // Render via `nc_code_proc::job_spawn` (Job Object scopes the whole tree;
    // `kill_tree` on timeout/cancel). pdftoppm's output is files, so stdio
    // capture is unnecessary: success = produced `page-*.jpg` + process died.
    let args: Vec<String> = vec![
        "-jpeg".to_string(),
        "-r".to_string(),
        PDF_RENDER_DPI.to_string(),
        "-f".to_string(),
        first.clone(),
        "-l".to_string(),
        last.clone(),
        pdf_str.clone(),
        prefix_str.clone(),
    ];
    let (pid, _job) =
        nc_code_proc::job_spawn("pdftoppm", &args, None, None).map_err(|e| match e {
            nc_code_proc::SpawnError::Setup(_) => {
                PdfError::Unavailable(POPPLER_MISSING.to_string())
            }
            nc_code_proc::SpawnError::Assign { .. } => {
                PdfError::Unavailable(POPPLER_MISSING.to_string())
            }
        })?;
    let deadline = Instant::now() + Duration::from_millis(READ_PDF_RENDER_TIMEOUT_MS);
    let mut timed_out = false;
    while nc_code_proc::pid_alive(pid) {
        if cancelled.load(Ordering::SeqCst) {
            let _ = nc_code_proc::kill_tree(pid, 2_000);
            return Err(PdfError::Cancelled);
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = nc_code_proc::kill_tree(pid, 2_000);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = nc_code_proc::kill_tree(pid, 2_000);
    if cancelled.load(Ordering::SeqCst) {
        return Err(PdfError::Cancelled);
    }
    if timed_out {
        return Err(PdfError::Timeout("operation"));
    }
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| PdfError::IoError(format!("list rendered pages: {e}")))?;
    let mut pages: Vec<(u32, std::path::PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if let Some(num) = rendered_page_number(name) {
                pages.push((num, path));
            }
        }
    }
    pages.sort_by_key(|(num, _)| *num);
    if pages.is_empty() {
        return Err(empty_render_error(pdf_path, range, cancelled));
    }
    let mut rendered = Vec::new();
    for (page_number, path) in pages {
        let bytes = std::fs::read(&path)
            .map_err(|e| PdfError::IoError(format!("read rendered page {page_number}: {e}")))?;
        if cancelled.load(Ordering::SeqCst) {
            return Err(PdfError::Cancelled);
        }
        rendered.push(RenderedPage {
            page_number,
            jpeg_bytes: bytes,
        });
    }
    Ok(rendered)
}

pub use super::pdf_image::{prepare_jpeg_for_model, PreparedImage};

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn error_classification_covers_documented_classes() {
        assert!(matches!(
            classify_render_error("I/O Error: Couldn't open file 'a.pdf'", 1, Some(2), "a.pdf"),
            PdfError::IoError(_)
        ));
        assert!(matches!(
            classify_render_error("Permission Error: denied", 1, None, "a.pdf"),
            PdfError::PermissionDenied(_)
        ));
        assert!(matches!(
            classify_render_error(
                "Wrong page range given: the last page (7) is past the end",
                10,
                Some(20),
                "a.pdf"
            ),
            PdfError::PageOutOfRange(_)
        ));
        assert!(matches!(
            classify_render_error("Command Line Error: damaged input", 1, None, "a.pdf"),
            PdfError::Corrupted(_)
        ));
        assert!(matches!(
            classify_render_error("password required", 1, None, "a.pdf"),
            PdfError::ProcessFailed(_)
        ));
        assert!(matches!(
            classify_render_error("weird unknown failure", 1, None, "a.pdf"),
            PdfError::ProcessFailed(_)
        ));
    }
    #[test]
    fn rendered_page_number_uses_jpg_suffix() {
        assert_eq!(rendered_page_number("page-1.jpg"), Some(1));
        assert_eq!(rendered_page_number("page-12.JPG"), Some(12));
        assert_eq!(rendered_page_number("page-01.jpg"), Some(1));
        assert_eq!(rendered_page_number("page-0.jpg"), None);
        assert_eq!(rendered_page_number("page-1.png"), None);
    }
}
