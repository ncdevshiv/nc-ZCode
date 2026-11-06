use nc_code_channels::{CancelToken, Channel, ChannelContext, ClientMode};
use nc_code_services_coding_plan_subscription::CodingPlanSubscriptionService;
use serde_json::json;

#[tokio::test]
async fn channel_contract_matches_disabled_ts_behavior() {
    let service = CodingPlanSubscriptionService::new();
    let ctx = ChannelContext::new("coding-plan-subscription", ClientMode::DesktopContinuous);

    let pending = service
        .call(&ctx, "checkPendingOrders", vec![], CancelToken::new())
        .await
        .expect("pending");
    let pending = match pending {
        nc_code_rpc::WireValue::Json(value) => value,
        other => panic!("expected json, got {other:?}"),
    };
    assert_eq!(pending["orders"], json!([]));

    let err = service
        .call(&ctx, "bindStripeCard", vec![], CancelToken::new())
        .await
        .expect_err("removed");
    assert!(err.to_string().contains("CodingPlanPurchaseRemoved"));
}
