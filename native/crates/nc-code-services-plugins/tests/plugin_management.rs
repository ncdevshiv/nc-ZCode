//! End-to-end tests for the `plugin-management` channel.
//!
//! Every fixture is a real temp directory tree: config layers under `<home>/.zcode`,
//! plugin packages with real manifests and `SKILL.md` files, and the real `skills`
//! channel for skill discovery. Nothing is faked at the engine boundary.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure, ClientMode};
use nc_code_rpc::WireValue;
use nc_code_service_core::AppPaths;
use nc_code_services_extensions::SkillsService;
use nc_code_services_plugins::PluginManagementService;
use serde_json::{json, Value};

const WORKSPACE_KEY: &str = "plugin-management-test-workspace";

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("zcode-pm-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&workspace).expect("workspace");
        Self {
            root,
            home,
            workspace,
        }
    }

    fn paths(&self) -> AppPaths {
        AppPaths::new(self.home.clone(), self.home.clone())
    }

    fn user_config_path(&self) -> PathBuf {
        self.home.join(".zcode").join("cli").join("config.json")
    }

    fn storage_root(&self) -> PathBuf {
        self.home.join(".zcode").join("cli").join("plugins")
    }

    fn write_json(path: &Path, value: &Value) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("config parent");
        }
        std::fs::write(path, serde_json::to_string_pretty(value).expect("json"))
            .expect("write json");
    }

    fn write_user_config(&self, value: Value) {
        Self::write_json(&self.user_config_path(), &value);
    }

    fn read_user_config(&self) -> Value {
        let text = std::fs::read_to_string(self.user_config_path()).expect("read config");
        serde_json::from_str(&text).expect("parse config")
    }

    /// One real plugin package: manifest, a skill and an agent component.
    fn plugin_dir(&self, name: &str) -> PathBuf {
        let root = self.home.join("plugin-sources").join(name);
        Self::write_json(
            &root.join(".nc-code-plugin").join("plugin.json"),
            &json!({
                "name": name,
                "version": "1.0.0",
                "description": "Fixture plugin",
            }),
        );
        let skill_dir = root.join("skills").join("greeter");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: greeter\ndescription: Greets people\n---\n\nUse the greeter skill.\n",
        )
        .expect("skill file");
        let agent_dir = root.join("agents");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        std::fs::write(
            agent_dir.join("reviewer.md"),
            "---\nname: reviewer\ndescription: Reviews code\n---\n\nReview carefully.\n",
        )
        .expect("agent file");
        root
    }

    fn configure_plugin_dirs(&self, dirs: &[&Path]) {
        let dirs: Vec<Value> = dirs
            .iter()
            .map(|dir| Value::String(dir.to_string_lossy().into_owned()))
            .collect();
        self.write_user_config(json!({ "plugins": { "dirs": dirs } }));
    }

    /// Official cache root discovery reads `cache/zcode-plugins-official/<name>/<version>`.
    fn add_official_cache_plugin(&self, name: &str) -> PathBuf {
        let root = self
            .storage_root()
            .join("cache")
            .join("zcode-plugins-official")
            .join(name)
            .join("1.0.0");
        Self::write_json(
            &root.join(".nc-code-plugin").join("plugin.json"),
            &json!({
                "name": name,
                "version": "1.0.0",
                "description": "Official fixture plugin",
            }),
        );
        root
    }

    fn service(&self) -> Arc<PluginManagementService> {
        // Real skill discovery owner; the channel delegates `getSkillReferenceCatalog`.
        let skills: Arc<dyn Channel> = Arc::new(SkillsService::new(self.paths()));
        PluginManagementService::new(self.paths(), skills)
    }

    fn base_params(&self) -> Value {
        json!({
            "workspace": {
                "workspacePath": self.workspace.to_string_lossy(),
                "workspaceKey": WORKSPACE_KEY,
            }
        })
    }

    fn params(&self, extra: Value) -> Value {
        let mut base = self.base_params();
        if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                base.insert(key.clone(), value.clone());
            }
        }
        base
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn context() -> ChannelContext {
    ChannelContext::new("plugin-management-test", ClientMode::DesktopContinuous)
}

