//! Request/response bridge between the Rust host's browser lifecycle commands
//! and the renderer-owned tab controller.
//!
//! The renderer owns the side-pane tab state, so `listUserTabs`/`activateTab`/
//! `close` cannot be answered by the shell alone. The shell emits
//! `zcode:browser-use-lifecycle-request` to the main webview, the renderer
//! handles it with `getBrowserUseLifecycleController()` and calls the
//! `browser_use_lifecycle_result` command with the same `requestId`.
//!
//! Requests are bounded and time out explicitly; a missing renderer listener
//! returns `browser_use_bridge_timeout` rather than hanging the agent turn.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tokio::sync::oneshot;

pub const REQUEST_EVENT: &str = "zcode:browser-use-lifecycle-request";
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PENDING: usize = 32;

#[derive(Default)]
pub struct BrowserUseBridgeState {
    next_id: AtomicU64,
    pending: Mutex<HashMap<String, oneshot::Sender<Value>>>,
}

impl BrowserUseBridgeState {
    /// Allocate a request id and park a response slot. Returns `None` when the
    /// pending table is full (the caller reports `browser_use_bridge_busy`).
    fn register(&self) -> Option<(String, oneshot::Receiver<Value>)> {
        let mut pending = match self.pending.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if pending.len() >= MAX_PENDING {
            return None;
        }
        let id = format!(
            "browser-use-{}",
            self.next_id.fetch_add(1, Ordering::Relaxed)
        );
        let (sender, receiver) = oneshot::channel();
        pending.insert(id.clone(), sender);
        Some((id, receiver))
    }

    fn take(&self, request_id: &str) -> Option<oneshot::Sender<Value>> {
        let mut pending = match self.pending.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        pending.remove(request_id)
    }

    /// Resolve a pending request; returns `false` when the id is unknown or the
    /// receiver was already dropped.
    pub fn complete(&self, request_id: &str, result: Value) -> bool {
        self.take(request_id)
            .map(|sender| sender.send(result).is_ok())
            .unwrap_or(false)
    }

    /// Emit one lifecycle request to the main webview and wait for the renderer.
    pub async fn request(&self, app: &AppHandle, command: Value) -> Result<Value, String> {
        let Some((request_id, receiver)) = self.register() else {
            return Err("browser_use_bridge_busy".to_string());
        };
        let payload = json!({
            "requestId": request_id,
            "command": command,
        });
        if let Err(error) = app.emit_to("main", REQUEST_EVENT, payload) {
            self.take(&request_id);
            return Err(format!("browser_use_bridge_emit_failed: {error}"));
        }
        match tokio::time::timeout(RESPONSE_TIMEOUT, receiver).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Err("browser_use_bridge_cancelled".to_string()),
            Err(_) => {
                self.take(&request_id);
                Err("browser_use_bridge_timeout".to_string())
            }
        }
    }
}

/// Renderer response endpoint for `REQUEST_EVENT`.
#[tauri::command]
pub fn browser_use_lifecycle_result(
    state: tauri::State<'_, std::sync::Arc<BrowserUseBridgeState>>,
    request_id: String,
    result: Value,
) -> bool {
    state.complete(&request_id, result)
}

/// Diagnostic entry point (`ZCODE_SHELL_BROWSER_DEBUG=1` only) used to verify
/// the renderer round-trip without starting a provider-backed agent turn.
#[tauri::command]
pub async fn browser_use_lifecycle_probe(
    app: AppHandle,
    command: Option<Value>,
) -> Result<Value, String> {
    if std::env::var("ZCODE_SHELL_BROWSER_DEBUG").ok().as_deref() != Some("1") {
        return Err("browser_use_debug_disabled".to_string());
    }
    use tauri::Manager;
    let bridge = app.state::<std::sync::Arc<BrowserUseBridgeState>>();
    bridge
        .request(
            &app,
            command.unwrap_or_else(|| json!({ "method": "listUserTabs" })),
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_a_pending_request() {
        let state = BrowserUseBridgeState::default();
        let (id, mut receiver) = state.register().expect("slot");
        assert!(state.complete(&id, json!({ "ok": true })));
        let value = receiver.try_recv().expect("result");
        assert_eq!(value["ok"], true);
        // A second completion is a no-op.
        assert!(!state.complete(&id, json!({ "ok": false })));
    }

    #[test]
    fn unknown_request_ids_are_rejected() {
        let state = BrowserUseBridgeState::default();
        assert!(!state.complete("missing", json!({ "ok": true })));
    }

    #[test]
    fn pending_table_is_bounded() {
        let state = BrowserUseBridgeState::default();
        let mut receivers = Vec::new();
        for _ in 0..MAX_PENDING {
            receivers.push(state.register().expect("slot"));
        }
        assert!(state.register().is_none());
        // Completing one frees a slot (keep the receiver alive so the send works).
        let (id, receiver) = receivers.pop().expect("receiver");
        assert!(state.complete(&id, json!({ "ok": true })));
        drop(receiver);
        assert!(state.register().is_some());
    }
}
