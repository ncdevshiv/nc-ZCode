//! `client-config` + `client-scenes` through the real channel registry.
//!
//! Both channels answer from local defaults: this build performs no zcode cloud
//! configuration fetch (`specs/disabled-features.md`). The test drives the same registry
//! the server uses, so a name/arg mismatch would fail here exactly as it would at boot.

use std::sync::Arc;

use nc_code_channels::{CancelToken, ChannelContext, ChannelRegistry, ClientMode};
use nc_code_rpc::WireValue;
use nc_code_services_client::ClientConfigService;
use nc_code_services_client::ClientScenesService;
use serde_json::{json, Value};

fn ctx() -> ChannelContext {
    ChannelContext::new("client-local-test", ClientMode::DesktopContinuous)
}

async fn call_json(registry: &ChannelRegistry, channel: &str, method: &str) -> Value {
    let service = registry.get(channel).unwrap_or_else(|| panic!("{channel}"));
    let result = service
        .call(&ctx(), method, vec![], CancelToken::new())
        .await
        .unwrap_or_else(|error| panic!("{channel}.{method}: {error}"));
    let WireValue::Json(value) = result else {
        panic!("{channel}.{method} must return JSON");
    };
    value
}

#[tokio::test]
async fn registry_answers_local_defaults_without_any_network_origin() {
    let registry = ChannelRegistry::new();
    registry
        .register("client-config", ClientConfigService::new())
        .expect("register client-config");
    registry
        .register("client-scenes", ClientScenesService::new())
        .expect("register client-scenes");

    let snapshot = call_json(&registry, "client-config", "getSnapshot").await;
    assert_eq!(snapshot, json!({ "pluginStoreOrder": null }));

    let scenes = call_json(&registry, "client-scenes", "list").await;
    assert_eq!(scenes["code"], json!(0));
    assert_eq!(scenes["data"], json!([]));
}

#[tokio::test]
async fn unknown_members_still_fail_closed() {
    let registry = Arc::new(ChannelRegistry::new());
    registry
        .register("client-config", ClientConfigService::new())
        .expect("register client-config");
    let service = registry.get("client-config").expect("client-config");
    let error = service
        .call(&ctx(), "nope", vec![], CancelToken::new())
        .await
        .expect_err("unknown member");
    assert!(error.to_string().contains("Method not found"), "{error}");
}