async fn call(
    service: &PluginManagementService,
    method: &str,
    params: Value,
) -> Result<Value, ChannelFailure> {
    let result = service
        .call(
            &context(),
            method,
            vec![WireValue::Json(params)],
            CancelToken::new(),
        )
        .await?;
    match result {
        WireValue::Json(value) => Ok(value),
        other => panic!("expected a JSON result, got {other:?}"),
    }
}

fn find<'a>(items: &'a [Value], key: &str, expected: &str) -> &'a Value {
    items
        .iter()
        .find(|item| item.get(key).and_then(Value::as_str) == Some(expected))
        .unwrap_or_else(|| panic!("no row with {key}={expected}: {items:?}"))
}

#[tokio::test]
async fn list_plugins_sees_configured_plugin_dir() {
    let fixture = Fixture::new();
    let plugin_dir = fixture.plugin_dir("demo");
    fixture.configure_plugin_dirs(&[&plugin_dir]);
    let service = fixture.service();

    let result = call(&service, "listPlugins", fixture.base_params())
        .await
        .expect("listPlugins");
    let plugins = result["plugins"].as_array().expect("plugins array");
    assert_eq!(plugins.len(), 1, "{plugins:?}");
    let plugin = &plugins[0];
    assert_eq!(plugin["id"], json!("demo@inline"));
    assert_eq!(plugin["name"], json!("demo"));
    assert_eq!(plugin["source"], json!("inline"));
    assert_eq!(plugin["enabled"], json!(true));
    assert_eq!(plugin["skillCount"], json!(1));

    let components = plugin["components"].as_array().expect("components");
    let skills = components
        .iter()
        .find(|group| group["kind"] == json!("skill"))
        .expect("skill group");
    assert_eq!(skills["items"][0]["name"], json!("greeter"));
    let agents = components
        .iter()
        .find(|group| group["kind"] == json!("agent"))
        .expect("agent group");
    assert_eq!(agents["items"][0]["name"], json!("reviewer"));
    assert!(result["diagnostics"].is_array());
}

#[tokio::test]
async fn list_plugins_includes_configured_but_missing_rows() {
    let fixture = Fixture::new();
    fixture.write_user_config(json!({
        "plugins": { "enabledPlugins": { "ghost@local-market": true } }
    }));
    let service = fixture.service();

    let result = call(&service, "listPlugins", fixture.base_params())
        .await
        .expect("listPlugins");
    let plugins = result["plugins"].as_array().expect("plugins array");
    let missing = find(plugins, "id", "ghost@local-market");
    assert_eq!(missing["name"], json!("ghost"));
    assert_eq!(missing["marketplace"], json!("local-market"));
    assert_eq!(missing["source"], json!("missing"));
    assert_eq!(missing["packageStatus"], json!("missing"));
    assert_eq!(missing["enabled"], json!(true));
    assert_eq!(missing["components"], json!([]));
}

#[tokio::test]
async fn set_plugin_enabled_writes_user_config() {
    let fixture = Fixture::new();
    let plugin_dir = fixture.plugin_dir("demo");
    fixture.configure_plugin_dirs(&[&plugin_dir]);
    let service = fixture.service();

    let enabled = call(
        &service,
        "setPluginEnabled",
        fixture.params(json!({
            "pluginId": "demo@inline",
            "enabled": true,
            "scope": "user",
        })),
    )
    .await
    .expect("enable");
    assert_eq!(enabled["enabled"], json!(true));
    assert_eq!(enabled["plugin"]["id"], json!("demo@inline"));
    assert_eq!(enabled["plugin"]["enabled"], json!(true));
    assert_eq!(enabled["plugin"]["enabledSource"], json!("user"));

    let config = fixture.read_user_config();
    assert_eq!(
        config["plugins"]["enabledPlugins"]["demo@inline"],
        json!(true)
    );
    // The unrelated `dirs` declaration survives the read-modify-write.
    assert_eq!(
        config["plugins"]["dirs"][0],
        json!(plugin_dir.to_string_lossy())
    );

    let disabled = call(
        &service,
        "setPluginEnabled",
        fixture.params(json!({
            "pluginId": "demo@inline",
            "enabled": false,
            "scope": "user",
        })),
    )
    .await
    .expect("disable");
    assert_eq!(disabled["enabled"], json!(false));
    assert_eq!(disabled["plugin"]["enabled"], json!(false));
    let config = fixture.read_user_config();
    assert_eq!(
        config["plugins"]["enabledPlugins"]["demo@inline"],
        json!(false)
    );
}

