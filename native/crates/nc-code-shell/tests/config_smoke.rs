//! 配置冒烟：tauri.conf.json 身份 / 版本与仓库 identity 一致（真实漂移守护）。

/// 产品身份必须与 electron-builder 的生产 identity 同族
/// （`packages/desktop/scripts/desktop-product-identity.mjs`：appId `dev.zcode.app`、
/// productName `ZCode`），否则 Windows 快捷方式/AUMID 会被视为不同应用。
#[test]
fn tauri_config_matches_electron_identity() {
    let cfg: serde_json::Value =
        serde_json::from_str(include_str!("../tauri.conf.json")).expect("parse tauri.conf.json");
    assert_eq!(cfg["identifier"], "dev.zcode.app");
    assert_eq!(cfg["productName"], "NC-Code");
    assert_eq!(
        cfg["bundle"]["active"], true,
        "打包已启用（§5 bundle active）"
    );
    // frontendDist 必须指向本 crate 的脚手架 dist（真实 UI 接入是开放项）。
    assert_eq!(cfg["build"]["frontendDist"], "./dist");
}

/// 版本三方一致：tauri.conf.json == crate（env!）== 仓库根 package.json。
#[test]
fn version_matches_crate_and_root_package_json() {
    let cfg: serde_json::Value =
        serde_json::from_str(include_str!("../tauri.conf.json")).expect("parse tauri.conf.json");
    let pkg: serde_json::Value =
        serde_json::from_str(include_str!("../../../../package.json")).expect("parse package.json");
    assert_eq!(cfg["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(cfg["version"], pkg["version"]);
}

/// dist 脚手架页必须存在（tauri codegen 依赖 frontendDist），且如实标注真实 UI
/// 资源来源为开放集成项（spec S4 验收 + §7 R4 门）——守护的是诚实标注契约本身。
#[test]
fn dist_scaffold_documents_open_ui_integration() {
    let html = include_str!("../dist/index.html");
    assert!(
        html.contains("packages/web"),
        "脚手架页必须说明 UI 资源来源为开放项"
    );
}
