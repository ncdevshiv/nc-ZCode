use nc_code_channels::{CancelToken, Channel, ChannelContext, ClientMode};
use nc_code_rpc::WireValue;
use nc_code_services_file_watcher::FileWatcherService;
use serde_json::json;
use tempfile::TempDir;

fn ctx() -> ChannelContext {
    ChannelContext::new("file-watcher-it", ClientMode::DesktopContinuous)
}

#[tokio::test]
async fn watches_real_directory_and_receives_change_event() {
    let tmp = TempDir::new().unwrap();
    let svc = FileWatcherService::new();
    let res = svc
        .call(
            &ctx(),
            "watch",
            vec![WireValue::Json(
                json!({"path": tmp.path().to_string_lossy(), "recursive": false}),
            )],
            CancelToken::new(),
        )
        .await
        .expect("watch");
    let id = match res {
        WireValue::Json(v) => v["id"].as_str().unwrap().to_string(),
        _ => panic!("bad"),
    };

    let mut sub = svc
        .listen(&ctx(), "onDynamicChange", WireValue::String(id.clone()))
        .await
        .expect("listen");

    // Trigger a filesystem change.
    let file_path = tmp.path().join("test-file.txt");
    tokio::fs::write(&file_path, b"hello").await.unwrap();

    // Debounce is 150ms; allow generous timeout for CI/slow FS.
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
        .await
        .expect("timeout waiting for event")
        .expect("subscription ended");

    match event {
        WireValue::Json(v) => {
            assert!(v.get("dirPath").is_some(), "missing dirPath: {v}");
        }
        other => panic!("unexpected event: {other:?}"),
    }

    svc.call(&ctx(), "disposeAll", vec![], CancelToken::new())
        .await
        .unwrap();
}
