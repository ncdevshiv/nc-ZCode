//! `provider-settings` channel — personal provider/model CRUD and resolution for any
//! provider. Built-in templates only seed creation; runtime data is personal records.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nc_code_channels::{
    CancelToken, Channel, ChannelContext, ChannelFailure, EventBus, EventSubscription,
};
use nc_code_rpc::WireValue;
use nc_code_service_core::{ServiceArgs, ServiceError};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::catalog::{self, Endpoint};
use crate::store::{
    default_model_config, model_effective_config, model_ids_from_config, new_provider_id,
    normalize_model_list, provider_effective_config, provider_order, templates, ModelRecord,
    ProviderRecord, ProviderStore, StoreSnapshot,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateInput {
    #[serde(default)]
    template_id: Option<String>,
    #[serde(default)]
    provider_name: Option<String>,
    #[serde(default)]
    locale: Option<String>,
    #[serde(default)]
    initial_config: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResolveInput {
    provider_id: String,
    #[serde(default)]
    model_id: Option<String>,
    #[serde(default)]
    original_model_id: Option<String>,
    #[serde(default)]
    personal_config: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveDraftInput {
    provider_id: String,
    original_model_id: String,
    next_model_id: String,
    personal_config: Value,
    #[serde(default)]
    use_recommended_config: Option<bool>,
    based_on_revision: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectivityInput {
    provider_id: String,
    model_id: String,
}

/// Fetch input parameters from the remote model directory (Auto fetch panel).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogInput {
    provider_id: String,
    #[serde(default)]
    force: bool,
}

/// Single model capability actual measurement input parameters; `checks` default = tools + thinking + throughput three full tests.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProbeInput {
    provider_id: String,
    model_id: String,
    #[serde(default)]
    checks: Option<Vec<String>>,
}

/// Batch import/remove input parameters of directory entries (one transaction to avoid N revision jitter).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelIdsInput {
    provider_id: String,
    model_ids: Vec<String>,
}

/// In-process remote directory cache: TTL and store revision double failure conditions to avoid hitting the upstream every time the panel is opened.
struct CachedCatalog {
    revision: u64,
    fetched_at: u64,
    base_url: String,
    api_type: String,
    models: Vec<Value>,
}

pub struct ProviderSettingsService {
    store: Arc<ProviderStore>,
    events: EventBus,
    selection_events: EventBus,
    catalogs: tokio::sync::Mutex<HashMap<String, CachedCatalog>>,
}

impl ProviderSettingsService {
    pub fn new(store: Arc<ProviderStore>, selection_events: EventBus) -> Arc<Self> {
        Arc::new(Self {
            store,
            events: EventBus::default(),
            selection_events,
            catalogs: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    pub async fn view(&self) -> Result<Value, ServiceError> {
        let snapshot = self.store.snapshot().await?;
        Ok(build_view(&snapshot))
    }

    /// The refresh of provisioning needs to reuse this broadcast (synchronize the provider-settings/model-selection view after writing the disk).
    pub async fn publish(&self, snapshot: &StoreSnapshot) {
        self.events.send(WireValue::Json(build_view(snapshot)));
        self.selection_events
            .send(WireValue::Json(crate::selection::build_view(
                snapshot, None,
            )));
    }

    async fn call_inner(&self, command: &str, args: Vec<WireValue>) -> Result<Value, ServiceError> {
        let args = ServiceArgs::new(args);
        match command {
            "getView" => self.view().await,
            "refresh" => {
                let snapshot = self.store.reload().await?;
                self.publish(&snapshot).await;
                Ok(build_view(&snapshot))
            }
            "createPersonalProvider" => {
                let input = args.typed::<CreateInput>(0)?.unwrap_or(CreateInput {
                    template_id: None,
                    provider_name: None,
                    locale: None,
                    initial_config: None,
                });
                self.create_provider(input).await
            }
            "resolveModelConfig" => {
                let input = args.required_typed::<ResolveInput>(0, "input")?;
                self.resolve_model_config(input).await
            }
            "savePersonalProviderOverlay" => {
                let provider_id = args.required_string(0, "providerId")?;
                let config = args.required_json(1, "config")?;
                let metadata = args.json(2)?;
                self.save_overlay(&provider_id, &config, metadata.as_ref())
                    .await
            }
            "deletePersonalProvider" => {
                let provider_id = args.required_string(0, "providerId")?;
                self.delete_provider(&provider_id).await
            }
            "reorderPersonalProviders" => {
                let ids = string_array(&args, 0, "providerIds")?;
                let snapshot = self
                    .store
                    .mutate(|snapshot| {
                        snapshot.provider_order = ids;
                        Ok(())
                    })
                    .await?;
                self.publish(&snapshot).await;
                Ok(build_view(&snapshot))
            }
            "reorderPersonalModels" => {
                let provider_id = args.required_string(0, "providerId")?;
                let ids = string_array(&args, 1, "modelIds")?;
                let snapshot = self
                    .store
                    .mutate(|snapshot| {
                        let provider = require_provider_mut(snapshot, &provider_id)?;
                        let mut ordered = Vec::new();
                        for id in &ids {
                            if let Some(position) = provider
                                .models
                                .iter()
                                .position(|model| model.model_id == *id)
                            {
                                ordered.push(provider.models.remove(position));
                            }
                        }
                        ordered.append(&mut provider.models);
                        provider.models = ordered;
                        Ok(())
                    })
                    .await?;
                self.publish(&snapshot).await;
                Ok(build_view(&snapshot))
            }
            "addPersonalModel" => {
                let provider_id = args.required_string(0, "providerId")?;
                let model_id = args.required_string(1, "modelId")?;
                let config = args.json(2)?.unwrap_or_else(default_model_config);
                let use_recommended = args.bool(3)?.unwrap_or(false);
                let snapshot = self
                    .store
                    .mutate(|snapshot| {
                        let provider = require_provider_mut(snapshot, &provider_id)?;
                        let mut config = config.clone();
                        if !config.is_object() {
                            return Err(ServiceError::invalid_params(
                                "model config must be an object",
                            ));
                        }
                        if config.get("enabled").is_none() {
                            config["enabled"] = Value::Bool(true);
                        }
                        match provider
                            .models
                            .iter_mut()
                            .find(|model| model.model_id == model_id)
                        {
                            Some(existing) => existing.config = config,
                            None => provider.models.push(ModelRecord {
                                model_id: model_id.clone(),
                                config,
                                enabled: true,
                                use_recommended,
                            }),
                        }
                        Ok(())
                    })
                    .await?;
                self.publish(&snapshot).await;
                Ok(build_view(&snapshot))
            }
            "renamePersonalModel" => {
                let provider_id = args.required_string(0, "providerId")?;
                let current = args.required_string(1, "currentModelId")?;
                let next = args.required_string(2, "nextModelId")?;
                let snapshot = self
                    .store
                    .mutate(|snapshot| rename_model(snapshot, &provider_id, &current, &next))
                    .await?;
                self.publish(&snapshot).await;
                Ok(build_view(&snapshot))
            }
            "deletePersonalModel" => {
                let provider_id = args.required_string(0, "providerId")?;
                let model_id = args.required_string(1, "modelId")?;
                let snapshot = self
                    .store
                    .mutate(|snapshot| {
                        let provider = require_provider_mut(snapshot, &provider_id)?;
                        let before = provider.models.len();
                        provider.models.retain(|model| model.model_id != model_id);
                        if provider.models.len() == before {
                            return Err(ServiceError::message(format!(
                                "Model not found: {model_id}"
                            )));
                        }
                        Ok(())
                    })
                    .await?;
                self.publish(&snapshot).await;
                Ok(build_view(&snapshot))
            }
            "savePersonalModelDraft" => {
                let input = args.required_typed::<SaveDraftInput>(0, "input")?;
                self.save_model_draft(input).await
            }
            "setPersonalModelEnabled" => {
                let provider_id = args.required_string(0, "providerId")?;
                let model_id = args.required_string(1, "modelId")?;
                let enabled = args
                    .bool(2)?
                    .ok_or_else(|| ServiceError::invalid_params("args[2] (enabled) is required"))?;
                let snapshot = self
                    .store
                    .mutate(|snapshot| {
                        let provider = require_provider_mut(snapshot, &provider_id)?;
                        let model = provider
                            .models
                            .iter_mut()
                            .find(|model| model.model_id == model_id)
                            .ok_or_else(|| {
                                ServiceError::message(format!("Model not found: {model_id}"))
                            })?;
                        model.enabled = enabled;
                        Ok(())
                    })
                    .await?;
                self.publish(&snapshot).await;
                Ok(build_view(&snapshot))
            }
            "testModelConnectivity" => {
                let input = args.required_typed::<ConnectivityInput>(0, "input")?;
                self.test_connectivity(input).await
            }
            "listRemoteModels" => {
                let input = args.required_typed::<CatalogInput>(0, "input")?;
                self.list_remote_models(input).await
            }
            "probeRemoteModel" => {
                let input = args.required_typed::<ProbeInput>(0, "input")?;
                self.probe_remote_model(input).await
            }
            "addRemoteModels" => {
                let input = args.required_typed::<ModelIdsInput>(0, "input")?;
                self.add_remote_models(input).await
            }
            "deletePersonalModels" => {
                let input = args.required_typed::<ModelIdsInput>(0, "input")?;
                self.delete_models(input).await
            }
            other => Err(ServiceError::message(format!("Method not found: {other}"))),
        }
    }

    async fn create_provider(&self, input: CreateInput) -> Result<Value, ServiceError> {
        let snapshot = self.store.snapshot().await?;
        let template = input
            .template_id
            .as_deref()
            .and_then(|template_id| snapshot.template(template_id));
        let initial = input.initial_config.unwrap_or_else(|| json!({}));
        let config = if let Some(template) = &template {
            let mut config = template.get("config").cloned().unwrap_or_else(|| json!({}));
            crate::store::deep_merge(&mut config, &initial);
            config
        } else {
            let mut config = json!({
                "group": "standard-personal",
                "visibility": "visible",
                "api": {"type": "openai-chat-completions", "baseUrl": null},
                "access": {"type": "api-key", "apiKey": null},
            });
            crate::store::deep_merge(&mut config, &initial);
            config
        };
        let provider_name = input
            .provider_name
            .or_else(|| {
                template.as_ref().and_then(|template| {
                    crate::store::template_name(template, input.locale.as_deref())
                })
            })
            .unwrap_or_else(|| "Provider".to_string());
        let mut models = Vec::new();
        normalize_model_list(&mut models, &model_ids_from_config(&config));
        let provider_id = new_provider_id(&snapshot);
        let snapshot = self
            .store
            .mutate(|snapshot| {
                snapshot.providers.push(ProviderRecord {
                    provider_id: provider_id.clone(),
                    provider_name,
                    template_id: input.template_id.clone(),
                    enabled: true,
                    config,
                    models,
                });
                snapshot.provider_order.push(provider_id.clone());
                Ok(())
            })
            .await?;
        self.publish(&snapshot).await;
        Ok(json!({
            "providerId": provider_id,
            "view": build_view(&snapshot),
        }))
    }

    async fn save_overlay(
        &self,
        provider_id: &str,
        config: &Value,
        metadata: Option<&Value>,
    ) -> Result<Value, ServiceError> {
        let config = config.clone();
        if !config.is_object() {
            return Err(ServiceError::invalid_params("config must be an object"));
        }
        let snapshot = self
            .store
            .mutate(|snapshot| {
                let provider = require_provider_mut(snapshot, provider_id)?;
                let mut merged = provider.config.clone();
                crate::store::deep_merge(&mut merged, &config);
                provider.config = merged;
                if let Some(metadata) = metadata.and_then(Value::as_object) {
                    if let Some(name) = metadata.get("providerName").and_then(Value::as_str) {
                        provider.provider_name = name.to_string();
                    }
                    if let Some(template_id) = metadata.get("templateId").and_then(Value::as_str) {
                        provider.template_id = Some(template_id.to_string());
                    }
                    if let Some(enabled) = metadata.get("enabled").and_then(Value::as_bool) {
                        provider.enabled = enabled;
                    }
                }
                Ok(())
            })
            .await?;
        self.publish(&snapshot).await;
        Ok(build_view(&snapshot))
    }

    async fn delete_provider(&self, provider_id: &str) -> Result<Value, ServiceError> {
        let provider_id = provider_id.to_string();
        let snapshot = self
            .store
            .mutate(|snapshot| {
                let before = snapshot.providers.len();
                snapshot
                    .providers
                    .retain(|provider| provider.provider_id != provider_id);
                if snapshot.providers.len() == before {
                    return Err(ServiceError::message(format!(
                        "Provider not found: {provider_id}"
                    )));
                }
                snapshot.provider_order.retain(|id| id != &provider_id);
                Ok(())
            })
            .await?;
        self.publish(&snapshot).await;
        Ok(build_view(&snapshot))
    }

    async fn resolve_model_config(&self, input: ResolveInput) -> Result<Value, ServiceError> {
        let snapshot = self.store.snapshot().await?;
        let provider = snapshot.provider(&input.provider_id).ok_or_else(|| {
            ServiceError::message(format!("Provider does not exist: {}", input.provider_id))
        })?;
        let model_id = input
            .model_id
            .clone()
            .or(input.original_model_id.clone())
            .ok_or_else(|| ServiceError::invalid_params("modelId is required"))?;
        let inherited = provider
            .models
            .iter()
            .find(|model| model.model_id == model_id)
            .map(model_effective_config)
            .unwrap_or_else(default_model_config);
        let mut effective = provider_effective_config(provider, &snapshot);
        crate::store::deep_merge(&mut effective, &inherited);
        if let Some(personal) = &input.personal_config {
            crate::store::deep_merge(&mut effective, personal);
        }
        Ok(json!({
            "inheritedConfig": inherited,
            "effectiveConfig": effective,
            "issues": [],
        }))
    }

    async fn save_model_draft(&self, input: SaveDraftInput) -> Result<Value, ServiceError> {
        let config = input.personal_config.clone();
        let use_recommended = input.use_recommended_config.unwrap_or(false);
        let snapshot = self
            .store
            .mutate(|snapshot| {
                if input.based_on_revision != snapshot.revision {
                    return Err(ServiceError::message("Provider Settings revision conflict"));
                }
                require_provider_mut(snapshot, &input.provider_id)?;
                if input.original_model_id != input.next_model_id {
                    rename_model(
                        snapshot,
                        &input.provider_id,
                        &input.original_model_id,
                        &input.next_model_id,
                    )?;
                }
                let provider = require_provider_mut(snapshot, &input.provider_id)?;
                match provider
                    .models
                    .iter_mut()
                    .find(|model| model.model_id == input.next_model_id)
                {
                    Some(model) => {
                        model.config = config.clone();
                        model.use_recommended = use_recommended;
                    }
                    None => provider.models.push(ModelRecord {
                        model_id: input.next_model_id.clone(),
                        config: config.clone(),
                        enabled: true,
                        use_recommended,
                    }),
                }
                Ok(())
            })
            .await?;
        self.publish(&snapshot).await;
        Ok(build_view(&snapshot))
    }

    /// Provider's outbound endpoint (baseUrl / apiType / apiKey); if any one is missing, it will fail closed directly.
    async fn endpoint_for(&self, provider_id: &str) -> Result<Endpoint, ServiceError> {
        let snapshot = self.store.snapshot().await?;
        let provider = snapshot
            .provider(provider_id)
            .ok_or_else(|| ServiceError::message(format!("Provider not found: {provider_id}")))?;
        let config = provider_effective_config(provider, &snapshot);
        let base_url = config
            .get("api")
            .and_then(|api| api.get("baseUrl"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ServiceError::invalid_params("Provider baseUrl is missing"))?
            .to_string();
        let api_type = config
            .get("api")
            .and_then(|api| api.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("openai-chat-completions")
            .to_string();
        let api_key = config
            .get("access")
            .and_then(|access| access.get("apiKey"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        if api_key.is_empty() {
            return Err(ServiceError::invalid_params("Provider apiKey is missing"));
        }
        Ok(Endpoint {
            base_url,
            api_type,
            api_key,
        })
    }

    /// Capture or reuse the remote directory and mark the existing/enabled models of this provider.
    async fn list_remote_models(&self, input: CatalogInput) -> Result<Value, ServiceError> {
        let endpoint = self.endpoint_for(&input.provider_id).await?;
        let snapshot = self.store.snapshot().await?;
        let now = catalog::now_ms();
        let mut catalogs = self.catalogs.lock().await;
        let fresh = !input.force
            && catalogs
                .get(&input.provider_id)
                .map(|cached| {
                    cached.revision == snapshot.revision
                        && now.saturating_sub(cached.fetched_at) < catalog::CATALOG_TTL_MS
                })
                .unwrap_or(false);
        if !fresh {
            let models = catalog::fetch_catalog(&endpoint).await?;
            catalogs.insert(
                input.provider_id.clone(),
                CachedCatalog {
                    revision: snapshot.revision,
                    fetched_at: now,
                    base_url: endpoint.base_url.clone(),
                    api_type: endpoint.api_type.clone(),
                    models,
                },
            );
        }
        let cached = catalogs
            .get(&input.provider_id)
            .ok_or_else(|| ServiceError::new("model catalog missing after fetch"))?;
        let provider = snapshot.provider(&input.provider_id).ok_or_else(|| {
            ServiceError::message(format!("Provider not found: {}", input.provider_id))
        })?;
        let existing: Vec<(String, bool)> = provider
            .models
            .iter()
            .map(|model| (model.model_id.clone(), model.enabled))
            .collect();
        let models = catalog::mark_existing(cached.models.clone(), &existing);
        Ok(json!({
            "baseUrl": cached.base_url,
            "apiType": cached.api_type,
            "fetchedAt": cached.fetched_at,
            "total": models.len(),
        "models": models,
        }))
    }

    /// Use the product's own provider harness (`nc-code-provider::executor`) to test the model capabilities.
    async fn probe_remote_model(&self, input: ProbeInput) -> Result<Value, ServiceError> {
        let endpoint = self.endpoint_for(&input.provider_id).await?;
        let checks = input.checks.unwrap_or_else(|| {
            vec![
                "toolCall".to_string(),
                "reasoning".to_string(),
                "throughput".to_string(),
            ]
        });
        catalog::probe_model(&endpoint, &input.model_id, &checks).await
    }

    /// Model entries in the cache directory (retrieved from the lock and cloned, the return value can be safely used to synchronize mutate closures).
    async fn catalog_entries(&self, provider_id: &str) -> Result<Vec<Value>, ServiceError> {
        let revision = self.store.snapshot().await?.revision;
        let cached = {
            let catalogs = self.catalogs.lock().await;
            catalogs.get(provider_id).map(|entry| CachedCatalog {
                revision: entry.revision,
                fetched_at: entry.fetched_at,
                base_url: entry.base_url.clone(),
                api_type: entry.api_type.clone(),
                models: entry.models.clone(),
            })
        };
        match cached {
            Some(entry) if entry.revision == revision => Ok(entry.models),
            _ => {
                self.list_remote_models(CatalogInput {
                    provider_id: provider_id.to_string(),
                    force: false,
                })
                .await?;
                let catalogs = self.catalogs.lock().await;
                Ok(catalogs
                    .get(provider_id)
                    .map(|entry| entry.models.clone())
                    .unwrap_or_default())
            }
        }
    }
    /// Batch drop directory entries. config source priority: cache directory (catalog/heuristic fact)
    /// > Name inference; never write empty configuration, otherwise the UI will display half the model.
    async fn add_remote_models(&self, input: ModelIdsInput) -> Result<Value, ServiceError> {
        if input.model_ids.is_empty() {
            return Err(ServiceError::invalid_params("modelIds is required"));
        }
        let entries = self.catalog_entries(&input.provider_id).await?;
        let configs: HashMap<String, Value> = entries
            .iter()
            .filter_map(|entry| {
                let model_id = entry.get("modelId").and_then(Value::as_str)?;
                Some((model_id.to_string(), catalog::to_model_config(entry)))
            })
            .collect();
        let snapshot = self
            .store
            .mutate(|snapshot| {
                let provider = require_provider_mut(snapshot, &input.provider_id)?;
                for model_id in &input.model_ids {
                    let config = configs
                        .get(model_id)
                        .cloned()
                        .unwrap_or_else(|| catalog::to_model_config(&catalog::infer(model_id)));
                    match provider
                        .models
                        .iter_mut()
                        .find(|model| &model.model_id == model_id)
                    {
                        Some(existing) => existing.config = config,
                        None => provider.models.push(ModelRecord {
                            model_id: model_id.clone(),
                            config,
                            enabled: true,
                            use_recommended: false,
                        }),
                    }
                }
                Ok(())
            })
            .await?;
        self.publish(&snapshot).await;
        Ok(build_view(&snapshot))
    }

    /// Batch removal (Remove action of Auto fetch panel); other than single deletePersonalModel
    /// The batch entry only serves one transaction to avoid N revision jitters.
    async fn delete_models(&self, input: ModelIdsInput) -> Result<Value, ServiceError> {
        if input.model_ids.is_empty() {
            return Err(ServiceError::invalid_params("modelIds is required"));
        }
        let removed = input.model_ids.clone();
        let snapshot = self
            .store
            .mutate(|snapshot| {
                let provider = require_provider_mut(snapshot, &input.provider_id)?;
                let before = provider.models.len();
                provider
                    .models
                    .retain(|model| !removed.iter().any(|id| id == &model.model_id));
                if provider.models.len() == before {
                    return Err(ServiceError::message("No matching models to remove"));
                }
                Ok(())
            })
            .await?;
        self.publish(&snapshot).await;
        Ok(build_view(&snapshot))
    }

    async fn test_connectivity(&self, input: ConnectivityInput) -> Result<Value, ServiceError> {
        let snapshot = self.store.snapshot().await?;
        let provider = snapshot.provider(&input.provider_id).ok_or_else(|| {
            ServiceError::message(format!("Provider does not exist: {}", input.provider_id))
        })?;
        let config = provider_effective_config(provider, &snapshot);
        let base_url = config
            .get("api")
            .and_then(|api| api.get("baseUrl"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ServiceError::invalid_params("Provider baseUrl is missing"))?;
        let api_type = config
            .get("api")
            .and_then(|api| api.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("openai-chat-completions");
        let api_key = config
            .get("access")
            .and_then(|access| access.get("apiKey"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if api_key.is_empty() {
            return Err(ServiceError::invalid_params("Provider apiKey is missing"));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|error| ServiceError::message(error.to_string()))?;
        let trimmed = base_url.trim_end_matches('/');
        let candidates = if trimmed.ends_with("/v1") {
            vec![format!("{trimmed}/models")]
        } else {
            vec![format!("{trimmed}/v1/models"), format!("{trimmed}/models")]
        };
        let mut last_error = None;
        for url in candidates {
            let request = client.get(&url);
            let request = if api_type == "anthropic-messages" {
                request
                    .header("x-api-key", api_key)
                    .header("anthropic-version", "2023-06-01")
            } else {
                request.bearer_auth(api_key)
            };
            match request.send().await {
                Ok(response) if response.status().is_success() => {
                    return Ok(json!({ "success": true, "modelId": input.model_id }));
                }
                Ok(response) => {
                    last_error = Some(format!("HTTP {}", response.status().as_u16()));
                }
                Err(error) => {
                    last_error = Some(error.to_string());
                }
            }
        }
        Err(ServiceError::message(format!(
            "Model connectivity failed: {}",
            last_error.unwrap_or_else(|| "unknown error".to_string())
        )))
    }
}

#[async_trait]
impl Channel for ProviderSettingsService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        self.call_inner(command, args)
            .await
            .map(WireValue::Json)
            .map_err(Into::into)
    }

    async fn listen(
        &self,
        _ctx: &ChannelContext,
        event: &str,
        _arg: WireValue,
    ) -> Result<EventSubscription, ChannelFailure> {
        match event {
            "onDidChange" => Ok(EventSubscription::from_broadcast(self.events.subscribe())),
            other => Err(ChannelFailure::event_not_found(other)),
        }
    }
}

fn string_array(
    args: &ServiceArgs,
    index: usize,
    label: &str,
) -> Result<Vec<String>, ServiceError> {
    let values = args.json_array(index)?.ok_or_else(|| {
        ServiceError::invalid_params(format!("args[{index}] ({label}) is required"))
    })?;
    values
        .into_iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| ServiceError::invalid_params(format!("{label} must be strings")))
        })
        .collect()
}

fn require_provider_mut<'a>(
    snapshot: &'a mut StoreSnapshot,
    provider_id: &str,
) -> Result<&'a mut ProviderRecord, ServiceError> {
    snapshot
        .provider_mut(provider_id)
        .ok_or_else(|| ServiceError::message(format!("Provider does not exist: {provider_id}")))
}

fn rename_model(
    snapshot: &mut StoreSnapshot,
    provider_id: &str,
    current: &str,
    next: &str,
) -> Result<(), ServiceError> {
    if current == next {
        return Ok(());
    }
    let provider = require_provider_mut(snapshot, provider_id)?;
    if provider.models.iter().any(|model| model.model_id == next) {
        return Err(ServiceError::message(format!(
            "Model already exists: {next}"
        )));
    }
    let model = provider
        .models
        .iter_mut()
        .find(|model| model.model_id == current)
        .ok_or_else(|| ServiceError::message(format!("Model not found: {current}")))?;
    model.model_id = next.to_string();
    Ok(())
}

pub fn build_view(snapshot: &StoreSnapshot) -> Value {
    json!({
        "revision": snapshot.revision,
        "providerTemplates": templates(),
        "providerOrder": provider_order(snapshot),
        "providers": snapshot
            .providers
            .iter()
            .map(|provider| crate::store::provider_view_record(provider, snapshot))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_service_core::AppPaths;
    use serde_json::json;

    fn temp_paths(tag: &str) -> (AppPaths, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "zcode-provider-test-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        (AppPaths::new(root.join("home"), root.join("data")), root)
    }

    async fn service(
        tag: &str,
    ) -> (
        Arc<ProviderSettingsService>,
        Arc<ProviderStore>,
        std::path::PathBuf,
    ) {
        let (paths, root) = temp_paths(tag);
        let store = ProviderStore::new(paths);
        let selection_events = EventBus::default();
        let service = ProviderSettingsService::new(Arc::clone(&store), selection_events);
        (service, store, root)
    }

    #[tokio::test]
    async fn create_from_template_exposes_models_and_requires_credentials() {
        let (service, _store, root) = service("create").await;
        let created = service
            .call_inner(
                "createPersonalProvider",
                vec![WireValue::Json(json!({"templateId": "zai-standard-api"}))],
            )
            .await
            .expect("create");
        let provider_id = created["providerId"].as_str().expect("id");
        let provider = created["view"]["providers"]
            .as_array()
            .expect("providers")
            .iter()
            .find(|provider| provider["providerId"] == json!(provider_id))
            .expect("created provider");
        assert_eq!(provider["templateId"], json!("zai-standard-api"));
        assert_eq!(provider["executable"], json!(false));
        assert!(!provider["models"].as_array().expect("models").is_empty());

        let view = service
            .call_inner(
                "savePersonalProviderOverlay",
                vec![
                    WireValue::String(provider_id.to_string()),
                    WireValue::Json(json!({
                        "access": {"type": "api-key", "apiKey": "test-key"}
                    })),
                ],
            )
            .await
            .expect("overlay");
        let provider = view["providers"]
            .as_array()
            .expect("providers")
            .iter()
            .find(|provider| provider["providerId"] == json!(provider_id))
            .expect("provider");
        assert_eq!(provider["executable"], json!(true));
        assert_eq!(
            provider["effectiveConfig"]["access"]["apiKey"],
            json!("test-key")
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn model_crud_and_selection_view_are_consistent() {
        let (service, store, root) = service("models").await;
        let created = service
            .call_inner(
                "createPersonalProvider",
                vec![WireValue::Json(json!({
                    "templateId": "zai-standard-api",
                    "initialConfig": {"access": {"type": "api-key", "apiKey": "k"}}
                }))],
            )
            .await
            .expect("create");
        let provider_id = created["providerId"].as_str().expect("id").to_string();
        service
            .call_inner(
                "addPersonalModel",
                vec![
                    WireValue::String(provider_id.to_string()),
                    WireValue::String("my-model".to_string()),
                    WireValue::Json(json!({"properties": {"contextWindow": 64000}})),
                ],
            )
            .await
            .expect("add");
        service
            .call_inner(
                "renamePersonalModel",
                vec![
                    WireValue::String(provider_id.to_string()),
                    WireValue::String("my-model".to_string()),
                    WireValue::String("renamed-model".to_string()),
                ],
            )
            .await
            .expect("rename");
        let view = service.view().await.expect("view");
        let models = view["providers"][0]["models"].as_array().expect("models");
        assert!(models
            .iter()
            .any(|model| model["modelId"] == json!("renamed-model")));
        let renamed = models
            .iter()
            .find(|model| model["modelId"] == json!("renamed-model"))
            .expect("renamed");
        assert_eq!(
            renamed["effectiveConfig"]["properties"]["contextWindow"],
            json!(64000)
        );

        let selection =
            crate::selection::build_view(&store.snapshot().await.expect("snapshot"), None);
        assert!(!selection["providers"]
            .as_array()
            .expect("providers")
            .is_empty());
        assert!(selection["preferredSelection"]["providerId"].is_string());

        service
            .call_inner(
                "deletePersonalModel",
                vec![
                    WireValue::String(provider_id.to_string()),
                    WireValue::String("renamed-model".to_string()),
                ],
            )
            .await
            .expect("delete");
        service
            .call_inner(
                "deletePersonalProvider",
                vec![WireValue::String(provider_id.to_string())],
            )
            .await
            .expect("delete provider");
        assert!(service.view().await.expect("view")["providers"]
            .as_array()
            .expect("providers")
            .is_empty());
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn store_persists_across_instances_and_rejects_stale_drafts() {
        let (paths, root) = temp_paths("persist");
        let store = ProviderStore::new(paths.clone());
        let service = ProviderSettingsService::new(Arc::clone(&store), EventBus::default());
        let created = service
            .call_inner(
                "createPersonalProvider",
                vec![WireValue::Json(json!({"initialConfig": {
                    "api": {"type": "openai-chat-completions", "baseUrl": "https://api.example.com/v1"},
                    "access": {"type": "api-key", "apiKey": "k"}
                }}))],
            )
            .await
            .expect("create");
        let provider_id = created["providerId"].as_str().expect("id").to_string();
        let revision = created["view"]["revision"].as_u64().expect("revision");

        let stale = service
            .call_inner(
                "savePersonalModelDraft",
                vec![WireValue::Json(json!({
                    "providerId": provider_id,
                    "originalModelId": "a",
                    "nextModelId": "a",
                    "personalConfig": crate::store::default_model_config(),
                    "basedOnRevision": revision - 1
                }))],
            )
            .await
            .expect_err("stale");
        assert!(stale.message.contains("revision conflict"));

        let reopened = ProviderStore::new(paths);
        let view = reopened.snapshot().await.expect("snapshot");
        assert_eq!(view.providers.len(), 1);
        assert_eq!(view.providers[0].provider_id, provider_id);
        std::fs::remove_dir_all(root).ok();
    }
}
