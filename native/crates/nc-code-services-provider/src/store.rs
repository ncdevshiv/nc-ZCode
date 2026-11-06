//! Personal provider store: providers/models/order/default selection, persisted as
//! `{appConfigDir}/provider-config-rust-v1.json`.
//!
//! This is the Rust backend's provider registry. It supports any provider through the
//! same personal-provider records the settings UI edits; built-in templates only seed
//! new providers and are never required at runtime.

use std::sync::Arc;

use nc_code_channels::EventBus;
use nc_code_rpc::WireValue;
use nc_code_service_core::atomic::{atomic_write_text, read_text_if_exists};
use nc_code_service_core::{AppPaths, ServiceError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

const STORE_VERSION: u32 = 1;
const TEMPLATES_FIXTURE: &str = include_str!("../../../fixtures/boot/provider-settings-view.json");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRecord {
    #[serde(rename = "modelId")]
    pub model_id: String,
    pub config: Value,
    pub enabled: bool,
    #[serde(rename = "useRecommendedConfig", default)]
    pub use_recommended: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderRecord {
    #[serde(rename = "providerId")]
    pub provider_id: String,
    #[serde(rename = "providerName")]
    pub provider_name: String,
    #[serde(rename = "templateId", skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
    pub enabled: bool,
    pub config: Value,
    pub models: Vec<ModelRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreSnapshot {
    pub version: u32,
    pub revision: u64,
    pub providers: Vec<ProviderRecord>,
    pub provider_order: Vec<String>,
    #[serde(
        rename = "defaultModelSelection",
        skip_serializing_if = "Option::is_none"
    )]
    pub default_model_selection: Option<Value>,
}

impl Default for StoreSnapshot {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            revision: 1,
            providers: Vec::new(),
            provider_order: Vec::new(),
            default_model_selection: None,
        }
    }
}

impl StoreSnapshot {
    pub fn provider(&self, provider_id: &str) -> Option<&ProviderRecord> {
        self.providers
            .iter()
            .find(|provider| provider.provider_id == provider_id)
    }

    pub fn provider_mut(&mut self, provider_id: &str) -> Option<&mut ProviderRecord> {
        self.providers
            .iter_mut()
            .find(|provider| provider.provider_id == provider_id)
    }

    pub fn template(&self, template_id: &str) -> Option<Value> {
        templates()
            .iter()
            .find(|template| template["templateId"].as_str() == Some(template_id))
            .cloned()
    }
}

pub struct ProviderStore {
    paths: AppPaths,
    snapshot: Mutex<Option<StoreSnapshot>>,
    pub changes: EventBus,
}

impl ProviderStore {
    pub fn new(paths: AppPaths) -> Arc<Self> {
        Arc::new(Self {
            paths,
            snapshot: Mutex::new(None),
            changes: EventBus::default(),
        })
    }

    fn file(&self) -> std::path::PathBuf {
        self.paths
            .app_config_dir()
            .join("provider-config-rust-v1.json")
    }

    pub async fn snapshot(&self) -> Result<StoreSnapshot, ServiceError> {
        let mut guard = self.snapshot.lock().await;
        if let Some(snapshot) = guard.as_ref() {
            return Ok(snapshot.clone());
        }
        let snapshot = self.load_from_disk().await?;
        *guard = Some(snapshot.clone());
        Ok(snapshot)
    }

    async fn load_from_disk(&self) -> Result<StoreSnapshot, ServiceError> {
        let Some(text) = read_text_if_exists(&self.file()).await? else {
            return Ok(StoreSnapshot::default());
        };
        let snapshot: StoreSnapshot = serde_json::from_str(&text)
            .map_err(|error| ServiceError::new(format!("provider config is corrupt: {error}")))?;
        if snapshot.version != STORE_VERSION {
            return Err(ServiceError::new(format!(
                "unsupported provider config version: {}",
                snapshot.version
            )));
        }
        Ok(snapshot)
    }

    pub async fn reload(&self) -> Result<StoreSnapshot, ServiceError> {
        let mut guard = self.snapshot.lock().await;
        let snapshot = self.load_from_disk().await?;
        *guard = Some(snapshot.clone());
        Ok(snapshot)
    }

