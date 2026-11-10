//! `hooks` channel

/// Handoff skeleton (pure data/types): serde persistence structs, port trait, Send/Sync markers.
pub mod handoff;

use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::ServiceArgs;
use serde_json::json;

pub struct HooksService;

impl HooksService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl Default for HooksService {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl Channel for HooksService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let _args = ServiceArgs::new(args);
        match command {
            "list" | "get" | "getStatus" => Ok(WireValue::Json(json!([]))),
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;

    fn ctx() -> ChannelContext {
        ChannelContext::new("hooks", ClientMode::DesktopContinuous)
    }

    #[tokio::test]
    async fn basic() {
        let service = HooksService::new();
        let _ = service
            .call(&ctx(), "list", vec![], CancelToken::new())
            .await;
    }
}
