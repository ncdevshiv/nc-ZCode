//! Feedback is a **removed upload surface** (claim boundary): `create` answers the honest
//! "not available in this backend" union and `list` reads the real local ticket store.
//! There is nothing to upload, so this test pins the removed-surface contract plus the
//! store's read path — never a fabricated ticket.

use serde_json::json;

#[tokio::test]
async fn removed_surface_is_honest_and_list_reads_the_local_store() {
    use nc_code_channels::{CancelToken, Channel, ChannelContext, ClientMode};
    use nc_code_rpc::WireValue;

    let root = std::env::temp_dir().join(format!("zcode-feedback-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let service = nc_code_services_feedback::FeedbackService::new_with_local_store(root.clone());
    let ctx = ChannelContext::new("feedback-it", ClientMode::DesktopContinuous);

    let created = service
        .call(
            &ctx,
            "create",
            vec![WireValue::Json(json!({"kind": "bug", "message": "x"}))],
            CancelToken::new(),
        )
        .await
        .expect("create");
    let WireValue::Json(created) = created else {
        panic!("create JSON");
    };
    assert_eq!(created["status"], json!("unsupported"), "{created}");
    assert!(
        created["message"]
            .as_str()
            .unwrap_or("")
            .contains("not available"),
        "{created}"
    );

    let listed = service
        .call(
            &ctx,
            "list",
            vec![WireValue::Json(json!({}))],
            CancelToken::new(),
        )
        .await
        .expect("list");
    let WireValue::Json(listed) = listed else {
        panic!("list JSON");
    };
    assert!(listed["items"].is_array(), "{listed}");
    assert_eq!(
        listed["total"],
        json!(0),
        "no ticket may be fabricated: {listed}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
