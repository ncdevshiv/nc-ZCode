//! Generic provider services (any provider): personal provider/model CRUD + model
//! selection. Z.ai claim data is handled by `nc-code-services-zai` and never flows here.

pub mod provisioning;
// Completion record (ENTRY 084) for provisioning.rs (provider-provisioning-target channel, 1 member apply):
// There is a complete `impl Channel … "apply"`, but the reference does not exist/unavailable API, complete the list (ENTRY 081):
// 1) Cargo.toml adds nc-code-services-config dependency (provisioning.rs uses CredentialService/SettingService);
// 2) ProviderStore adds mutate_from_disk (called by provisioning.rs:373,435);
// 3) store::publish changes to pub (provisioning.rs:520 call);
// 4) provisioning/file_lock.rs remove unstable windows_by_handle and unlinked windows crate,
// Use stable file locking instead (nc-code-service-core::atomic or fs2);
// 5) Option<Option<String>>::transpose correction of provisioning.rs:1271;
// 6) nc-code-app registers the provider-provisioning-target channel.
pub mod catalog;
pub mod runtime_config;
pub mod selection;
pub mod settings;
pub mod store;

use std::sync::Arc;

use nc_code_channels::{ChannelRegistry, DuplicateChannel, EventBus};
use nc_code_service_core::AppPaths;

pub use runtime_config::RuntimeConfigService;
pub use selection::ModelSelectionService;
pub use settings::ProviderSettingsService;
pub use store::ProviderStore;

pub struct ProviderServices {
    pub store: Arc<ProviderStore>,
    pub settings: Arc<ProviderSettingsService>,
    pub selection: Arc<ModelSelectionService>,
    pub runtime_config: Arc<RuntimeConfigService>,
}

impl ProviderServices {
    pub fn new(paths: AppPaths) -> Self {
        let store = ProviderStore::new(paths);
        let selection_events = EventBus::default();
        let settings = ProviderSettingsService::new(Arc::clone(&store), selection_events.clone());
        let selection = ModelSelectionService::new(Arc::clone(&store), selection_events);
        let runtime_config = RuntimeConfigService::new(Arc::clone(&selection));
        Self {
            settings,
            selection,
            runtime_config,
            store,
        }
    }

    pub fn register(&self, registry: &ChannelRegistry) -> Result<(), DuplicateChannel> {
        registry.register("provider-settings", self.settings.clone())?;
        registry.register("model-selection", self.selection.clone())?;
        registry.register("coding-plan-subscription", self.runtime_config.clone())?;
        Ok(())
    }
}

#[cfg(test)]
mod fixture_tests {
    use super::*;
    use nc_code_service_core::AppPaths;
    use serde_json::{json, Value};

    fn temp_paths(tag: &str) -> (AppPaths, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "zcode-provider-fixture-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        (AppPaths::new(root.join("home"), root.join("data")), root)
    }

    #[tokio::test]
    async fn empty_provider_view_matches_the_typescript_boot_fixture() {
        let (paths, root) = temp_paths("settings");
        let services = ProviderServices::new(paths);
        let view = services.settings.view().await.expect("view");
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../fixtures/boot/provider-settings-view.json"
        ))
        .expect("fixture");
        assert_eq!(view["revision"], fixture["revision"]);
        assert_eq!(view["providerTemplates"], fixture["providerTemplates"]);
        assert_eq!(view["providerOrder"], fixture["providerOrder"]);
        // The TS fixture also projects six built-in coding-plan account providers
        // (`account:...`). They belong to the removed coding-plan engine; the claim
        // boundary keeps only the Z.ai OAuth claim. A fresh Rust store starts empty.
        assert_eq!(view["providers"], json!([]));
        assert!(fixture["providers"]
            .as_array()
            .expect("fixture providers")
            .iter()
            .all(|provider| provider["providerId"]
                .as_str()
                .is_some_and(|id| id.starts_with("account:"))));
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn empty_selection_view_matches_the_typescript_boot_fixture() {
        let (paths, root) = temp_paths("selection");
        let services = ProviderServices::new(paths);
        let view = services.selection.view(None).await.expect("view");
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../fixtures/boot/model-selection-view.json"
        ))
        .expect("fixture");
        assert_eq!(view, fixture);
        std::fs::remove_dir_all(root).ok();
    }
}
