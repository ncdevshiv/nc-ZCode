//! Window screenshot for the Tauri shell.
//!
//! - Windows: WebView2 `CapturePreview` (the same Chromium compositor the
//!   Electron `webContents.capturePage` path used).
//! - macOS: `WKWebView.takeSnapshotWithConfiguration` → NSImage → PNG.
//! - Linux: `webkit_web_view_get_snapshot` (WebKitGTK ≥ 2.40) → cairo PNG.
//!
//! All platforms return a base64 PNG. A failed/empty capture returns `null`
//! rather than a fabricated image.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::Serialize;
use tauri::WebviewWindow;

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowScreenshotResult {
    pub data_base64: String,
    pub filename: String,
    pub content_type: String,
    pub size: usize,
}

fn screenshot_filename(now: SystemTime) -> String {
    let millis = now
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    format!("zcode-error-{millis}.png")
}

fn screenshot_result(bytes: Vec<u8>, now: SystemTime) -> Option<WindowScreenshotResult> {
    if bytes.is_empty() {
        return None;
    }
    Some(WindowScreenshotResult {
        data_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        filename: screenshot_filename(now),
        content_type: "image/png".to_string(),
        size: bytes.len(),
    })
}

#[cfg(windows)]
fn capture_webview_png(window: &WebviewWindow) -> Result<Vec<u8>, String> {
    use std::sync::mpsc;
    use webview2_com::CapturePreviewCompletedHandler;
    use webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG;
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
    use windows::Win32::System::Com::{IStream, STREAM_SEEK_SET};

    let (sender, receiver) = mpsc::channel::<Result<Vec<u8>, String>>();
    window
        .with_webview(move |platform_webview| {
            let started = (|| -> Result<(), String> {
                let stream: IStream =
                    unsafe { CreateStreamOnHGlobal(HGLOBAL(std::ptr::null_mut()), true) }
                        .map_err(|error| error.to_string())?;
                let handler_stream = stream.clone();
                let handler_sender = sender.clone();
                let handler = CapturePreviewCompletedHandler::create(Box::new(move |error_code| {
                    if error_code.is_err() {
                        let _ = handler_sender.send(Err("capture_failed".to_string()));
                        return Ok(());
                    }
                    let result = (|| -> Result<Vec<u8>, String> {
                        unsafe { handler_stream.Seek(0, STREAM_SEEK_SET, None) }
                            .map_err(|error| error.to_string())?;
                        let mut bytes = Vec::new();
                        let mut buffer = [0u8; 8192];
                        loop {
                            let mut read = 0u32;
                            let read_result = unsafe {
                                handler_stream.Read(
                                    buffer.as_mut_ptr() as *mut core::ffi::c_void,
                                    buffer.len() as u32,
                                    Some(&mut read),
                                )
                            };
                            if read_result.is_err() || read == 0 {
                                break;
                            }
                            bytes.extend_from_slice(&buffer[..read as usize]);
                        }
                        Ok(bytes)
                    })();
                    let _ = handler_sender.send(result);
                    Ok(())
                }));
                let controller = platform_webview.controller();
                let core =
                    unsafe { controller.CoreWebView2() }.map_err(|error| error.to_string())?;
                unsafe {
                    core.CapturePreview(
                        COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
                        &stream,
                        &handler,
                    )
                }
                .map_err(|error| error.to_string())
            })();
            if let Err(error) = started {
                let _ = sender.send(Err(error));
            }
        })
        .map_err(|error| error.to_string())?;
    receiver
        .recv_timeout(CAPTURE_TIMEOUT)
        .map_err(|_| "capture_timeout".to_string())?
}

