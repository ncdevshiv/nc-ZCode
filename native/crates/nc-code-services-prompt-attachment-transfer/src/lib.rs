//! `prompt-attachment-transfer` service channel — local implementation.
//!
//! Port of TS `createLocalPromptAttachmentTransferService`:
//! - `stage`: validates local path and returns a ref (no remote staging needed locally)
//! - `adopt`/`cancel`/`cleanup`: lifecycle no-ops for local transfers
//! - `onDynamicProgress`: event subscription (local stage is instant; no progress events emitted)

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nc_code_channels::{
    CancelToken, Channel, ChannelContext, ChannelFailure, EventBus, EventSubscription,
};
use nc_code_rpc::WireValue;
use nc_code_service_core::{ServiceArgs, ServiceError};
use serde_json::{json, Value};
use tokio::fs;

#[derive(Clone)]
struct OperationState {
    _bytes: u64,
}

pub struct PromptAttachmentTransferService {
    operations: Mutex<HashMap<String, OperationState>>,
    buses: Mutex<HashMap<String, EventBus>>,
}

impl PromptAttachmentTransferService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            operations: Mutex::new(HashMap::new()),
            buses: Mutex::new(HashMap::new()),
        })
    }

    fn bus_for(&self, operation_id: &str) -> EventBus {
        let mut buses = self.buses.lock().unwrap_or_else(|p| p.into_inner());
        buses
            .entry(operation_id.to_string())
            .or_insert_with(|| EventBus::new(8))
            .clone()
    }

    fn required_str(params: &Value, key: &str) -> Result<String, ServiceError> {
        params
            .get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ServiceError::invalid_params(format!("{key} is required")))
    }

    async fn stage(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let operation_id = Self::required_str(&params, "operationId")?;
        let _session_id = Self::required_str(&params, "sessionId")?;
        let _workspace_path = Self::required_str(&params, "workspacePath")?;
        let local_path = Self::required_str(&params, "localPath")?;
        let _file_name = Self::required_str(&params, "fileName")?;
        let _mime = Self::required_str(&params, "mime")?;

        let bytes = if let Some(n) = params.get("sizeBytes").and_then(|v| v.as_u64()) {
            if n > 0 {
                n
            } else {
                fs::metadata(&local_path)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0)
            }
        } else {
            fs::metadata(&local_path)
                .await
                .map(|m| m.len())
                .unwrap_or(0)
        };

        // Local transfer: ref is the local path, not staged to a remote host.
        let result = json!({
            "operationId": operation_id,
            "ref": local_path,
            "bytes": bytes,
            "staged": false,
        });

        self.operations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(operation_id.clone(), OperationState { _bytes: bytes });
        // Ensure bus exists for potential listeners (no progress emitted for instant local stage).
        let _ = self.bus_for(&operation_id);

        Ok(WireValue::Json(result))
    }

    fn op_id_arg(&self, args: &ServiceArgs) -> Result<String, ServiceError> {
        // Accept either a string arg or {operationId: ...}
        if let Ok(id) = args.required_string(0, "operationId") {
            return Ok(id);
        }
        if let Ok(params) = args.required_json(0, "params") {
            if let Some(id) = params.get("operationId").and_then(|v| v.as_str()) {
                return Ok(id.to_string());
            }
        }
        Err(ServiceError::invalid_params("operationId is required"))
    }

    fn remove_op(&self, operation_id: &str) {
        self.operations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(operation_id);
        // Keep bus around is harmless; drop on cleanup.
        if operation_id.is_empty() {
            return;
        }
        self.buses
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(operation_id);
    }
}

