//! `commands` channel

use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::{ServiceArgs, ServiceError};
use serde_json::json;

pub struct CommandsService;

impl CommandsService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl Default for CommandsService {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl Channel for CommandsService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let _args = ServiceArgs::new(args);
        match command {
            "list" => Ok(WireValue::Json(json!([]))),
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;

    fn ctx() -> ChannelContext {
        ChannelContext::new("commands", ClientMode::DesktopContinuous)
    }

    #[tokio::test]
    async fn list() {
        let service = CommandsService::new();
        let result = service
            .call(&ctx(), "list", vec![], CancelToken::new())
            .await
            .unwrap();
        if let WireValue::Json(v) = result {
            assert!(v.is_array());
        } else {
            panic!("bad");
        }
    }
}
