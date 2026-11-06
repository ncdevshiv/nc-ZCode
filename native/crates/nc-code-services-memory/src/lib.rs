//! `memory` channel

use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::{ServiceArgs, ServiceError};
use serde_json::json;

pub struct MemoryService;

impl MemoryService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl Default for MemoryService {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl Channel for MemoryService {
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
        ChannelContext::new("memory", ClientMode::DesktopContinuous)
    }

    #[tokio::test]
    async fn basic() {
        let service = MemoryService::new();
        let _ = service
            .call(&ctx(), "list", vec![], CancelToken::new())
            .await;
    }
}