impl Default for PromptAttachmentTransferService {
    fn default() -> Self {
        Self {
            operations: Mutex::new(HashMap::new()),
            buses: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl Channel for PromptAttachmentTransferService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        match command {
            "stage" => self.stage(&args).await.map_err(ChannelFailure::from),
            "adopt" => {
                // Validate operationId exists in contract; local adopt is a no-op.
                let _id = self.op_id_arg(&args).map_err(ChannelFailure::from)?;
                Ok(WireValue::Undefined)
            }
            "cancel" | "cleanup" => {
                if let Ok(id) = self.op_id_arg(&args) {
                    self.remove_op(&id);
                }
                Ok(WireValue::Undefined)
            }
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }

    async fn listen(
        &self,
        _ctx: &ChannelContext,
        event: &str,
        arg: WireValue,
    ) -> Result<EventSubscription, ChannelFailure> {
        match event {
            "onDynamicProgress" => {
                let operation_id = match &arg {
                    WireValue::String(s) => s.clone(),
                    WireValue::Json(v) => v.as_str().unwrap_or_default().to_string(),
                    _ => return Err(ChannelFailure::event_not_found(event)),
                };
                if operation_id.is_empty() {
                    return Err(ChannelFailure::event_not_found(event));
                }
                Ok(EventSubscription::from_broadcast(
                    self.bus_for(&operation_id).subscribe(),
                ))
            }
            _ => Err(ChannelFailure::event_not_found(event)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;
    use tempfile::TempDir;

    fn ctx() -> ChannelContext {
        ChannelContext::new("prompt-attachment-transfer", ClientMode::DesktopContinuous)
    }

    #[tokio::test]
    async fn stage_returns_local_ref_and_bytes() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("a.txt");
        tokio::fs::write(&file, b"hello").await.unwrap();
        let svc = PromptAttachmentTransferService::new();
        let params = json!({
            "operationId": "op1",
            "sessionId": "s1",
            "workspacePath": tmp.path().to_string_lossy(),
            "localPath": file.to_string_lossy(),
            "fileName": "a.txt",
            "mime": "text/plain"
        });
        let out = svc
            .call(
                &ctx(),
                "stage",
                vec![WireValue::Json(params)],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = out else {
            panic!("json")
        };
        assert_eq!(v["operationId"], "op1");
        assert_eq!(v["ref"].as_str(), Some(file.to_string_lossy().as_ref()));
        assert_eq!(v["bytes"], 5);
        assert_eq!(v["staged"], false);
    }

    #[tokio::test]
    async fn stage_uses_size_bytes_when_provided() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("b.bin");
        tokio::fs::write(&file, b"x").await.unwrap();
        let svc = PromptAttachmentTransferService::new();
        let params = json!({
            "operationId": "op2",
            "sessionId": "s1",
            "workspacePath": tmp.path().to_string_lossy(),
            "localPath": file.to_string_lossy(),
            "fileName": "b.bin",
            "mime": "application/octet-stream",
            "sizeBytes": 999
        });
        let out = svc
            .call(
                &ctx(),
                "stage",
                vec![WireValue::Json(params)],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = out else {
            panic!("json")
        };
        assert_eq!(v["bytes"], 999);
    }

    #[tokio::test]
    async fn adopt_cancel_cleanup_and_unknown_method() {
        let svc = PromptAttachmentTransferService::new();
        let adopt = svc
            .call(
                &ctx(),
                "adopt",
                vec![WireValue::String("op1".into())],
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(adopt, WireValue::Undefined));
        let cancel = svc
            .call(
                &ctx(),
                "cancel",
                vec![WireValue::String("op1".into())],
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(cancel, WireValue::Undefined));
        let cleanup = svc
            .call(
                &ctx(),
                "cleanup",
                vec![WireValue::Json(json!({"operationId":"op1"}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(cleanup, WireValue::Undefined));
        let err = svc
            .call(&ctx(), "prepare", vec![], CancelToken::new())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Method not found"));
    }

    #[tokio::test]
    async fn on_dynamic_progress_returns_subscription() {
        let svc = PromptAttachmentTransferService::new();
        let mut sub = svc
            .listen(&ctx(), "onDynamicProgress", WireValue::String("op9".into()))
            .await
            .unwrap();
        // Local stage emits no progress; subscription should not yield spuriously in short window.
        let result = tokio::time::timeout(std::time::Duration::from_millis(80), sub.next()).await;
        assert!(result.is_err() || matches!(result, Ok(None)));
    }

    #[tokio::test]
    async fn listen_unknown_event_fails() {
        let svc = PromptAttachmentTransferService::new();
        assert!(svc
            .listen(&ctx(), "onNope", WireValue::String("x".into()))
            .await
            .is_err());
    }
}
