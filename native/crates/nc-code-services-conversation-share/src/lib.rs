//! `conversation-share` channel — contract-correct, fail-closed implementation.
//!
//! ZCode cloud share services were removed; this channel preserves the RPC contract
//! with honest disabled/unavailable behavior (no fake data, no network calls).

use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::{ServiceArgs, ServiceError};
use serde_json::json;

const UNAVAILABLE: &str = "Conversation sharing is unavailable: ZCode services have been removed";

pub struct ConversationShareService;

impl ConversationShareService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }

    fn capabilities() -> WireValue {
        // Contract-compatible shape (IConversationShareService.getCapabilities).
        // Disabled: no access modes, no artifacts, zero limits where appropriate.
        WireValue::Json(json!({
            "schema_version": 1,
            "access_modes": [],
            "allowed_artifacts": [],
            "ttl_ms": 0,
            "max_rows": 0,
            "max_payload_bytes": 0,
            "max_artifact_count": 0,
            "max_artifact_bytes": 0,
            "max_total_artifact_bytes": 0
        }))
    }

    fn unavailable() -> ChannelFailure {
        ServiceError::named("ConversationShareUnavailable", UNAVAILABLE).into()
    }
}

impl Default for ConversationShareService {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl Channel for ConversationShareService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let _args = ServiceArgs::new(args);
        match command {
            "getCapabilities" => Ok(Self::capabilities()),
            // RPC methods that require ZCode cloud/share backend: fail closed honestly.
            "getContinuation"
            | "getImportedConversation"
            | "getPreview"
            | "importShare"
            | "preflight"
            | "publish"
            | "revoke"
            | "listImportedConversations"
            | "deleteImportedConversation" => Err(Self::unavailable()),
            // Dynamic event subscriptions are not available without the backend.
            "onDynamicImportProgress" | "onDynamicPublishProgress" => Err(Self::unavailable()),
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;

    fn ctx() -> ChannelContext {
        ChannelContext::new("conversation-share", ClientMode::DesktopContinuous)
    }

    #[tokio::test]
    async fn get_capabilities_returns_contract_shape_disabled() {
        let service = ConversationShareService::new();
        let result = service
            .call(&ctx(), "getCapabilities", vec![], CancelToken::new())
            .await
            .unwrap();
        let WireValue::Json(v) = result else {
            panic!("expected json");
        };
        assert_eq!(v["schema_version"], 1);
        assert!(v["access_modes"].is_array());
        assert!(v["access_modes"].as_array().unwrap().is_empty());
        assert!(v["allowed_artifacts"].is_array());
        assert!(v["max_rows"].is_number());
        assert!(v["max_payload_bytes"].is_number());
        assert!(v["max_artifact_count"].is_number());
    }

    #[tokio::test]
    async fn cloud_methods_fail_closed_with_honest_error() {
        let service = ConversationShareService::new();
        for method in [
            "getPreview",
            "preflight",
            "publish",
            "importShare",
            "getContinuation",
            "getImportedConversation",
            "revoke",
        ] {
            let err = service
                .call(
                    &ctx(),
                    method,
                    vec![WireValue::Json(json!({}))],
                    CancelToken::new(),
                )
                .await
                .unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("unavailable") || msg.contains("removed") || msg.contains("ZCode"),
                "{method}: expected honest unavailable error, got {msg}"
            );
        }
    }

    #[tokio::test]
    async fn dynamic_events_fail_closed_honestly() {
        let service = ConversationShareService::new();
        let err = service
            .call(
                &ctx(),
                "onDynamicPublishProgress",
                vec![WireValue::Json(json!({"operationId": "op1"}))],
                CancelToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("unavailable")
                || err.to_string().contains("removed")
        );
    }

    #[tokio::test]
    async fn unknown_method_reports_method_not_found() {
        let service = ConversationShareService::new();
        let err = service
            .call(&ctx(), "nope", vec![], CancelToken::new())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Method not found") || err.to_string().contains("not found")
        );
    }
}
