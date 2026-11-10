//! `nc-code-services-file` is the standalone file channel crate. The registered
//! `file` channel in the composition root is `nc-code-services-workspace::file`
//! (single owner per channel); this crate keeps the standalone surface, and its
//! members are exercised here so the two implementations cannot silently diverge
//! on read semantics.

use serde_json::json;

/// `readTextFile` returns the real bytes of a real file (content + size), not a stub.
#[tokio::test]
async fn read_text_file_returns_real_content_and_size() {
    use nc_code_channels::{CancelToken, Channel, ChannelContext, ClientMode};
    use nc_code_rpc::WireValue;

    let root = std::env::temp_dir().join(format!("zcode-file-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("workspace");
    let path = root.join("note.txt");
    std::fs::write(&path, "round-trip payload").expect("seed");

    let service = nc_code_services_file::FileService::new();
    let ctx = ChannelContext::new("file-it", ClientMode::DesktopContinuous);
    let read = service
        .call(
            &ctx,
            "readTextFile",
            vec![WireValue::Json(json!({"path": path.to_string_lossy()}))],
            CancelToken::new(),
        )
        .await
        .expect("readTextFile");
    let WireValue::Json(value) = read else {
        panic!("readTextFile must return JSON");
    };
    assert_eq!(value["content"], json!("round-trip payload"));
    assert_eq!(
        value["totalLength"],
        json!("round-trip payload".chars().count())
    );
    assert_eq!(value["eof"], json!(true));
    let _ = std::fs::remove_dir_all(&root);
}
