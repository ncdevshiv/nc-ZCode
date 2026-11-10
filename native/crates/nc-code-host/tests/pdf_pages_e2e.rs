//! Real-PDF end-to-end for `Read` `pages` (spec §3).
//!
//! Writes a minimal but structurally valid 1-page PDF (correct xref offsets)
//! and drives the real `ReadTool` → `pdfinfo`/`pdftoppm` spawn pipeline.
//! When poppler is installed the test proves a successful render
//! (`parts[0].pageNumber == 1`, JPEG payload); when poppler is absent it
//! proves the honest fail-closed configuration error (never the removed
//! generic rejection). Both branches exercise the same real spawn path.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use nc_code_host::tools::fs_tools::ReadTool;
use nc_code_host::tools::{Tool, ToolContext};
use serde_json::json;

fn test_ctx(root: &Path) -> ToolContext {
    ToolContext {
        workspace_root: root.to_path_buf(),
        full_access: true,
        interactive: false,
        trace_id: "t".to_string(),
        turn_id: "turn".to_string(),
        read_state: Arc::new(Mutex::new(HashSet::new())),
        cancel: Arc::new(AtomicBool::new(false)),
        todos: nc_code_host::tools::todo::new_todo_store(),
        provider_config: None,
        session_id: "test-session".to_string(),
        model_ref: None,
        tool_call_id: None,
        plan_mode: false,
        ask_user: None,
        plan: None,
        subagent: None,
        background: None,
        sessions: None,
        workflow: None,
        automation: None,
        browser: None,
        computer: None,
        offpeak: None,
    }
}

fn temp_workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zcode-pdf-real-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("workspace");
    dir
}

/// Minimal valid 1-page PDF: header, 4 objects, xref with correct offsets,
/// trailer. Poppler's `pdfinfo`/`pdftoppm` accept this shape.
fn minimal_one_page_pdf() -> Vec<u8> {
    fn push(buf: &mut Vec<u8>, bytes: &[u8]) {
        buf.extend_from_slice(bytes);
    }
    let mut buf = Vec::new();
    let mut offsets: Vec<u64> = Vec::new();
    push(&mut buf, b"%PDF-1.4\n");
    let objs = [
        "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n",
        "2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n",
        "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\nendobj\n",
        "4 0 obj\n<< /Length 44 >>\nstream\nBT /F1 24 Tf 100 700 Td (Hello) Tj ET\nendstream\nendobj\n",
        "5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n",
    ];
    for obj in objs {
        offsets.push(buf.len() as u64);
        push(&mut buf, obj.as_bytes());
    }
    let xref_pos = buf.len() as u64;
    push(&mut buf, b"xref\n0 6\n");
    push(&mut buf, b"0000000000 65535 f \n");
    for off in &offsets {
        push(&mut buf, format!("{:010} 00000 n \n", off).as_bytes());
    }
    push(&mut buf, b"trailer\n<< /Size 6 /Root 1 0 R >>\n");
    push(
        &mut buf,
        format!("startxref\n{}\n%%EOF\n", xref_pos).as_bytes(),
    );
    buf
}

fn poppler_present() -> bool {
    // Probe without requiring the full render path: same availability check
    // the renderer performs (`pdftoppm -v` succeeds or prints version).
    std::process::Command::new("pdftoppm")
        .arg("-v")
        .output()
        .map(|out| out.status.success() || !out.stderr.is_empty())
        .unwrap_or(false)
}

#[test]
fn read_pages_with_real_pdf() {
    let root = temp_workspace("real");
    let ctx = test_ctx(&root);
    let pdf_bytes = minimal_one_page_pdf();
    assert!(pdf_bytes.starts_with(b"%PDF-"));
    std::fs::write(root.join("real.pdf"), &pdf_bytes).expect("write pdf");

    // Native read works without poppler (magic + size gates only).
    let native = ReadTool.execute(&json!({"file_path": "real.pdf"}), &ctx);
    // Native read may fail only if pdfinfo (when present) reports >10 pages
    // (it reports 1 here) — so it must succeed in both environments.
    let native_out = native.expect("native read of 1-page PDF");
    let parsed: serde_json::Value = serde_json::from_str(&native_out).expect("json");
    assert_eq!(parsed["type"], "pdf");
    assert_eq!(parsed["originalSize"], pdf_bytes.len());

    // Pages read: real spawn. With poppler → parts success; without → the
    // poppler-missing configuration error (never the removed generic text).
    match ReadTool.execute(&json!({"file_path": "real.pdf", "pages": "1-1"}), &ctx) {
        Ok(out) => {
            let parts: serde_json::Value = serde_json::from_str(&out).expect("parts json");
            eprintln!(
                "pdf-e2e branch: rendered numParts={} base64_len={}",
                parts["numParts"],
                parts["pages"][0]["base64"].as_str().unwrap_or("").len()
            );
            assert_eq!(parts["type"], "parts");
            assert_eq!(parts["numParts"], 1);
            assert_eq!(parts["pages"][0]["pageNumber"], 1);
            assert_eq!(parts["pages"][0]["mimeType"], "image/jpeg");
            assert!(!parts["pages"][0]["base64"]
                .as_str()
                .unwrap_or("")
                .is_empty());
            assert!(poppler_present(), "rendered but poppler probe says absent");
        }
        Err(error) => {
            eprintln!("pdf-e2e branch: fail-closed {}", error.message);
            assert!(
                !poppler_present(),
                "render failed but poppler probe says present: {}",
                error.message
            );
            assert!(
                error.message.contains("pdftoppm")
                    || error.message.contains("poppler")
                    || error.message.contains("corrupted")
                    || error.message.contains("pdftoppm failed"),
                "unexpected pages error: {}",
                error.message
            );
            assert!(
                !error.message.contains("not available in this Rust host"),
                "old rejection must stay gone"
            );
        }
    }
    // Wrong range on a 1-page document: with poppler the render attempt
    // must surface the spec page-out-of-range error (proven via the
    // `pdfinfo` page count, never a generic corruption message).
    if poppler_present() {
        let wrong = ReadTool.execute(&json!({"file_path": "real.pdf", "pages": "10"}), &ctx);
        let error = wrong.expect_err("range past the 1-page document must fail");
        eprintln!("pdf-e2e wrong-range branch: {}", error.message);
        assert!(
            error.message.contains("outside the document")
                && error.message.contains("PDF has 1 pages"),
            "wrong range must report page-out-of-range with Pages:N, got: {}",
            error.message
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}