#[cfg(target_os = "macos")]
fn capture_webview_png(window: &WebviewWindow) -> Result<Vec<u8>, String> {
    use std::sync::mpsc;

    use block2::RcBlock;
    use objc2::runtime::AnyObject;
    use objc2_app_kit::{
        NSBitmapImageFileType, NSBitmapImageRep, NSBitmapImageRepPropertyKey, NSImage,
    };
    use objc2_core_foundation::CFData;
    use objc2_foundation::{NSDictionary, NSError};
    use objc2_web_kit::{WKSnapshotConfiguration, WKWebView};

    let (sender, receiver) = mpsc::channel::<Result<Vec<u8>, String>>();
    window
        .with_webview(move |platform_webview| {
            // `with_webview` runs on the main thread; WKWebView/AppKit calls
            // must stay on it, including the snapshot completion handler.
            let webview: &WKWebView = unsafe { &*(platform_webview.inner() as *mut WKWebView) };
            let configuration = unsafe { WKSnapshotConfiguration::new() };
            let handler_sender = sender.clone();
            let handler = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
                let result = (|| -> Result<Vec<u8>, String> {
                    if !error.is_null() || image.is_null() {
                        return Err("capture_failed".to_string());
                    }
                    let image = unsafe { &*image };
                    let tiff = image
                        .TIFFRepresentation()
                        .ok_or_else(|| "capture_failed".to_string())?;
                    let representation = NSBitmapImageRep::imageRepWithData(&tiff)
                        .ok_or_else(|| "capture_failed".to_string())?;
                    let properties = NSDictionary::<NSBitmapImageRepPropertyKey, AnyObject>::new();
                    let png = unsafe {
                        representation.representationUsingType_properties(
                            NSBitmapImageFileType::PNG,
                            &properties,
                        )
                    }
                    .ok_or_else(|| "capture_failed".to_string())?;
                    let data: &CFData = png.as_ref();
                    Ok(data.to_vec())
                })();
                let _ = handler_sender.send(result);
            });
            unsafe {
                webview.takeSnapshotWithConfiguration_completionHandler(
                    Some(&configuration),
                    &handler,
                );
            }
        })
        .map_err(|error| error.to_string())?;
    receiver
        .recv_timeout(CAPTURE_TIMEOUT)
        .map_err(|_| "capture_timeout".to_string())?
}

#[cfg(target_os = "linux")]
fn capture_webview_png(window: &WebviewWindow) -> Result<Vec<u8>, String> {
    use std::sync::mpsc;

    use webkit2gtk::{SnapshotOptions, SnapshotRegion, WebViewExt};

    let (sender, receiver) = mpsc::channel::<Result<Vec<u8>, String>>();
    window
        .with_webview(move |platform_webview| {
            // WebKitGTK requires the snapshot call on the GTK main thread;
            // `with_webview` dispatches there.
            let webview = platform_webview.inner();
            let callback_sender = sender.clone();
            webview.snapshot(
                SnapshotRegion::Visible,
                SnapshotOptions::NONE,
                None::<&webkit2gtk::gio::Cancellable>,
                move |result| {
                    let result = match result {
                        Ok(surface) => {
                            let mut bytes = Vec::new();
                            surface
                                .write_to_png(&mut bytes)
                                .map(|()| bytes)
                                .map_err(|error| error.to_string())
                        }
                        Err(error) => Err(error.to_string()),
                    };
                    let _ = callback_sender.send(result);
                },
            );
        })
        .map_err(|error| error.to_string())?;
    receiver
        .recv_timeout(CAPTURE_TIMEOUT)
        .map_err(|_| "capture_timeout".to_string())?
}

/// `IPlatformService.captureWindowScreenshot`: Windows/macOS/Linux return a
/// base64 PNG; unsupported targets return `null` explicitly.
#[tauri::command]
pub async fn capture_window_screenshot(window: WebviewWindow) -> Option<WindowScreenshotResult> {
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = window;
        return None;
    }
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    {
        let result = tauri::async_runtime::spawn_blocking(move || capture_webview_png(&window))
            .await
            .ok()?
            .ok()?;
        screenshot_result(result, SystemTime::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_png_result_with_base64_and_metadata() {
        let bytes = b"\x89PNG\r\n\x1a\nfake".to_vec();
        let result = screenshot_result(bytes.clone(), UNIX_EPOCH).expect("result");
        assert_eq!(result.content_type, "image/png");
        assert_eq!(result.size, bytes.len());
        assert!(result.filename.starts_with("zcode-error-"));
        assert!(result.filename.ends_with(".png"));
        assert!(!result.data_base64.is_empty());
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&result.data_base64)
                .unwrap(),
            bytes
        );
    }

    #[test]
    fn empty_capture_returns_none() {
        assert!(screenshot_result(Vec::new(), UNIX_EPOCH).is_none());
    }

    #[test]
    fn filename_uses_epoch_millis() {
        assert_eq!(
            screenshot_filename(UNIX_EPOCH + Duration::from_millis(1_704_067_200_123)),
            "zcode-error-1704067200123.png"
        );
    }
}
