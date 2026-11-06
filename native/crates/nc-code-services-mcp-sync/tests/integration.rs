use nc_code_channels::{CancelToken, Channel, ChannelContext, ClientMode};
use nc_code_rpc::WireValue;
use nc_code_service_core::AppPaths;
use nc_code_services_mcp_sync::McpSyncService;
use serde_json::json;
use tempfile::TempDir;

fn ctx() -> ChannelContext {
    ChannelContext::new("mcp-sync", ClientMode::DesktopContinuous)
}

#[tokio::test]
async fn full_local_lifecycle_upsert_enable_load_remove() {
    let tmp = TempDir::new().unwrap();
    let svc = McpSyncService::new(AppPaths::new(
        tmp.path().join("home"),
        tmp.path().join("data"),
    ));

    // upsert
    svc.call(
        &ctx(),
        "saveMcpToUserDirectory",
        vec![WireValue::Json(json!({
            "action":"upsert","name":"alpha","config":{"command":"node","args":["a.js"],"type":"stdio"}
        }))],
        CancelToken::new(),
    )
    .await
    .unwrap();

    // disable
    svc.call(
        &ctx(),
        "saveMcpToUserDirectory",
        vec![WireValue::Json(
            json!({"action":"set-enabled","name":"alpha","enabled":false}),
        )],
        CancelToken::new(),
    )
    .await
    .unwrap();

    // load
    let loaded = svc
        .call(
            &ctx(),
            "loadMcpFromUserDirectory",
            vec![WireValue::Json(json!({}))],
            CancelToken::new(),
        )
        .await
        .unwrap();
    let WireValue::Json(v) = loaded else {
        panic!("json")
    };
    let servers = v["servers"].as_array().unwrap();
    let alpha = servers
        .iter()
        .find(|s| s["name"] == "alpha")
        .expect("alpha");
    assert_eq!(alpha["enabled"], false);

    // candidates
    let cands = svc
        .call(
            &ctx(),
            "listLocalUserMcpCandidates",
            vec![],
            CancelToken::new(),
        )
        .await
        .unwrap();
    let WireValue::Json(v) = cands else { panic!() };
    assert_eq!(v["candidates"].as_array().unwrap().len(), 1);

    // write access
    let wa = svc
        .call(
            &ctx(),
            "checkRemoteUserMcpWriteAccess",
            vec![],
            CancelToken::new(),
        )
        .await
        .unwrap();
    let WireValue::Json(v) = wa else { panic!() };
    assert_eq!(v["writable"], true);

    // remove
    svc.call(
        &ctx(),
        "saveMcpToUserDirectory",
        vec![WireValue::Json(json!({"action":"remove","name":"alpha"}))],
        CancelToken::new(),
    )
    .await
    .unwrap();
    let loaded = svc
        .call(
            &ctx(),
            "loadMcpFromUserDirectory",
            vec![WireValue::Json(json!({}))],
            CancelToken::new(),
        )
        .await
        .unwrap();
    let WireValue::Json(v) = loaded else { panic!() };
    assert!(v["servers"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn workspace_status_listing_is_explicitly_unavailable() {
    let tmp = TempDir::new().unwrap();
    let svc = McpSyncService::new(AppPaths::new(
        tmp.path().join("home"),
        tmp.path().join("data"),
    ));
    let err = svc
        .call(
            &ctx(),
            "listWorkspaceMcpServerStatuses",
            vec![WireValue::Json(json!({"workspacePath":"/w"}))],
            CancelToken::new(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unavailable"));
}
