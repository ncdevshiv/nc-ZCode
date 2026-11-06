import io

path = 'crates/zcode-services-provider/src/settings.rs'
s = io.open(path, encoding='utf-8').read()

s = s.replace(''''#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectivityInput {
    provider_id: String,
    model_id: String,
}''','''#[derive(Deserialize)]
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

/// In-process caching of Auto fetch directories: TTL + provider records intergenerational double invalidation.
/// apiKey does not cache the value itself, and endpoint changes are determined by revision.
struct CachedCatalog {
    fetched_at_ms: u64,
    provider_revision: u64,
    api_type: String,
    base_url: String,
    entries: Vec<Value>,
}'', 1)

s = s.replace(''''pub struct ProviderSettingsService {
    store: Arc<ProviderStore>,
    events: EventBus,
    selection_events: EventBus,
}''','''pub struct ProviderSettingsService {
    store: Arc<ProviderStore>,
    events: EventBus,
    selection_events: EventBus,
    catalogs: tokio::sync::Mutex<HashMap<String, CachedCatalog>>,
}'', 1)

s = s.replace(''' store,
            events: EventBus::default(),
            selection_events,
        })'','''' store,
            events: EventBus::default(),
            selection_events,            catalogs: tokio::sync::Mutex::new(HashMap::new()),
        })''', 1)

io.open(path, 'w', encoding='utf-8', newline='').write(s)
print('inputs added')
