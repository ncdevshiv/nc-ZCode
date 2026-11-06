//! Record models for plugin storage, mirroring `adapters/src/plugins/marketplace.ts`.
//!
//! serde_json runs with `preserve_order` workspace-wide, so building objects through
//! `serde_json::Map` keeps the exact key insertion order the TypeScript literals use.

use serde_json::{Map, Value};

pub const OFFICIAL_MARKETPLACE_ID: &str = "zcode-plugins-official";
pub const KNOWN_MARKETPLACES_FILE: &str = "known_marketplaces.json";
pub const INSTALLED_PLUGINS_FILE: &str = "installed_plugins.json";
pub const MARKETPLACE_FILE: &str = "marketplace.json";
pub const DEFAULT_VERSION: &str = "0.0.0";

pub fn is_official_marketplace_id(id: &str) -> bool {
    id == OFFICIAL_MARKETPLACE_ID
}

/// Marketplace ids must be namespaced to prevent cross-marketplace confusion
/// (TS `MARKETPLACE_NAME_PATTERN`/`PLUGIN_NAME_PATTERN`).
pub fn is_valid_namespaced_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 128 {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    bytes[1..].iter().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
    })
}

pub fn sanitize_plugin_id(raw: &str) -> String {
    raw.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '@' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

/// `<name>@<marketplace>` split on the last `@` (TS `parsePluginId`).
pub fn parse_plugin_id(plugin_id: &str) -> Result<(String, String), String> {
    let at = plugin_id.rfind('@');
    match at {
        Some(index) if index > 0 && index + 1 < plugin_id.len() => Ok((
            plugin_id[..index].to_string(),
            plugin_id[index + 1..].to_string(),
        )),
        _ => Err(format!(
            "Plugin id must use <name>@<marketplace>: {plugin_id}"
        )),
    }
}

pub fn qualify_dependency(dependency: &str, marketplace: &str) -> String {
    if dependency.contains('@') {
        dependency.to_string()
    } else {
        format!("{dependency}@{marketplace}")
    }
}

/// Strip npm-style version ranges from a dependency reference (TS
/// `normalizeDependencyRef`: `name@^1.2.3` → `name`).
pub fn normalize_dependency_ref(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => {
            // Remove a trailing `@^range` segment when no further '@' follows it,
            // mirroring TS `text.replace(/@\^[^@]*$/u, "")`.
            if let Some(index) = text.rfind("@^") {
                if !text[index + 1..].contains('@') {
                    return Some(text[..index].to_string());
                }
            }
            Some(text.clone())
        }
        Value::Object(record) => {
            let name = record
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if name.is_empty() {
                return None;
            }
            let marketplace = record
                .get("marketplace")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            Some(if marketplace.is_empty() {
                name.to_string()
            } else {
                format!("{name}@{marketplace}")
            })
        }
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct KnownMarketplace {
    pub id: String,
    pub source: Value,
    pub name: String,
    pub description: Option<String>,
    pub added_at: String,
    pub last_updated: Option<String>,
    pub plugin_count: u64,
    pub cache_transaction_id: Option<String>,
    pub last_refresh_failure: Option<Value>,
}

impl KnownMarketplace {
    pub fn from_value(value: &Value) -> Option<Self> {
        let record = value.as_object()?;
        Some(Self {
            id: record.get("id")?.as_str()?.to_string(),
            source: record.get("source")?.clone(),
            name: record.get("name")?.as_str()?.to_string(),
            description: record
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            added_at: record
                .get("addedAt")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            last_updated: record
                .get("lastUpdated")
                .and_then(Value::as_str)
                .map(str::to_string),
            plugin_count: record
                .get("pluginCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cache_transaction_id: record
                .get("cacheTransactionId")
                .and_then(Value::as_str)
                .map(str::to_string),
            last_refresh_failure: record.get("lastRefreshFailure").cloned(),
        })
    }

    /// Key order mirrors the `KnownMarketplaceRecord` literal in `addMarketplace`;
    /// `lastRefreshFailure` is appended by `persistMarketplaceRefreshFailure` spreads.
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("id".into(), Value::String(self.id.clone()));
        map.insert("source".into(), self.source.clone());
        map.insert("name".into(), Value::String(self.name.clone()));
        if let Some(description) = &self.description {
            map.insert("description".into(), Value::String(description.clone()));
        }
        map.insert("addedAt".into(), Value::String(self.added_at.clone()));
        if let Some(last_updated) = &self.last_updated {
            map.insert("lastUpdated".into(), Value::String(last_updated.clone()));
        }
        map.insert("pluginCount".into(), Value::from(self.plugin_count));
        if let Some(transaction) = &self.cache_transaction_id {
            map.insert(
                "cacheTransactionId".into(),
                Value::String(transaction.clone()),
            );
        }
        if let Some(failure) = &self.last_refresh_failure {
            map.insert("lastRefreshFailure".into(), failure.clone());
        }
        Value::Object(map)
    }

    pub fn summary_projection(
        &self,
        featured: Option<Vec<String>>,
        plugin_count: Option<u64>,
    ) -> Value {
        // Order mirrors bootstrap `toMarketplaceSummaryData`.
        let mut map = Map::new();
        map.insert("id".into(), Value::String(self.id.clone()));
        map.insert("name".into(), Value::String(self.name.clone()));
        map.insert("source".into(), self.source.clone());
        if let Some(description) = &self.description {
            map.insert("description".into(), Value::String(description.clone()));
        }
        if let Some(last_updated) = &self.last_updated {
            map.insert("lastUpdated".into(), Value::String(last_updated.clone()));
        }
        map.insert(
            "pluginCount".into(),
            Value::from(plugin_count.unwrap_or(self.plugin_count)),
        );
        map.insert(
            "isOfficial".into(),
            Value::Bool(is_official_marketplace_id(&self.id)),
        );
        if let Some(failure) = &self.last_refresh_failure {
            let mut projection = Map::new();
            projection.insert(
                "code".into(),
                failure.get("code").cloned().unwrap_or(Value::Null),
            );
            projection.insert(
                "failedAt".into(),
                failure.get("failedAt").cloned().unwrap_or(Value::Null),
            );
            projection.insert(
                "message".into(),
                failure.get("message").cloned().unwrap_or(Value::Null),
            );
            map.insert("refreshFailure".into(), Value::Object(projection));
        }
        if let Some(featured) = featured.filter(|items| !items.is_empty()) {
            map.insert(
                "featured".into(),
                Value::Array(featured.into_iter().map(Value::String).collect()),
            );
        }
        Value::Object(map)
    }
}

#[derive(Debug, Clone)]
pub struct InstalledPlugin {
    pub id: String,
    pub name: String,
    pub marketplace: String,
    pub version: String,
    pub install_path: String,
    pub installed_at: String,
    pub updated_at: Option<String>,
    pub scope: String,
    pub dependencies: Option<Vec<String>>,
    pub source: Option<Value>,
    pub cache_transaction_id: Option<String>,
}

impl InstalledPlugin {
    pub fn from_value(value: &Value) -> Option<Self> {
        let record = value.as_object()?;
        let id = record.get("id")?.as_str()?;
        let name = record.get("name").and_then(Value::as_str);
        let marketplace = record.get("marketplace").and_then(Value::as_str);
        let install_path = record.get("installPath").and_then(Value::as_str)?;
        let (fallback_name, fallback_marketplace) = parse_plugin_id(id)
            .map(|(name, marketplace)| (Some(name), Some(marketplace)))
            .unwrap_or((None, None));
        Some(Self {
            id: id.to_string(),
            name: name.map(str::to_string).or(fallback_name)?,
            marketplace: marketplace.map(str::to_string).or(fallback_marketplace)?,
            version: record
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_VERSION)
                .to_string(),
            install_path: install_path.to_string(),
            installed_at: record
                .get("installedAt")
                .and_then(Value::as_str)
                .unwrap_or("1970-01-01T00:00:00.000Z")
                .to_string(),
            updated_at: record
                .get("updatedAt")
                .or_else(|| record.get("lastUpdated"))
                .and_then(Value::as_str)
                .map(str::to_string),
            // Legacy `project`/`local` scopes normalize to `workspace` (TS reader).
            scope: match record.get("scope").and_then(Value::as_str) {
                Some("project") | Some("local") => "workspace".to_string(),
                Some(other) => other.to_string(),
                None => "user".to_string(),
            },
            dependencies: record.get("dependencies").and_then(|value| {
                value.as_array().map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
            }),
            source: record.get("source").cloned(),
            cache_transaction_id: record
                .get("cacheTransactionId")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }

    /// Key order mirrors the `InstalledPluginRecord` literal in `cacheMarketplacePlugin`.
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("id".into(), Value::String(self.id.clone()));
        map.insert("name".into(), Value::String(self.name.clone()));
        map.insert(
            "marketplace".into(),
            Value::String(self.marketplace.clone()),
        );
        map.insert("version".into(), Value::String(self.version.clone()));
        map.insert(
            "installPath".into(),
            Value::String(self.install_path.clone()),
        );
        map.insert(
            "installedAt".into(),
            Value::String(self.installed_at.clone()),
        );
        if let Some(updated_at) = &self.updated_at {
            map.insert("updatedAt".into(), Value::String(updated_at.clone()));
        }
        map.insert("scope".into(), Value::String(self.scope.clone()));
        if let Some(dependencies) = &self.dependencies {
            map.insert(
                "dependencies".into(),
                Value::Array(
                    dependencies
                        .iter()
                        .map(|item| Value::String(item.clone()))
                        .collect(),
                ),
            );
        }
        if let Some(source) = &self.source {
            map.insert("source".into(), source.clone());
        }
        if let Some(transaction) = &self.cache_transaction_id {
            map.insert(
                "cacheTransactionId".into(),
                Value::String(transaction.clone()),
            );
        }
        Value::Object(map)
    }
}

/// Identity pin axis for update detection (TS `readPluginSourceIdentityPin`).
pub fn read_source_identity_pin(source: Option<&Value>) -> Option<String> {
    let record = source?.as_object()?;
    let kind = record.get("source").and_then(Value::as_str).unwrap_or("");
    if kind == "url" && record.get("type").and_then(Value::as_str) == Some("zip") {
        if let Some(sha) = record.get("sha256").and_then(Value::as_str) {
            if sha.len() == 64 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Some(sha.to_ascii_lowercase());
            }
        }
    }
    record
        .get("sha")
        .and_then(Value::as_str)
        .or_else(|| record.get("commit").and_then(Value::as_str))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plugin_id_splits_on_last_at() {
        assert_eq!(
            parse_plugin_id("a@b@c").unwrap(),
            ("a@b".to_string(), "c".to_string())
        );
        assert!(parse_plugin_id("@only-marketplace").is_err());
        assert!(parse_plugin_id("no-at-sign").is_err());
        assert!(parse_plugin_id("trailing@").is_err());
    }

    #[test]
    fn sanitize_keeps_safe_characters() {
        assert_eq!(sanitize_plugin_id("a/b c.d@e-f_1"), "a-b-c.d@e-f_1");
    }

    #[test]
    fn namespaced_name_pattern_rejects_uppercase_and_leading_symbols() {
        assert!(is_valid_namespaced_name("zcode-plugins-official"));
        assert!(is_valid_namespaced_name("a1.b_c-d"));
        assert!(!is_valid_namespaced_name("Bad"));
        assert!(!is_valid_namespaced_name("-bad"));
    }

    #[test]
    fn dependency_refs_strip_version_ranges_and_qualify() {
        assert_eq!(
            normalize_dependency_ref(&json!("core-dep@^1.2.3")).as_deref(),
            Some("core-dep")
        );
        assert_eq!(
            normalize_dependency_ref(&json!("other@market")).as_deref(),
            Some("other@market")
        );
        assert_eq!(qualify_dependency("plain", "mp"), "plain@mp");
    }

    #[test]
    fn installed_record_preserves_typescript_key_order() {
        let record = InstalledPlugin {
            id: "x@mp".into(),
            name: "x".into(),
            marketplace: "mp".into(),
            version: "1.0.0".into(),
            install_path: "/tmp/x".into(),
            installed_at: "t".into(),
            updated_at: None,
            scope: "user".into(),
            dependencies: None,
            source: None,
            cache_transaction_id: None,
        };
        let value = record.to_value();
        let keys: Vec<&String> = value.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec![
                "id",
                "name",
                "marketplace",
                "version",
                "installPath",
                "installedAt",
                "scope"
            ]
        );
    }

    #[test]
    fn installed_record_reads_legacy_map_scope() {
        let record = InstalledPlugin::from_value(&json!({
            "id": "legacy@mp",
            "installPath": "/tmp/legacy",
            "scope": "project",
            "lastUpdated": "2026-01-01T00:00:00.000Z"
        }))
        .expect("record");
        assert_eq!(record.scope, "workspace");
        assert_eq!(record.name, "legacy");
        assert_eq!(record.marketplace, "mp");
        assert_eq!(
            record.updated_at.as_deref(),
            Some("2026-01-01T00:00:00.000Z")
        );
    }

    #[test]
    fn identity_pin_prefers_zip_sha_then_sha_then_commit() {
        let zip_sha = "a".repeat(64);
        assert_eq!(
            read_source_identity_pin(Some(
                &json!({"source":"url","type":"zip","sha256": zip_sha})
            ))
            .as_deref(),
            Some(zip_sha.as_str())
        );
        assert_eq!(
            read_source_identity_pin(Some(&json!({"source":"git","sha":"deadbeef"}))).as_deref(),
            Some("deadbeef")
        );
        assert_eq!(
            read_source_identity_pin(Some(&json!({"source":"github","commit":"cafe"}))).as_deref(),
            Some("cafe")
        );
        assert_eq!(read_source_identity_pin(Some(&json!("filesystem"))), None);
    }
}
