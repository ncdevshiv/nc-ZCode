//! Official MCP channel — explicit, fail-closed implementation.
//!
//! The TS side currently provides credential/issuance/audit helpers but no
//! registered RPC channel contract in `ServiceChannels` for this module. This Rust
//! crate exposes an honest status surface: reads report unavailable/empty, and
//! mutating operations fail explicitly (no fake success).

/// Handoff skeleton (pure data/types): serde persistence structs, port trait, Send/Sync markers.
pub mod handoff;

use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::ServiceArgs;
use serde_json::json;

const NOT_AVAILABLE: &str = "official_mcp_not_available";

pub struct OfficialMcpService;

impl OfficialMcpService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl Default for OfficialMcpService {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl Channel for OfficialMcpService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let _args = ServiceArgs::new(args);
        match command {
            "getStatus" => Ok(WireValue::Json(json!({
                "available": false,
                "reason": NOT_AVAILABLE
            }))),
            "list" | "getServers" => Ok(WireValue::Json(json!({ "servers": [] }))),
            "install" | "enable" | "disable" | "remove" | "update" => {
                Err(ChannelFailure::error(NOT_AVAILABLE))
            }
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;

    fn ctx() -> ChannelContext {
        ChannelContext::new("official-mcp", ClientMode::DesktopContinuous)
    }

    async fn call(command: &str) -> Result<WireValue, ChannelFailure> {
        OfficialMcpService::new()
            .call(&ctx(), command, vec![], CancelToken::new())
            .await
    }

    fn json(value: WireValue) -> serde_json::Value {
        match value {
            WireValue::Json(value) => value,
            other => panic!("expected json, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reads_are_explicitly_unavailable() {
        let status = json(call("getStatus").await.expect("status"));
        assert_eq!(status["available"], json!(false));
        assert_eq!(status["reason"], json!(NOT_AVAILABLE));
        assert_eq!(
            json(call("list").await.expect("list"))["servers"],
            json!([])
        );
    }

    #[tokio::test]
    async fn mutations_fail_closed() {
        for command in ["install", "enable", "disable", "remove"] {
            let err = call(command).await.expect_err(command);
            assert!(err.to_string().contains(NOT_AVAILABLE), "{command}: {err}");
        }
    }
}
