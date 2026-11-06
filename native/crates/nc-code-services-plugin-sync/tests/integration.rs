use nc_code_channels::{CancelToken, Channel, ChannelContext, ClientMode};
use nc_code_rpc::WireValue;
use nc_code_service_core::AppPaths;
use nc_code_services_plugin_sync::PluginSyncService;
use serde_json::json;
use tempfile::TempDir;
use tokio::fs;

fn ctx() -> ChannelContext {
    ChannelContext::new("plugin-sync", ClientMode::DesktopContinuous)
}

#[tokio::test]
async fn local_discovery_status_and_honest_archive_behavior() {
    let tmp = TempDir::new().unwrap();
    let paths = AppPaths::new(tmp.path().join("home"), tmp.path().join("data"));
    let plugin_dir = tmp.path().join("my-plugin");
    fs::create_dir_all(plugin_dir.join(".zcode-plugin"))
        .await
        .unwrap();
    fs::write(
        plugin_dir.join(".zcode-plugin/plugin.json"),
        r#"{"name":"my-plugin","description":"desc","version":"0.1.0","commands":{}}"#,
    )
    .await
    .unwrap();
    let cfg_dir = paths.home_dir().join(".zcode").join("cli");
    fs::create_dir_all(&cfg_dir).await.unwrap();
    fs::write(
        cfg_dir.join("config.json"),
        serde_json::to_string(&json!({
            "plugins": {"dirs":[plugin_dir.to_string_lossy()], "enabledPlugins": {"my-plugin@inline": true}}
        }))
        .unwrap(),
    )
    .await
    .unwrap();

    let svc = PluginSyncService::new(paths);
    let list = svc
        .call(
            &ctx(),
            "listLocalUserPluginCandidates",
            vec![],
            CancelToken::new(),
        )
        .await
        .unwrap();
    let WireValue::Json(v) = list else { panic!() };
    assert_eq!(
        v["candidates"].as_array().unwrap()[0]["pluginId"],
        "my-plugin@inline"
    );
    assert!(v["maxArchiveBytes"].as_u64().unwrap() > 0);

    let st = svc
        .call(
            &ctx(),
            "listRemoteUserPluginStatuses",
            vec![WireValue::Json(
                json!({"plugins":[{"pluginId":"my-plugin@inline","directoryName":"my-plugin"}]}),
            )],
            CancelToken::new(),
        )
        .await
        .unwrap();
    let WireValue::Json(v) = st else { panic!() };
    assert_eq!(v["statuses"][0]["exists"], false);

    let wa = svc
        .call(
            &ctx(),
            "checkRemoteUserPluginWriteAccess",
            vec![],
            CancelToken::new(),
        )
        .await
        .unwrap();
    let WireValue::Json(v) = wa else { panic!() };
    assert_eq!(v["writable"], true);

    let err = svc
        .call(
            &ctx(),
            "importPluginsArchive",
            vec![WireValue::Json(json!({"archive": []}))],
            CancelToken::new(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not supported"));
}
