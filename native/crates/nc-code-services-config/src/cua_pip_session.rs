//! `cua-pip-session` channel �?" CUA picture-in-picture session events.
//!
//! The headless Rust backend does not present a desktop PiP window. Publish/dispose calls
//! are accepted as explicit no-ops (acknowledged) so callers get deterministic behavior;
//! state reflects that no PiP session is active.

use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::ServiceArgs;
use serde_json::json;

pub struct CuaPipSessionService;

impl CuaPipSessionService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl Default for CuaPipSessionService {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl Channel for CuaPipSessionService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let _args = ServiceArgs::new(args);
        match command {
            // TS: getState() - not in the minimal TS interface shown but useful; keep for compat.
            "getState" => Ok(WireValue::Json(
                json!({ "active": false, "available": false }),
            )),
            // TS: publishFocus(event) / publishLifecycle(event) - accept and acknowledge.
            "publishFocus" | "publishLifecycle" => Ok(WireValue::Undefined),
            // TS: dispose()
            "dispose" => Ok(WireValue::Undefined),
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;

    fn ctx() -> ChannelContext {
        ChannelContext::new("cua-pip-session", ClientMode::DesktopContinuous)
    }

    #[tokio::test]
    async fn get_state_reports_inactive() {
        let service = CuaPipSessionService::new();
        let result = service
            .call(&ctx(), "getState", vec![], CancelToken::new())
            .await
            .unwrap();
        match result {
            WireValue::Json(v) => {
                assert_eq!(v["active"], false);
            }
            other => panic!("expected json, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn publish_and_dispose_are_acknowledged() {
        let service = CuaPipSessionService::new();
        for cmd in ["publishFocus", "publishLifecycle", "dispose"] {
            let result = service
                .call(
                    &ctx(),
                    cmd,
                    vec![WireValue::Json(json!({"kind":"test"}))],
                    CancelToken::new(),
                )
                .await
                .unwrap();
            assert!(
                matches!(result, WireValue::Undefined | WireValue::Json(_)),
                "{cmd} returned unexpected {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn unknown_method_is_error() {
        let service = CuaPipSessionService::new();
        assert!(service
            .call(&ctx(), "nope", vec![], CancelToken::new())
            .await
            .is_err());
    }
}