#[tokio::test]
async fn configure_plugin_persists_options() {
    let fixture = Fixture::new();
    let plugin_dir = fixture.plugin_dir("demo");
    fixture.configure_plugin_dirs(&[&plugin_dir]);
    let service = fixture.service();

    let configured = call(
        &service,
        "configurePlugin",
        fixture.params(json!({
            "pluginId": "demo@inline",
            "options": {
                "apiKey": "value",
                "retries": 3,
                "verbose": false,
                "nested": { "ignored": true },
            },
            "clearOptionKeys": ["legacy"],
        })),
    )
    .await
    .expect("configure");
    assert_eq!(configured["pluginId"], json!("demo@inline"));
    assert_eq!(configured["diagnostics"], json!([]));

    let config = fixture.read_user_config();
    assert_eq!(
        config["plugins"]["options"]["demo@inline"],
        json!({ "apiKey": "value", "retries": 3, "verbose": false })
    );

    // dryRun validates without persisting.
    call(
        &service,
        "configurePlugin",
        fixture.params(json!({
            "pluginId": "demo@inline",
            "options": { "tempKey": "temp" },
            "dryRun": true,
        })),
    )
    .await
    .expect("dry run");
    let config = fixture.read_user_config();
    assert!(
        config["plugins"]["options"]["demo@inline"]["tempKey"].is_null(),
        "{config}"
    );

    call(
        &service,
        "resetPluginConfig",
        fixture.params(json!({ "pluginId": "demo@inline" })),
    )
    .await
    .expect("reset");
    let config = fixture.read_user_config();
    assert!(
        config["plugins"]["options"]["demo@inline"].is_null(),
        "{config}"
    );
}

#[tokio::test]
async fn validate_plugin_reports_real_diagnostic() {
    let fixture = Fixture::new();
    let empty_marketplace = fixture.root.join("empty-marketplace");
    std::fs::create_dir_all(&empty_marketplace).expect("empty marketplace");
    let service = fixture.service();

    let result = call(
        &service,
        "validatePlugin",
        fixture.params(json!({ "source": empty_marketplace.to_string_lossy() })),
    )
    .await
    .expect("validate source");
    assert_eq!(result["ok"], json!(false));
    let diagnostics = result["diagnostics"].as_array().expect("diagnostics");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0]["code"], json!("plugin_marketplace_invalid"));
    assert!(
        diagnostics[0]["message"]
            .as_str()
            .expect("message")
            .contains("Marketplace manifest not found in directory"),
        "{diagnostics:?}"
    );
    assert_eq!(
        result["compatibility"],
        json!({
            "runnable": ["skills", "commands", "hooks", "mcpServers", "userConfig"],
            "diagnosticOnly": ["agents", "lspServers", "outputStyles", "channels", "settings"],
            "unsupported": ["mcpb", "dxt", "npm", "hostPattern", "pathPattern"],
        })
    );

    let unknown = call(
        &service,
        "validatePlugin",
        fixture.params(json!({ "marketplace": "nope", "pluginName": "demo" })),
    )
    .await
    .expect("validate marketplace");
    assert_eq!(unknown["ok"], json!(false));
    let diagnostics = unknown["diagnostics"].as_array().expect("diagnostics");
    assert_eq!(diagnostics[0]["code"], json!("plugin_marketplace_invalid"));
    assert!(
        diagnostics[0]["message"]
            .as_str()
            .expect("message")
            .contains("Marketplace not found: nope"),
        "{diagnostics:?}"
    );
}

