//! The packaged composition root builds a registry with the live channel families.

#[test]
fn app_crate_builds_the_live_registry() {
    let root = std::env::temp_dir().join(format!("zcode-app-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("data dir");
    let backend = nc_code_app::build(nc_code_service_core::AppPaths::new(
        root.clone(),
        root.clone(),
    ))
    .expect("build");
    assert!(backend.registry.get("zcode-agent").is_some());
    assert!(backend.registry.get("zcode-session").is_some());
    assert!(backend.registry.get("media-preview").is_some());
    assert!(backend.registry.get("settings-sync").is_some());
    let _ = std::fs::remove_dir_all(&root);
}
