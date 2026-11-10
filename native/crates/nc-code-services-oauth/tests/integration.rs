//! OAuth is disabled by product decision: session reads are signed-out and start/refresh
//! fail with the named `OAuth disabled` error (never a fabricated token).

use nc_code_channels::{CancelToken, Channel, ChannelContext, ClientMode};
use nc_code_rpc::WireValue;
use serde_json::json;

fn ctx() -> ChannelContext {
    ChannelContext::new("it", ClientMode::DesktopContinuous)
}

#[tokio::test]
async fn disabled_surface_never_fabricates_a_session() {
    let service = nc_code_services_oauth::OAuthService::new();
    let providers = service
        .call(
            &ctx(),
            "getProviders",
            vec![WireValue::Json(json!({}))],
            CancelToken::new(),
        )
        .await
        .expect("getProviders");
    let WireValue::Json(value) = providers else {
        panic!("getProviders JSON")
    };
    assert!(
        value.as_array().is_some_and(|list| list.is_empty()) || value.get("providers").is_some(),
        "{value}"
    );
    let refresh = service
        .call(
            &ctx(),
            "refreshToken",
            vec![WireValue::Json(json!({"providerId": "zai"}))],
            CancelToken::new(),
        )
        .await
        .expect_err("refresh must fail closed");
    assert!(refresh.to_string().contains("OAuth disabled"), "{refresh}");
}
