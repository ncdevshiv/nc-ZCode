//! `client-config` channel — public client config snapshot (`IClientConfigService`).
//!
//! TS fetches `{origin}/api/v1/client/configs?app_version=..&platform=..` and projects a
//! single remote field, `pluginStoreOrder` (plugin store display ordering). That is a
//! zcode cloud configuration fetch, which this build does not perform: the product keeps
//! no zcode cloud connection other than auth/token claim and account/model setup
//! (`specs/disabled-features.md`). The channel answers the documented local default —
//! `{ pluginStoreOrder: null }` — so consumers keep their built-in ordering instead of
//! issuing a request that is intentionally never sent.

use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure, EventSubscription};
use nc_code_rpc::WireValue;
use nc_code_service_core::{ServiceArgs, ServiceError};
use serde_json::{json, Value};

pub struct ClientConfigService;

impl ClientConfigService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }

    pub async fn call_inner(
        &self,
        command: &str,
        args: Vec<WireValue>,
    ) -> Result<WireValue, ServiceError> {
        match command {
            "getSnapshot" => {
                let args = ServiceArgs::new(args);
                // `getSnapshot(options?)` — forceRefresh is accepted (and ignored: there is
                // no remote snapshot to refresh) but the params object must still be an object.
                let options = args.json(0)?.unwrap_or_else(|| json!({}));
                if !options.is_object() {
                    return Err(ServiceError::invalid_params(
                        "getSnapshot options must be an object",
                    ));
                }
                Ok(WireValue::Json(local_snapshot()))
            }
            other => Err(ServiceError::message(format!("Method not found: {other}"))),
        }
    }
}

/// The snapshot consumers see when no remote config is fetched.
fn local_snapshot() -> Value {
    json!({ "pluginStoreOrder": Value::Null })
}

#[async_trait]
impl Channel for ClientConfigService {
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
    async fn snapshot_is_local_and_shape_matches_the_remote_envelope() {
        let service = ClientConfigService::new();
        let WireValue::Json(value) = service
            .call_inner("getSnapshot", vec![WireValue::Json(json!({}))])
            .await
            .expect("getSnapshot")
        else {
            panic!("getSnapshot must return JSON");
        };
        assert_eq!(value, json!({ "pluginStoreOrder": null }));
    }

    #[tokio::test]
    async fn force_refresh_and_missing_options_still_answer() {
        let service = ClientConfigService::new();
        let WireValue::Json(value) = service
            .call_inner(
                "getSnapshot",
                vec![WireValue::Json(json!({ "forceRefresh": true }))],
            )
            .await
            .expect("getSnapshot")
        else {
            panic!("getSnapshot must return JSON");
        };
        assert_eq!(value["pluginStoreOrder"], Value::Null);
        service
            .call_inner("getSnapshot", vec![])
            .await
            .expect("no args is accepted");
    }

    #[tokio::test]
    async fn non_object_options_are_rejected() {
        let service = ClientConfigService::new();
        let error = service
            .call_inner("getSnapshot", vec![WireValue::Json(json!(5))])
            .await
            .expect_err("options must be an object");
        assert_eq!(error.name, "ZodError");
    }

    #[tokio::test]
    async fn unknown_method_and_missing_channel_events() {
        let service = ClientConfigService::new();
        let error = service
            .call_inner("nope", vec![])
            .await
            .expect_err("unknown method");
        assert!(error.message.contains("Method not found"));
        let ctx = ChannelContext::new("c", nc_code_channels::ClientMode::DesktopContinuous);
        assert!(service
            .listen(&ctx, "onMessage", WireValue::Undefined)
            .await
            .is_err());
    }
}
