use nc_code_channels::{CancelToken, Channel, ChannelContext, ClientMode};
use nc_code_rpc::WireValue;
use nc_code_service_core::AppPaths;
use nc_code_services_skill_sync::SkillSyncService;
use serde_json::json;
use tempfile::TempDir;
use tokio::fs;

fn ctx() -> ChannelContext {
    ChannelContext::new("skill-sync", ClientMode::DesktopContinuous)
}

#[tokio::test]
async fn discovers_exports_imports_and_checks_access() {
    let tmp = TempDir::new().unwrap();
    let paths = AppPaths::new(tmp.path().join("home"), tmp.path().join("data"));
    let skill_dir = paths
        .home_dir()
        .join(".zcode")
        .join("skills")
        .join("demo-skill");
    fs::create_dir_all(&skill_dir).await.unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: Demo Skill\ndescription: test\n---\n# demo\n",
    )
    .await
    .unwrap();
    fs::write(skill_dir.join("extra.txt"), "hello")
        .await
        .unwrap();

    let svc = SkillSyncService::new(paths.clone());
    let list = svc
        .call(
            &ctx(),
            "listLocalUserSkillCandidates",
            vec![],
            CancelToken::new(),
        )
        .await
        .unwrap();
    let WireValue::Json(v) = list else { panic!() };
    let candidates = v["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0]["name"], "Demo Skill");
    let id = candidates[0]["id"].as_str().unwrap();

    let exp = svc
        .call(
            &ctx(),
            "exportSkillsArchive",
            vec![WireValue::Json(json!({"skillIds":[id]}))],
            CancelToken::new(),
        )
        .await
        .unwrap();
    let WireValue::Json(expv) = exp else { panic!() };
    assert_eq!(expv["skills"].as_array().unwrap().len(), 1);

    // import into fresh home
    let tmp2 = TempDir::new().unwrap();
    let svc2 = SkillSyncService::new(AppPaths::new(
        tmp2.path().join("home"),
        tmp2.path().join("data"),
    ));
    let imp = svc2
        .call(
            &ctx(),
            "importSkillsArchive",
            vec![WireValue::Json(json!({"archive": expv["archive"]}))],
            CancelToken::new(),
        )
        .await
        .unwrap();
    let WireValue::Json(impv) = imp else { panic!() };
    assert_eq!(impv["results"][0]["status"], "synced");
    assert!(tmp2
        .path()
        .join("home/.zcode/skills/demo-skill/SKILL.md")
        .exists());

    // remote status + write access
    let st = svc2
        .call(
            &ctx(),
            "listRemoteUserSkillStatuses",
            vec![WireValue::Json(
                json!({"directoryNames":["demo-skill","missing"]}),
            )],
            CancelToken::new(),
        )
        .await
        .unwrap();
    let WireValue::Json(stv) = st else { panic!() };
    assert_eq!(stv["statuses"][0]["exists"], true);
    assert_eq!(stv["statuses"][1]["exists"], false);

    let wa = svc2
        .call(
            &ctx(),
            "checkRemoteUserSkillWriteAccess",
            vec![],
            CancelToken::new(),
        )
        .await
        .unwrap();
    let WireValue::Json(wav) = wa else { panic!() };
    assert_eq!(wav["writable"], true);
}