#[tokio::test]
async fn overview_includes_configured_but_missing_plugin_rows() {
    let fixture = Fixture::new();
    let missing_install_path = fixture.root.join("never-materialized");
    Fixture::write_json(
        &fixture.storage_root().join("installed_plugins.json"),
        &json!({
            "version": 1,
            "plugins": [{
                "id": "ghost@local-market",
                "name": "ghost",
                "marketplace": "local-market",
                "version": "2.0.0",
                "installPath": missing_install_path.to_string_lossy(),
                "installedAt": "2026-01-01T00:00:00.000Z",
                "scope": "user",
            }],
        }),
    );
    let declared_catalog = fixture.root.join("declared-catalog");
    std::fs::create_dir_all(&declared_catalog).expect("declared catalog");
    fixture.write_user_config(json!({
        "plugins": {
            "enabledPlugins": { "ghost@local-market": true },
            "extraKnownMarketplaces": {
                "team-catalog": {
                    "source": {
                        "source": "directory",
                        "path": declared_catalog.to_string_lossy(),
                    }
                }
            }
        }
    }));
    let service = fixture.service();

    let result = call(&service, "getPluginsOverview", fixture.base_params())
        .await
        .expect("overview");
    assert_eq!(result["capability"]["supported"], json!(true));
    assert!(result["diagnostics"].is_array());
    assert_eq!(result["restorableBuiltins"], json!([]));

    let installed = result["installedPlugins"].as_array().expect("installed");
    let ghost = find(installed, "id", "ghost@local-market");
    assert_eq!(ghost["name"], json!("ghost"));
    assert_eq!(ghost["version"], json!("2.0.0"));
    assert_eq!(ghost["enabled"], json!(true));
    assert_eq!(ghost["scope"], json!("user"));
    assert_eq!(
        ghost["installPath"],
        json!(missing_install_path.to_string_lossy())
    );

    // Declared-but-not-materialized marketplaces are configured rows with zero plugins.
    let marketplaces = result["marketplaces"].as_array().expect("marketplaces");
    let team = find(marketplaces, "id", "team-catalog");
    assert_eq!(team["pluginCount"], json!(0));
    assert_eq!(team["name"], json!("team-catalog"));
}

#[tokio::test]
async fn reference_catalog_returns_discovered_components() {
    let fixture = Fixture::new();
    let plugin_dir = fixture.plugin_dir("demo");
    fixture.configure_plugin_dirs(&[&plugin_dir]);
    let service = fixture.service();

    let result = call(&service, "getPluginReferenceCatalog", fixture.base_params())
        .await
        .expect("reference catalog");
    assert_eq!(result["authority"], json!("workspace"));
    let plugins = result["plugins"].as_array().expect("plugins");
    assert_eq!(plugins.len(), 1, "{plugins:?}");
    let entry = &plugins[0];
    assert_eq!(entry["pluginId"], json!("demo@inline"));
    assert_eq!(entry["name"], json!("demo"));
    assert_eq!(entry["marketplace"], json!("inline"));
    assert_eq!(entry["enabled"], json!(true));
    assert_eq!(entry["conflictingPluginIds"], json!([]));
    assert_eq!(entry["skillQualifiedNames"], json!(["demo:greeter"]));
    assert_eq!(entry["subagentNames"], json!(["demo:reviewer"]));
    assert_eq!(entry["mcpServerNames"], json!([]));
    // No store listing exists for an inline plugin: category falls back to "other".
    assert_eq!(entry["category"], json!("other"));
}