    pub async fn mutate<F>(&self, mutate: F) -> Result<StoreSnapshot, ServiceError>
    where
        F: FnOnce(&mut StoreSnapshot) -> Result<(), ServiceError>,
    {
        let mut guard = self.snapshot.lock().await;
        if guard.is_none() {
            *guard = Some(self.load_from_disk().await?);
        }
        let snapshot = guard.as_mut().expect("loaded");
        let previous_revision = snapshot.revision;
        mutate(snapshot)?;
        snapshot.revision = previous_revision.saturating_add(1);
        let contents = format!(
            "{}\n",
            serde_json::to_string_pretty(&*snapshot)
                .map_err(|error| ServiceError::new(error.to_string()))?
        );
        atomic_write_text(&self.file(), &contents).await?;
        let snapshot = snapshot.clone();
        self.changes
            .send(WireValue::Json(json!({ "revision": snapshot.revision })));
        Ok(snapshot)
    }

    /// Identical to [`Self::mutate`], but each time it is forced to reread from the disk before changing (the CAS token comes from the latest disk
    /// state, in-memory cache is not trusted) - use this for compare-and-swap provisioning transactions/rollbacks.
    pub async fn mutate_from_disk<F>(&self, mutate: F) -> Result<StoreSnapshot, ServiceError>
    where
        F: FnOnce(&mut StoreSnapshot) -> Result<(), ServiceError>,
    {
        let mut guard = self.snapshot.lock().await;
        *guard = Some(self.load_from_disk().await?);
        let snapshot = guard.as_mut().expect("loaded");
        let previous_revision = snapshot.revision;
        mutate(snapshot)?;
        snapshot.revision = previous_revision.saturating_add(1);
        let contents = format!(
            "{}\n",
            serde_json::to_string_pretty(&*snapshot)
                .map_err(|error| ServiceError::new(error.to_string()))?
        );
        atomic_write_text(&self.file(), &contents).await?;
        let snapshot = snapshot.clone();
        self.changes
            .send(WireValue::Json(json!({ "revision": snapshot.revision })));
        Ok(snapshot)
    }
}

