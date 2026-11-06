use nc_code_channels::{CancelToken, Channel, ChannelContext, ClientMode};
use nc_code_rpc::WireValue;
use nc_code_services_feedback::FeedbackService;
use serde_json::json;
use tempfile::TempDir;

fn ctx() -> ChannelContext {
    ChannelContext::new("feedback", ClientMode::DesktopContinuous)
}

#[tokio::test]
async fn channel_list_and_get_use_local_store_when_configured() {
    let tmp = TempDir::new().unwrap();
    // Seed store file directly via the same format the store uses
    let store_path = tmp.path().join("tickets.json");
    let seed = json!({
        "tickets": [{
            "deviceMid": "",
            "id": "t1",
            "status": "open",
            "type": "bug",
            "updated_at": "2026-01-01T00:00:00Z",
            "title": "x",
            "created_at": "2026-01-01T00:00:00Z"
        }]
    });
    std::fs::write(&store_path, seed.to_string()).unwrap();

    let svc = FeedbackService::new_with_local_store(tmp.path().to_path_buf());
    let list = svc
        .call(&ctx(), "list", vec![], CancelToken::new())
        .await
        .unwrap();
    if let WireValue::Json(v) = list {
        assert_eq!(v["total"], 1);
        assert_eq!(v["items"][0]["id"], "t1");
    } else {
        panic!("json");
    }

    let get = svc
        .call(
            &ctx(),
            "get",
            vec![WireValue::String("t1".into())],
            CancelToken::new(),
        )
        .await
        .unwrap();
    if let WireValue::Json(v) = get {
        assert_eq!(v["id"], "t1");
    } else {
        panic!("json");
    }
}