#[tokio::test]
async fn skill_reference_catalog_projects_the_skills_channel() {
    let fixture = Fixture::new();
    let plugin_dir = fixture.plugin_dir("demo");
    fixture.configure_plugin_dirs(&[&plugin_dir]);
    let skill_dir = fixture
        .workspace
        .join(".zcode")
        .join("skills")
        .join("local-skill");
    std::fs::create_dir_all(&skill_dir).expect("workspace skill");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: local-skill\ndescription: Workspace skill\n---\n\nBody.\n",
    )
    .expect("skill file");
    let service = fixture.service();

    let result = call(&service, "getSkillReferenceCatalog", fixture.base_params())
        .await
        .expect("skill catalog");
    assert_eq!(result["authority"], json!("workspace"));
    let skills = result["skills"].as_array().expect("skills");

    let local = skills
        .iter()
        .find(|skill| skill["name"] == json!("local-skill"))
        .expect("workspace skill row");
    assert_eq!(local["scope"], json!("workspace"));
    assert_eq!(local["enabled"], json!(true));
    assert!(
        local["id"]
            .as_str()
            .expect("id")
            .starts_with("glm:workspace:"),
        "{local}"
    );

    let plugin_skill = skills
        .iter()
        .find(|skill| skill["name"] == json!("greeter"))
        .expect("plugin skill row");
    assert_eq!(plugin_skill["scope"], json!("plugin"));
    assert_eq!(plugin_skill["pluginName"], json!("demo"));
    assert!(
        plugin_skill["id"]
            .as_str()
            .expect("id")
            .starts_with("glm:plugin:"),
        "{plugin_skill}"
    );
}

#[tokio::test]
async fn cancel_plugin_operation_reports_unknown_ids() {
    let fixture = Fixture::new();
    let service = fixture.service();

    let result = call(
        &service,
        "cancelPluginOperation",
        json!({ "operationId": "not-running" }),
    )
    .await
    .expect("cancel");
    assert_eq!(
        result,
        json!({ "operationId": "not-running", "cancelled": false })
    );

    let error = call(&service, "cancelPluginOperation", json!({}))
        .await
        .expect_err("missing operationId");
    let payload = error.payload().expect("error payload");
    assert_eq!(payload.name, "ZodError");
}

#[tokio::test]
async fn resolve_suggested_reference_rejects_untrusted_ids() {
    let fixture = Fixture::new();
    let service = fixture.service();

    let result = call(
        &service,
        "resolveSuggestedPluginReference",
        fixture.params(json!({
            "stableId": "demo@not-official",
            "operationId": "op-untrusted",
            "clientMode": "desktop-continuous",
            "deliveryKind": "desktop-continuous",
        })),
    )
    .await
    .expect("resolve");
    assert_eq!(result["stableId"], json!("demo@not-official"));
    assert_eq!(result["status"], json!("unavailable"));
    let diagnostics = result["diagnostics"].as_array().expect("diagnostics");
    assert_eq!(
        diagnostics[0]["code"],
        json!("plugin_suggested_reference_untrusted_source")
    );
    assert_eq!(diagnostics[0]["severity"], json!("error"));
    assert_eq!(diagnostics[0]["pluginId"], json!("demo@not-official"));
}

#[tokio::test]
async fn resolve_suggested_reference_uses_discovered_official_plugin() {
    let fixture = Fixture::new();
    fixture.add_official_cache_plugin("demo");
    fixture.write_user_config(json!({
        "plugins": { "enabledPlugins": { "demo@zcode-plugins-official": true } }
    }));
    let service = fixture.service();

    let result = call(
        &service,
        "resolveSuggestedPluginReference",
        fixture.params(json!({
            "stableId": "demo@zcode-plugins-official",
            "operationId": "op-ready",
            "clientMode": "desktop-continuous",
            "deliveryKind": "desktop-continuous",
        })),
    )
    .await
    .expect("resolve");
    assert_eq!(result["stableId"], json!("demo@zcode-plugins-official"));
    assert_eq!(result["status"], json!("ready"));
    assert_eq!(result["marketplace"], json!("zcode-plugins-official"));
    assert_eq!(result["pluginName"], json!("demo"));
    assert_eq!(result["sourceTrust"], json!("official"));
    assert_eq!(result["diagnostics"], json!([]));
}

