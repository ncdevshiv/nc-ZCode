//! `cua-permission` channel �?" Computer Use Agent permission status/control.
//!
//! The Rust backend runs headless (no macOS TCC broker/desktop helper). Methods are
//! implemented with honest, explicit unavailability rather than fake success.

use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::ServiceArgs;
use serde_json::json;

pub struct CuaPermissionService;

impl CuaPermissionService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }

    fn unavailable_status() -> serde_json::Value {
        json!({
            "available": false,
            "reason": "cua permission broker is unavailable in this runtime",
            "idle": true,
            "grantOwnerDisplayName": null
        })
    }

    fn restart_unavailable() -> serde_json::Value {
        json!({
            "ok": false,
            "reason": "cua permission broker is unavailable in this runtime"
        })
    }
}

impl Default for CuaPermissionService {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl Channel for CuaPermissionService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let _args = ServiceArgs::new(args);
        match command {
            // TS: getStatus(workspacePath, workspaceIdentity?, options?)
            "getStatus" => Ok(WireValue::Json(Self::unavailable_status())),
            // TS: restartHelper(workspacePath?, workspaceIdentity?, options?)
            "restartHelper" => Ok(WireValue::Json(Self::restart_unavailable())),
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;

    fn ctx() -> ChannelContext {
        ChannelContext::new("cua-permission", ClientMode::DesktopContinuous)
    }

    #[tokio::test]
    async fn get_status_reports_unavailable_with_contract_fields() {
        let service = CuaPermissionService::new();
        let result = service
            .call(
                &ctx(),
                "getStatus",
                vec![
                    WireValue::String("/ws".into()),
                    WireValue::Undefined,
                    WireValue::Undefined,
                ],
                CancelToken::new(),
            )
            .await
            .unwrap();
        match result {
            WireValue::Json(v) => {
                assert_eq!(v["available"], false);
                assert!(v["reason"].is_string());
            }
            other => panic!("expected json, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn restart_helper_reports_unavailable() {
        let service = CuaPermissionService::new();
        let result = service
            .call(&ctx(), "restartHelper", vec![], CancelToken::new())
            .await
            .unwrap();
        match result {
            WireValue::Json(v) => {
                assert_eq!(v["ok"], false);
                assert!(v["reason"].is_string());
            }
            other => panic!("expected json, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_method_is_error() {
        let service = CuaPermissionService::new();
        assert!(service
            .call(&ctx(), "nope", vec![], CancelToken::new())
            .await
            .is_err());
    }
}
