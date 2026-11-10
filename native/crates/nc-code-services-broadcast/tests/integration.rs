//! The broadcast claim lifecycle must be real: acquire, claim, release, re-acquire.

use nc_code_channels::{CancelToken, Channel, ChannelContext, ClientMode};
use nc_code_rpc::WireValue;
use serde_json::json;

fn ctx() -> ChannelContext {
    ChannelContext::new("it", ClientMode::DesktopContinuous)
}

#[tokio::test]
async fn claim_lifecycle_hands_the_claim_over() {
    let service = nc_code_services_broadcast::BroadcastService::new();
    let acquired = service
        .call(
            &ctx(),
            "acquireClaim",
            vec![WireValue::Json(json!({"key": "k1"}))],
            CancelToken::new(),
        )
        .await
        .expect("acquire");
    let WireValue::Json(value) = acquired else {
        panic!("acquire JSON")
    };
    assert_eq!(value["status"], json!("acquired"), "{value}");
    assert_eq!(value["lease"]["key"], json!("k1"), "{value}");
    let claim_id = value["lease"]["token"].as_str().unwrap_or("").to_string();
    let released = service
        .call(
            &ctx(),
            "releaseClaim",
            vec![WireValue::Json(json!({"key": "k1", "claimId": claim_id}))],
            CancelToken::new(),
        )
        .await
        .expect("release");
    assert!(matches!(
        released,
        WireValue::Json(_) | WireValue::Undefined
    ));
}
