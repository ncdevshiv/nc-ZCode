use nc_code_channels::{CancelToken, Channel, ChannelContext, ClientMode};
use nc_code_rpc::WireValue;
use nc_code_services_usage_stats::UsageStatsService;
use serde_json::json;

fn ctx() -> ChannelContext {
    ChannelContext::new("usage-stats", ClientMode::DesktopContinuous)
}

#[tokio::test]
async fn contract_alignment_local_defaults_and_named_failures() {
    let s = UsageStatsService::new();
    let r = s
        .call(
            &ctx(),
            "getAppUsageSnapshot",
            vec![WireValue::Json(json!({"range":"7d"}))],
            CancelToken::new(),
        )
        .await
        .expect("getAppUsageSnapshot");
    let WireValue::Json(v) = r else {
        panic!("json")
    };
    // Contract-shaped AppUsageSnapshot (honest zero defaults, no fabricated usage).
    assert_eq!(v["range"], "7d");
    assert_eq!(v["source"], "agent-db");
    assert!(v["generatedAt"].is_number());
    assert_eq!(v["summary"]["totalTokens"], 0);
    assert!(v["heatmap"]["weeks"].is_array());
    assert!(v["dailyModelUsage"].is_array());
    assert!(v["models"].is_array());
    assert!(v["tools"].is_array());

    let err = s
        .call(
            &ctx(),
            "getCodingPlanUsageSnapshot",
            vec![WireValue::Json(json!({}))],
            CancelToken::new(),
        )
        .await
        .expect_err("coding plan should fail");
    assert!(err.to_string().contains("no_bigmodel_api_key"));

    let ent = s
        .call(&ctx(), "getEntitlementSnapshot", vec![], CancelToken::new())
        .await
        .expect("entitlement");
    let WireValue::Json(e) = ent else {
        panic!("json")
    };
    assert_eq!(e["authenticated"], false);
}
