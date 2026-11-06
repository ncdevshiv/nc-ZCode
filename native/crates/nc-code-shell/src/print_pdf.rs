//! macOS/Linux `printPageToPdf` implementations for the Tauri shell.
//!
//! - **macOS**: `WKWebView.createPDFWithConfiguration` (macOS 11+) returns the
//!   page as PDF `NSData`; the completion handler runs on the main thread and
//!   hands the bytes back through a channel.
//! - **Linux**: WebKitGTK's `WebKitPrintOperation` is driven with GTK's
//!   print-to-file settings (`Print to File` + `output-uri` + `pdf`), then the
//!   produced file is read back. `webkit_print_operation_print()` runs the
//!   operation without a dialog, so no user interaction is required.
//!
//! Both paths validate the PDF magic (`%PDF-`) before returning bytes; a missing
//! or empty file is `print_failed`, never a fabricated document. The Windows
//! path (`WebView2.PrintToPdf`) stays in `main.rs` unchanged.
//!
//! This module is compiled on macOS/Linux and in test builds; on Windows release
//! builds it is excluded (no dead code), so the target-specific branches are
//! compile-parsed only on this host.

use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::time::Duration;

#[cfg(target_os = "macos")]
const PRINT_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn output_path() -> PathBuf {
    std::env::temp_dir().join(format!("zcode-print-{}.pdf", std::process::id()))
}

/// Reject empty/non-PDF output instead of returning a broken document.
pub(crate) fn validate_pdf(bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    if bytes.is_empty() || !bytes.starts_with(b"%PDF-") {
        return Err("print_failed".to_string());
    }
    Ok(bytes)
}

/// `WKWebView.createPDFWithConfiguration:completionHandler:` → PDF bytes.
#[cfg(target_os = "macos")]
pub fn print_page_to_pdf(window: &tauri::WebviewWindow) -> Result<Vec<u8>, String> {
    use std::sync::mpsc;

    use block2::RcBlock;
    use objc2::sel;
    use objc2_core_foundation::CFData;
    use objc2_foundation::{NSData, NSError, NSObjectProtocol};
    use objc2_web_kit::WKWebView;

    let (sender, receiver) = mpsc::channel::<Result<Vec<u8>, String>>();
    window
        .with_webview(move |platform_webview| {
            // AppKit/WebKit calls must stay on the main thread, including the
            // snapshot completion handler.
            let webview: &WKWebView = unsafe { &*(platform_webview.inner() as *mut WKWebView) };
            // `createPDFWithConfiguration:` is macOS 11+; fail explicitly on
            // older systems instead of calling an unavailable selector.
            if !webview.respondsToSelector(sel!(createPDFWithConfiguration:completionHandler:)) {
                let _ = sender.send(Err("print_unsupported".to_string()));
                return;
            }
            let handler_sender = sender.clone();
            let handler = RcBlock::new(move |data: *mut NSData, error: *mut NSError| {
                let result = (|| -> Result<Vec<u8>, String> {
                    if !error.is_null() || data.is_null() {
                        return Err("print_failed".to_string());
                    }
                    let data = unsafe { &*data };
                    let bytes: &CFData = data.as_ref();
                    Ok(bytes.to_vec())
                })();
                let _ = handler_sender.send(result);
            });
            unsafe {
                // `None` = the bounds of the currently displayed page.
                webview.createPDFWithConfiguration_completionHandler(None, &handler);
            }
        })
        .map_err(|error| error.to_string())?;
    let bytes = receiver
        .recv_timeout(PRINT_TIMEOUT)
        .map_err(|_| "print_timeout".to_string())??;
    validate_pdf(bytes)
}

/// WebKitGTK `WebKitPrintOperation` with GTK print-to-file settings.
#[cfg(target_os = "linux")]
pub fn print_page_to_pdf(window: &tauri::WebviewWindow) -> Result<Vec<u8>, String> {
    use std::sync::{Arc, Mutex};

    use webkit2gtk::PrintOperationExt;

    let path = output_path();
    let uri = tauri::Url::from_file_path(&path)
        .map_err(|_| "print_invalid_path".to_string())?
        .to_string();
    let failure: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let failure_for_webview = Arc::clone(&failure);
    window
        .with_webview(move |platform_webview| {
            let webview = platform_webview.inner();
            let settings = gtk::PrintSettings::new();
            settings.set_printer("Print to File");
            settings.set(gtk::PRINT_SETTINGS_OUTPUT_FILE_FORMAT.as_str(), Some("pdf"));
            settings.set(gtk::PRINT_SETTINGS_OUTPUT_URI.as_str(), Some(uri.as_str()));
            let operation = webkit2gtk::PrintOperation::new(&webview);
            operation.set_print_settings(&settings);
            let failed_slot = Arc::clone(&failure_for_webview);
            operation.connect_failed(move |_, error| {
                if let Ok(mut slot) = failed_slot.lock() {
                    *slot = Some(error.to_string());
                }
            });
            // `print()` runs the operation without a dialog and returns after
            // the job settles; the `failed` signal fires before it returns.
            operation.print();
        })
        .map_err(|error| error.to_string())?;
    if let Some(error) = failure.lock().ok().and_then(|slot| slot.clone()) {
        return Err(format!("print_failed: {error}"));
    }
    let bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
    let _ = std::fs::remove_file(&path);
    validate_pdf(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_real_pdf_bytes() {
        let pdf = b"%PDF-1.7\n1 0 obj\n<<>>\nendobj\n%%EOF\n".to_vec();
        assert_eq!(validate_pdf(pdf.clone()).unwrap(), pdf);
    }

    #[test]
    fn rejects_empty_and_non_pdf_payloads() {
        assert_eq!(validate_pdf(Vec::new()).unwrap_err(), "print_failed");
        assert_eq!(
            validate_pdf(b"<html></html>".to_vec()).unwrap_err(),
            "print_failed"
        );
        assert_eq!(
            validate_pdf(b"PDF-1.7".to_vec()).unwrap_err(),
            "print_failed"
        );
    }

    #[test]
    fn output_path_is_a_unique_pdf_in_the_temp_dir() {
        let path = output_path();
        assert_eq!(
            path.extension().and_then(|value| value.to_str()),
            Some("pdf")
        );
        assert!(path
            .to_string_lossy()
            .contains(&std::process::id().to_string()));
        assert_eq!(path.parent(), Some(std::env::temp_dir().as_path()));
    }
}
