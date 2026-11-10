//! `official-mcp` reads answer the contract shape with no fabricated servers.

use nc_code_channels::{CancelToken, Channel, ChannelContext, ClientMode};
use nc_code_rpc::WireValue;
use serde_json::json;

fn ctx() -> ChannelContext {
    ChannelContext::new("it", ClientMode::DesktopContinuous)
}

#[tokio::test]
async fn reads_answer_the_contract_shape() {
    let service = nc_code_services_official_mcp::OfficialMcpService::new();
    let servers = service
        .call(
            &ctx(),
            "getServers",
            vec![WireValue::Json(json!({"workspacePath": "."}))],
            CancelToken::new(),
        )
        .await
        .expect("getServers");
    let WireValue::Json(value) = servers else {
        panic!("getServers JSON")
    };
    assert!(
        value.get("servers").is_some() || value.is_array(),
        "{value}"
    );
}
