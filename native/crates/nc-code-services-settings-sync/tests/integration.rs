//! The first-run prompt flag persists through the real settings-sync store.

use nc_code_channels::{CancelToken, Channel, ChannelContext, ClientMode};
use nc_code_rpc::WireValue;
use serde_json::json;

fn ctx() -> ChannelContext {
    ChannelContext::new("it", ClientMode::DesktopContinuous)
}

#[tokio::test]
async fn first_run_prompt_flag_round_trips() {
    let root = std::env::temp_dir().join(format!("zcode-settings-sync-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let service = nc_code_services_settings_sync::SettingsSyncService::with_home_dir(root.clone());
    let before = service
        .call(
            &ctx(),
            "getFirstRunPromptState",
            vec![WireValue::Json(json!({}))],
            CancelToken::new(),
        )
        .await
        .expect("state");
    let WireValue::Json(before) = before else {
        panic!("JSON")
    };
    assert!(before.get("handled").is_some(), "{before}");
    service
        .call(
            &ctx(),
            "markFirstRunPromptHandled",
            vec![WireValue::Json(json!({}))],
            CancelToken::new(),
        )
        .await
        .expect("mark");
    let after = service
        .call(
            &ctx(),
            "getFirstRunPromptState",
            vec![WireValue::Json(json!({}))],
            CancelToken::new(),
        )
        .await
        .expect("state2");
    let WireValue::Json(after) = after else {
        panic!("JSON")
    };
    assert_eq!(after["handled"], json!(true), "{after}");
    let _ = std::fs::remove_dir_all(&root);
}

/// End-to-end through the channel surface: detect → importSelected → the files exist
/// in the zcode target roots and a second import reports `targetExists`.
#[tokio::test]
async fn detect_then_import_writes_real_files_and_is_idempotent() {
    let root = std::env::temp_dir().join(format!(
        "zcode-settings-sync-import-it-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");

    // Source skill + command in the Claude Code layout.
    let skill_dir = root.join(".claude").join("skills").join("ported-skill");
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: ported-skill\n---\nbody",
    )
    .expect("skill file");
    let command_dir = root.join(".claude").join("commands");
    std::fs::create_dir_all(&command_dir).expect("command dir");
    std::fs::write(command_dir.join("ported.md"), "ported command body").expect("command file");

    let service = nc_code_services_settings_sync::SettingsSyncService::with_home_dir(root.clone());

    let detected = service
        .call(
            &ctx(),
            "detect",
            vec![WireValue::Json(json!({"categories":["skills","commands"]}))],
            CancelToken::new(),
        )
        .await
        .expect("detect");
    let WireValue::Json(detected) = detected else {
        panic!("JSON")
    };
    assert!(
        detected["agents"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "detect must see the fixtures: {detected}"
    );

    let request = json!({
        "selections": [
            {"agent":"claudeCode","category":"skills"},
            {"agent":"claudeCode","category":"commands"},
        ]
    });
    let imported = service
        .call(
            &ctx(),
            "importSelected",
            vec![WireValue::Json(request.clone())],
            CancelToken::new(),
        )
        .await
        .expect("import");
    let WireValue::Json(imported) = imported else {
        panic!("JSON")
    };
    assert_eq!(imported["successCount"], 2, "{imported}");
    assert_eq!(imported["failedCount"], 0, "{imported}");
    assert!(root
        .join(".zcode")
        .join("skills")
        .join("ported-skill")
        .join("SKILL.md")
        .is_file());
    assert_eq!(
        std::fs::read_to_string(root.join(".zcode").join("commands").join("ported.md"))
            .expect("copied command"),
        "ported command body"
    );

    // Idempotent second run: both targets exist, so nothing is imported again.
    let again = service
        .call(
            &ctx(),
            "importSelected",
            vec![WireValue::Json(request)],
            CancelToken::new(),
        )
        .await
        .expect("import again");
    let WireValue::Json(again) = again else {
        panic!("JSON")
    };
    assert_eq!(again["successCount"], 0, "{again}");
    assert_eq!(again["skippedCount"], 2, "{again}");
    assert_eq!(again["failedCount"], 0, "{again}");
    let _ = std::fs::remove_dir_all(&root);
}
