//! `media-preview` channel

use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::{ServiceArgs, ServiceError};

pub struct MediaPreviewService;

impl MediaPreviewService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }

    async fn prepare(&self, _args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        Err(ServiceError::new("not implemented"))
    }
}

impl Default for MediaPreviewService {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl Channel for MediaPreviewService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        match command {
            "prepare" => self.prepare(&args).await.map_err(ChannelFailure::from),
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;

    fn ctx() -> ChannelContext {
        ChannelContext::new("media-preview", ClientMode::DesktopContinuous)
    }

    #[tokio::test]
    async fn prepare_exists() {
        let service = MediaPreviewService::new();
        let _ = service
            .call(&ctx(), "prepare", vec![], CancelToken::new())
            .await;
    }
}
