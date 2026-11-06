//! One in-process nc-code-host runtime per workspace, speaking the v4 JSON-RPC surface.
//!
//! Requests go through `HostServer::handle_line` (in-process, no child process); all
//! asynchronous frames land on the runtime's event bus. Responses are returned to the
//! caller and post-response frames are forwarded to the bus in order.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use nc_code_channels::EventBus;
use nc_code_host::provider_config::ProviderConfig;
use nc_code_host::HostServer;
use nc_code_rpc::WireValue;
use nc_code_service_core::ServiceError;
use serde_json::{json, Value};

pub struct RequestResult {
    pub result: Value,
    pub frames: Vec<Value>,
}

pub struct AgentRuntime {
    pub workspace_key: String,
    pub workspace_path: String,
    pub frames: EventBus,
    server: Arc<HostServer>,
    next_id: AtomicU64,
}

impl AgentRuntime {
    pub fn create(
        workspace: PathBuf,
        data_dir: PathBuf,
        provider: Option<ProviderConfig>,
        workspace_key: String,
        automation_port: Option<Arc<dyn nc_code_host::tools::AutomationPort>>,
        browser_port: Option<Arc<dyn nc_code_host::tools::browser::BrowserControlPort>>,
        computer_port: Option<Arc<dyn nc_code_host::tools::computer::ComputerControlPort>>,
    ) -> Result<Arc<Self>, ServiceError> {
        let workspace_path = workspace.to_string_lossy().into_owned();
        let (server, mut outbox) =
            HostServer::new(workspace, data_dir, provider, false, Vec::new()).map_err(|error| {
                ServiceError::new(format!("agent host startup failed: {error}"))
            })?;
        server.set_inline_responses(true);
        // The scheduled-task tool surface: the port is injected by the composition root (host does not depend on services-automation).
        if let Some(port) = automation_port {
            server.set_automation_port(port);
        }
        // Native browser control: absent = the `Browser` tool fails closed.
        if let Some(port) = browser_port {
            server.set_browser_port(port);
        }
        // Native computer use: absent = the `Computer` tool fails closed.
        if let Some(port) = computer_port {
            server.set_computer_port(port);
        }
        let frames = EventBus::default();
        let bus = frames.clone();
        tokio::spawn(async move {
            while let Some(line) = outbox.recv().await {
                bus.send(WireValue::String(line));
            }
        });
        server.resume_sessions();
        Ok(Arc::new(Self {
            workspace_key,
            workspace_path,
            frames,
            server,
            next_id: AtomicU64::new(1),
        }))
    }

    /// The session's runtime projection (read by the task list; does not trigger cold resume).
    pub fn session_view(&self, session_id: &str) -> Option<Value> {
        self.server.session_view(session_id)
    }

    /// Cold-read fallback: `v4/conversation/resync/subscribe` already goes through the host's cold resume,
    /// this adds one more attempt for sessions that "have meta but are not loaded" — `load_session` is the host's only
    /// cold-recovery entry point (nc-code-host/src/lib.rs:1091). Returns whether it got loaded.
    pub fn ensure_loaded(&self, session_id: &str) -> bool {
        if self.server.session_view(session_id).is_some() {
            return true;
        }
        self.server.load_session(session_id).is_some()
    }

    /// The session kind (subagent child sessions must be excluded from the task list).
    pub fn session_task_type(&self, session_id: &str) -> Option<String> {
        self.server.session_task_type(session_id)
    }

    /// Snapshot of the registered session ids.
    pub fn session_ids(&self) -> Vec<String> {
        self.server.session_ids()
    }

    /// The session body view (rows + fileChanges + meta).
    pub fn session_transcript_view(&self, session_id: &str) -> Option<Value> {
        self.server.session_transcript_view(session_id)
    }

    /// The session's cumulative usage.
    pub fn session_usage(&self, session_id: &str) -> Option<Value> {
        self.server.session_usage(session_id)
    }

    pub async fn request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<RequestResult, ServiceError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let line = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        })
        .to_string();
        let (response, post_frames) = self.server.handle_line(&line).await;
        let mut frames = Vec::new();
        for frame in &post_frames {
            if let Ok(value) = serde_json::from_str::<Value>(frame) {
                frames.push(value.clone());
                self.frames.send(WireValue::Json(value));
            }
        }
        let Some(response) = response else {
            return Err(ServiceError::new(format!(
                "agent host returned no response for {method}"
            )));
        };
        let value: Value = serde_json::from_str(&response)
            .map_err(|error| ServiceError::new(format!("invalid host response: {error}")))?;
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("agent host error");
            let code = error.get("code").cloned();
            let mut service_error = ServiceError::new(message.to_string());
            if let Some(code) = code {
                service_error = service_error.with_code_value(code);
            }
            if let Some(data) = error.get("data") {
                service_error = service_error.with_data(data.clone());
            }
            return Err(service_error);
        }
        Ok(RequestResult {
            result: value.get("result").cloned().unwrap_or(Value::Null),
            frames,
        })
    }
}