#[tokio::test]
async fn resolve_suggested_reference_reports_same_name_conflicts() {
    let fixture = Fixture::new();
    let plugin_dir = fixture.plugin_dir("demo");
    fixture.add_official_cache_plugin("demo");
    fixture.write_user_config(json!({
        "plugins": {
            "dirs": [plugin_dir.to_string_lossy()],
            "enabledPlugins": { "demo@zcode-plugins-official": true },
        }
    }));
    let service = fixture.service();

    let result = call(
        &service,
        "resolveSuggestedPluginReference",
        fixture.params(json!({
            "stableId": "demo@zcode-plugins-official",
            "operationId": "op-conflict",
            "clientMode": "desktop-continuous",
            "deliveryKind": "desktop-continuous",
        })),
    )
    .await
    .expect("resolve");
    assert_eq!(result["status"], json!("conflict"));
    let diagnostics = result["diagnostics"].as_array().expect("diagnostics");
    assert_eq!(
        diagnostics[0]["code"],
        json!("plugin_suggested_reference_conflict")
    );
}

#[tokio::test]
async fn resolve_suggested_reference_refreshes_and_reports_missing_with_progress() {
    let fixture = Fixture::new();
    // A known official marketplace whose source is an inline `settings` manifest: the
    // refresh below performs the real staging/merge path without any network.
    Fixture::write_json(
        &fixture.storage_root().join("known_marketplaces.json"),
        &json!({
            "version": 1,
            "marketplaces": [{
                "id": "zcode-plugins-official",
                "name": "zcode-plugins-official",
                "source": {
                    "source": "settings",
                    "marketplace": {
                        "name": "zcode-plugins-official",
                        "plugins": [{
                            "name": "remote-demo",
                            "version": "1.2.3",
                            "description": "Catalog-only plugin",
                            "displayName": "Remote Demo",
                            "icon": "https://example.com/remote-demo.png",
                        }],
                    },
                },
                "addedAt": "2026-01-01T00:00:00.000Z",
            }],
        }),
    );
    let service = fixture.service();
    let mut subscription = service
        .listen(
            &context(),
            "onDynamicPluginOperationProgress",
            WireValue::String("op-refresh".into()),
        )
        .await
        .expect("listen");

    let request = {
        let service = Arc::clone(&service);
        let params = fixture.params(json!({
            "stableId": "remote-demo@zcode-plugins-official",
            "operationId": "op-refresh",
            "clientMode": "desktop-continuous",
            "deliveryKind": "desktop-continuous",
        }));
        tokio::spawn(async move { call(&service, "resolveSuggestedPluginReference", params).await })
    };
    let event = tokio::time::timeout(Duration::from_secs(10), subscription.next())
        .await
        .expect("progress event before timeout")
        .expect("progress payload");
    let WireValue::Json(event) = event else {
        panic!("progress event must be JSON");
    };
    assert_eq!(event["operationId"], json!("op-refresh"));
    assert_eq!(event["state"], json!("refreshing"));

    let result = request.await.expect("join").expect("resolve after refresh");
    assert_eq!(
        result["stableId"],
        json!("remote-demo@zcode-plugins-official")
    );
    assert_eq!(result["status"], json!("missing"));
    assert_eq!(result["marketplace"], json!("zcode-plugins-official"));
    assert_eq!(result["pluginName"], json!("remote-demo"));
    assert_eq!(result["sourceTrust"], json!("official"));
    assert_eq!(result["icon"], json!("https://example.com/remote-demo.png"));
    assert_eq!(result["listing"]["displayName"], json!("Remote Demo"));
    assert_eq!(result["diagnostics"], json!([]));

    // The refresh really materialized the official catalog on disk.
    assert!(fixture
        .storage_root()
        .join("marketplaces")
        .join("zcode-plugins-official")
        .join("marketplace.json")
        .is_file());
}
