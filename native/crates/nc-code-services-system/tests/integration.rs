use nc_code_channels::{CancelToken, Channel, ChannelContext, ClientMode};
use nc_code_rpc::WireValue;
use nc_code_services_system::SystemService;

fn ctx() -> ChannelContext {
    ChannelContext::new("system", ClientMode::DesktopContinuous)
}

#[tokio::test]
async fn info_roundtrip() {
    let service = SystemService::new();
    let result = service
        .call(&ctx(), "info", vec![], CancelToken::new())
        .await
        .unwrap();
    match result {
        WireValue::Json(v) => {
            assert!(v.get("homedir").is_some());
            assert!(v.get("platform").is_some());
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn list_shells_roundtrip() {
    let service = SystemService::new();
    let result = service
        .call(
            &ctx(),
            "listIntegratedTerminalShells",
            vec![],
            CancelToken::new(),
        )
        .await
        .unwrap();
    match result {
        WireValue::Json(v) => assert!(v.is_array()),
        other => panic!("unexpected: {other:?}"),
    }
}
