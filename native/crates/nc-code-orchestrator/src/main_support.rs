//! Host-facing helpers shared by the standalone binary and the ZCode server/shell:
//! build the runtime from configuration and describe what is/was not configured.

use std::sync::Arc;

use serde_json::{json, Value};

use nc_code_service_core::AppPaths;
use nc_code_services_provider::store::ProviderStore;

use crate::config::NcagConfig;
use crate::error::Result;
use crate::{NcagRuntime, Store};

/// Compose the runtime from a config and the app paths (provider store is wired here,
/// the same instance the host services use, so there is exactly one provider truth).
pub fn runtime_from_config(config: NcagConfig, paths: &AppPaths) -> Result<NcagRuntime> {
    let provider_store = ProviderStore::new(paths.clone());
    NcagRuntime::open(config, Some(provider_store))
}

/// Open a runtime for tests/tools with an explicit store and optional provider store.
pub fn runtime_with_store(
    config: NcagConfig,
    store: Arc<Store>,
    provider_store: Option<Arc<ProviderStore>>,
) -> NcagRuntime {
    let hub = Arc::new(crate::EventHub::new());
    let orchestrator = Arc::new(crate::Orchestrator::new(
        Arc::clone(&store),
        Arc::clone(&hub),
        config.clone(),
        provider_store,
    ));
    let supervisor = Arc::new(crate::Supervisor::new(
        Arc::clone(&orchestrator),
        Arc::clone(&store),
        Arc::clone(&hub),
    ));
    NcagRuntime {
        store,
        hub,
        orchestrator,
        supervisor,
        config,
    }
}

/// Human-readable startup facts. Deliberately names what is missing.
pub fn describe(config: &NcagConfig) -> Value {
    let provider_hint = if config.model.provider_id.trim().is_empty()
        && config.model.model_id.trim().is_empty()
    {
        "no explicit provider/model selection: the first executable entry in the personal provider store is used"
    } else {
        "an explicit provider/model selection is configured"
    };
    json!({
        "workspaceRoot": config.workspace_root.to_string_lossy(),
        "dataDir": config.data_dir.to_string_lossy(),
        "database": config.database_path.to_string_lossy(),
        "model": {
            "providerId": config.model.provider_id,
            "modelId": config.model.model_id,
            "contextWindow": config.model.context_window,
            "hint": provider_hint,
            "credentialSource": "per personal provider store (Settings → Providers); no NC-AG-side secret",
        },
        "budgets": config.budgets,
        "policy": config.policy,
        "apiTokenRequired": config.api_token.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_names_the_provider_source_and_paths() {
        let config = NcagConfig::load(None, |_| None).unwrap();
        let value = describe(&config);
        assert!(value["model"]["credentialSource"]
            .as_str()
            .unwrap()
            .contains("provider store"));
        assert!(value["database"].as_str().unwrap().ends_with("ncag.sqlite"));
    }
}