pub fn templates() -> Vec<Value> {
    let parsed: Value =
        serde_json::from_str(TEMPLATES_FIXTURE).expect("provider templates fixture");
    parsed
        .get("providerTemplates")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

pub fn template_name(template: &Value, locale: Option<&str>) -> Option<String> {
    let map = template.get("templateNameMap")?.as_object()?;
    let preferred = match locale {
        Some(locale) if locale.starts_with("zh") => "zh-CN",
        _ => "en-US",
    };
    map.get(preferred)
        .and_then(Value::as_str)
        .or_else(|| map.values().find_map(Value::as_str))
        .map(str::to_string)
}

pub fn deep_merge(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(key) {
                    Some(existing) => deep_merge(existing, value),
                    None => {
                        base.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (base, overlay) => *base = overlay.clone(),
    }
}

pub fn model_ids_from_config(config: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    for key in ["builtinModelIds", "personalModelIds", "modelOrder"] {
        if let Some(items) = config.get(key).and_then(Value::as_array) {
            for item in items.iter().filter_map(Value::as_str) {
                if !ids.iter().any(|existing| existing == item) {
                    ids.push(item.to_string());
                }
            }
        }
    }
    ids
}

pub fn default_model_config() -> Value {
    json!({
        "enabled": true,
        "properties": {
            "requiresMfjsToolSchema": false,
            "contextWindow": 128000,
            "inputFormat": {
                "supportsText": true,
                "supportsImage": true,
                "supportsVideo": false,
                "supportsAudio": false,
                "supportsPdf": true
            },
            "outputFormat": {"supportsText": true},
            "supportsToolCall": true,
            "supportsJsonSchemaOutput": false,
            "supportsNativeWebSearch": false,
            "supportsMidConversationSystem": false
        },
        "optionSpecs": {
            "reasoningLevel": {"values": ["low", "medium", "high"], "map": "{}"},
            "maxOutputTokens": {"max": 8192, "map": "{}"}
        }
    })
}

pub fn provider_effective_config(provider: &ProviderRecord, snapshot: &StoreSnapshot) -> Value {
    let mut effective = provider
        .template_id
        .as_deref()
        .and_then(|template_id| snapshot.template(template_id))
        .and_then(|template| template.get("config").cloned())
        .unwrap_or_else(|| json!({}));
    deep_merge(&mut effective, &provider.config);
    if effective.get("group").is_none() {
        effective["group"] = Value::String("standard-personal".to_string());
    }
    effective
}

pub fn provider_issues(config: &Value) -> Vec<Value> {
    let mut issues = Vec::new();
    let api_base = config
        .get("api")
        .and_then(|api| api.get("baseUrl"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if api_base.is_empty() {
        issues.push(json!({
            "code": "required-field-missing",
            "path": ["api", "baseUrl"],
            "message": "Base URL is required",
        }));
    }
    let access_type = config
        .get("access")
        .and_then(|access| access.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if access_type != "zhipu-account" {
        let api_key = config
            .get("access")
            .and_then(|access| access.get("apiKey"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if api_key.is_empty() {
            issues.push(json!({
                "code": "required-field-missing",
                "path": ["access", "apiKey"],
                "message": "API key is required",
            }));
        }
    }
    issues
}

pub fn model_effective_config(model: &ModelRecord) -> Value {
    let mut effective = default_model_config();
    deep_merge(&mut effective, &model.config);
    effective["enabled"] = Value::Bool(model.enabled);
    effective
}

pub fn provider_order(snapshot: &StoreSnapshot) -> Vec<String> {
    let mut order = Vec::new();
    for id in &snapshot.provider_order {
        if snapshot.provider(id).is_some() && !order.contains(id) {
            order.push(id.clone());
        }
    }
    for provider in &snapshot.providers {
        if !order.contains(&provider.provider_id) {
            order.push(provider.provider_id.clone());
        }
    }
    order
}

pub fn new_provider_id(snapshot: &StoreSnapshot) -> String {
    let mut index = snapshot.revision + 1;
    loop {
        let id = format!("personal:{index}");
        if snapshot.provider(&id).is_none() {
            return id;
        }
        index += 1;
    }
}

pub fn normalize_model_list(models: &mut Vec<ModelRecord>, model_ids: &[String]) {
    for model_id in model_ids {
        if !models.iter().any(|model| model.model_id == *model_id) {
            models.push(ModelRecord {
                model_id: model_id.clone(),
                config: default_model_config(),
                enabled: true,
                use_recommended: true,
            });
        }
    }
}

pub fn provider_view_record(provider: &ProviderRecord, snapshot: &StoreSnapshot) -> Value {
    let effective = provider_effective_config(provider, snapshot);
    let issues = provider_issues(&effective);
    let executable = issues.is_empty() && provider.enabled;
    let template_config = provider
        .template_id
        .as_deref()
        .and_then(|template_id| snapshot.template(template_id))
        .and_then(|template| template.get("config").cloned());
    let models: Vec<Value> = provider
        .models
        .iter()
        .map(|model| {
            let model_config = model_effective_config(model);
            json!({
                "kind": "candidate",
                "modelId": model.model_id,
                "builtin": false,
                "effectiveBuiltinConfig": {},
                "personalExactConfig": model.config,
                "useRecommendedConfig": model.use_recommended,
                "effectiveConfig": model_config,
                "enabled": model.enabled,
                "executable": executable,
                "selectable": executable && model.enabled,
                "issues": [],
            })
        })
        .collect();
    let mut view = json!({
        "providerId": provider.provider_id,
        "providerName": provider.provider_name,
        "enabled": provider.enabled,
        "executable": executable,
        "personalConfig": provider.config,
        "effectiveConfig": effective,
        "issues": issues,
        "models": models,
    });
    if let Some(template_id) = &provider.template_id {
        view["templateId"] = Value::String(template_id.clone());
    }
    if let Some(template_config) = template_config {
        view["templateConfig"] = template_config;
    }
    view
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deep_merge_keeps_nested_provider_config() {
        let mut base = json!({"api": {"type": "openai-chat-completions", "baseUrl": "https://a"}, "access": {"type": "api-key"}});
        deep_merge(
            &mut base,
            &json!({"api": {"baseUrl": "https://b"}, "access": {"apiKey": "k"}}),
        );
        assert_eq!(base["api"]["type"], json!("openai-chat-completions"));
        assert_eq!(base["api"]["baseUrl"], json!("https://b"));
        assert_eq!(base["access"]["apiKey"], json!("k"));
    }

    #[test]
    fn model_ids_dedupe_in_declared_order() {
        let config = json!({
            "builtinModelIds": ["a", "b"],
            "personalModelIds": ["b", "c"],
            "modelOrder": ["c", "a"]
        });
        assert_eq!(model_ids_from_config(&config), vec!["a", "b", "c"]);
    }

    #[test]
    fn issues_require_endpoint_and_key() {
        let issues = provider_issues(&json!({}));
        assert_eq!(issues.len(), 2);
        let issues = provider_issues(&json!({
            "api": {"baseUrl": "https://api.example.com/v1"},
            "access": {"type": "api-key", "apiKey": "k"}
        }));
        assert!(issues.is_empty());
    }

    #[test]
    fn model_ids_dedupe_preserves_declared_order_multiple_sources() {
        let config = json!({
            "builtinModelIds": ["x", "y"],
            "personalModelIds": ["y", "z"],
            "modelOrder": ["z", "x", "y"]
        });
        assert_eq!(model_ids_from_config(&config), vec!["x", "y", "z"]);
    }
}
