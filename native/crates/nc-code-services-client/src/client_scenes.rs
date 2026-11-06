//! `client-scenes` channel — client scene catalog (`IClientScenesService`).
//!
//! TS performs `GET {origin}/api/v1/client/scenes` and returns the endpoint payload
//! unchanged (`{ code, msg, data: ClientSceneConfig[] }`). The catalog is remote zcode
//! content (starter prompt cards and automation templates), which this build does not
//! fetch — no zcode cloud connection other than auth/token claim and account/model setup
//! (`specs/disabled-features.md`). The channel answers the empty catalog in the same
//! envelope, so consumers render no scenes instead of failing.

use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure, EventSubscription};
use nc_code_rpc::WireValue;
use nc_code_service_core::{ServiceArgs, ServiceError};
use serde_json::json;

pub struct ClientScenesService;

impl ClientScenesService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }

    pub async fn call_inner(
        &self,
        command: &str,
        args: Vec<WireValue>,
    ) -> Result<WireValue, ServiceError> {
        match command {
            "list" => {
                // `list()` takes no parameters; ProxyChannel still sends `[]`.
                let args = ServiceArgs::new(args);
                if let Some(extra) = args.get(0) {
                    if !matches!(extra, WireValue::Undefined) {
                        return Err(ServiceError::invalid_params(
                            "client-scenes.list takes no arguments",
                        ));
                    }
                }
                Ok(WireValue::Json(json!({
                    "code": 0,
                    "msg": "",
                    "data": [],
                })))
            }
            other => Err(ServiceError::message(format!("Method not found: {other}"))),
        }
    }
}

#[async_trait]
impl Channel for ClientScenesService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        self.call_inner(command, args).await.map_err(Into::into)
    }

    async fn listen(
        &self,
        _ctx: &ChannelContext,
        event: &str,
        _arg: WireValue,
    ) -> Result<EventSubscription, ChannelFailure> {
        Err(ChannelFailure::event_not_found(event))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn list_returns_the_empty_catalog_envelope() {
        let service = ClientScenesService::new();
        let WireValue::Json(value) = service.call_inner("list", vec![]).await.expect("list") else {
            panic!("list must return JSON");
        };
        assert_eq!(value, json!({ "code": 0, "msg": "", "data": [] }));
    }

    #[tokio::test]
    async fn rejects_unexpected_argument() {
        let service = ClientScenesService::new();
        let error = service
            .call_inner("list", vec![WireValue::Json(json!({}))])
            .await
            .expect_err("no args allowed");
        assert_eq!(error.name, "ZodError");
    }

    #[tokio::test]
    async fn unknown_method_maps_to_method_not_found() {
        let service = ClientScenesService::new();
        let error = service
            .call_inner("nope", vec![])
            .await
            .expect_err("unknown");
        assert!(error.message.contains("Method not found"));
    }
}
