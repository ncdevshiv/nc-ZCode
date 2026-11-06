import io

path = 'crates/zcode-services-provider/src/settings.rs'
s = io.open(path, encoding='utf-8').read()

# 1) new input types + cache struct after ConnectivityInput
old = '''#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectivityInput {
    provider_id: String,
    model_id: String,
}'''
new = '''#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectivityInput {
    provider_id: String,
    model_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogInput {
    provider_id: String,
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProbeInput {
    provider_id: String,
    model_id: String,
    #[serde(default)]
    checks: Option<Vec<String>>,
}

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
}'''
assert old in s
s = s.replace(old, new, 1)

# 2) service struct field + constructor
old = '''pub struct ProviderSettingsService {
    store: Arc<ProviderStore>,
    events: EventBus,
    selection_events: EventBus,
}

impl ProviderSettingsService {
    pub fn new(store: Arc<ProviderStore>, selection_events: EventBus) -> Arc<Self> {
        Arc::new(Self {
            store,
            events: EventBus::default(),
            selection_events,
        })
    }'''
new = '''pub struct ProviderSettingsService {
    store: Arc<ProviderStore>,
    events: EventBus,
    selection_events: EventBus,
    catalogs: tokio::sync::Mutex<HashMap<String, CachedCatalog>>,
}impl ProviderSettingsService {
    pub fn new(store: Arc<ProviderStore>, selection_events: EventBus) -> Arc<Self> {
        Arc::new(Self {
            store,
            events: EventBus::default(),
            selection_events,
            catalogs: tokio::sync::Mutex::new(HashMap::new()),
        })
    }'''
assert old in s
s = s.replace(old, new, 1)

# 3) dispatch arms
old = ''' "testModelConnectivity" => {
                let input = args.required_typed::<ConnectivityInput>(0, "input")?;
                self.test_connectivity(input).await
            }'''
new = ''' "testModelConnectivity" => {
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
            }'''
assert old in s
s = s.replace(old, new, 1)

# 4) implementations before test_connectivity
old = ''' async fn test_connectivity(&self, input: ConnectivityInput) -> Result<Value, ServiceError> {'''
new = ''' /// Provider's outbound endpoint (baseUrl / apiType / apiKey); if any item is missing, it will fail closed directly.async fn endpoint_for(&self, provider_id: &str) -> Result<Endpoint, ServiceError> {
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
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_millis() as u64)
            .unwrap_or(0);let mut catalogs = self.catalogs.lock().await;
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

    /// Use the product's own provider harness (`zcode-provider::executor`) to test the model capabilities.async fn probe_remote_model(&self, input: ProbeInput) -> Result<Value, ServiceError> {
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
    /// > Name inference; never write empty configuration, otherwise the UI will display half the model.    async fn add_remote_models(&self, input: ModelIdsInput) -> Result<Value, ServiceError> {
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

/// Batch removal (Remove action of Auto fetch panel); except single deletePersonalModel
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

    async fn test_connectivity(&self, input: ConnectivityInput) -> Result<Value, ServiceError> {'''
assert old in s
s = s.replace(old, new, 1)

# 5) SystemTime imports
s = s.replace('''use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;''','''use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};''')

io.open(path, 'w', encoding='utf-8', newline='').write(s)
print('patched settings.rs')