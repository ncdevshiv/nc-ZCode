//! Manifest parsing: the marketplace catalog (`marketplace.json` /
//! `.claude-plugin/marketplace.json`), plugin manifests
//! (`.zcode-plugin|claude-plugin|codex-plugin/plugin.json`) and the markdown
//! frontmatter used for component names. Mirrors
//! `adapters/src/plugins/marketplace.ts` + `markdown-frontmatter.ts`.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::fsutil::is_file;
use crate::types::{is_valid_namespaced_name, DEFAULT_VERSION};

pub const ZCODE_MANIFEST_PATH: &str = ".zcode-plugin/plugin.json";
pub const NCCODE_MANIFEST_PATH: &str = ".nc-code-plugin/plugin.json";
pub const CLAUDE_MANIFEST_PATH: &str = ".claude-plugin/plugin.json";
pub const CODEX_MANIFEST_PATH: &str = ".codex-plugin/plugin.json";

/// Parsed marketplace catalog entry. `raw` keeps the original JSON object (key order
/// preserved) exactly like TS `PluginMarketplaceEntry.raw`.
#[derive(Debug, Clone)]
pub struct MarketplaceEntry {
    pub name: String,
    pub category: Option<String>,
    pub description: Option<String>,
    pub version: Option<String>,
    pub source: Option<Value>,
    pub cache_path: Option<String>,
    pub dependencies: Vec<String>,
    pub strict: bool,
    pub tags: Vec<String>,
    pub listing: Option<Map<String, Value>>,
    pub raw: Map<String, Value>,
}

#[derive(Debug, Clone)]
pub struct MarketplaceManifest {
    pub name: String,
    pub description: Option<String>,
    pub plugins: Vec<MarketplaceEntry>,
    pub allow_cross_marketplace_dependencies_on: Vec<String>,
    pub plugin_root: Option<String>,
    pub featured: Vec<String>,
    pub raw: Value,
}

/// `parseMarketplaceManifest`: returns None for invalid manifests (bad name or shape).
pub fn parse_marketplace_manifest(value: &Value) -> Option<MarketplaceManifest> {
    let record = value.as_object()?;
    let name = record
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if !is_valid_namespaced_name(name) {
        return None;
    }
    let raw_plugins = record.get("plugins").cloned().unwrap_or(Value::Null);
    let plugin_items: Vec<Value> = match &raw_plugins {
        Value::Array(items) => items.clone(),
        Value::Object(entries) => entries
            .iter()
            .map(|(plugin_name, plugin)| match plugin.as_object() {
                Some(object) => {
                    let mut merged = object.clone();
                    merged.insert("name".to_string(), Value::String(plugin_name.clone()));
                    Value::Object(merged)
                }
                None => Value::Object({
                    let mut single = Map::new();
                    single.insert("name".to_string(), Value::String(plugin_name.clone()));
                    single
                }),
            })
            .collect(),
        _ => Vec::new(),
    };
    let plugins = plugin_items
        .iter()
        .filter_map(parse_marketplace_entry)
        .collect();
    let metadata = record.get("metadata").and_then(Value::as_object);
    Some(MarketplaceManifest {
        name: name.to_string(),
        description: record
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                metadata
                    .and_then(|m| m.get("description"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }),
        plugins,
        allow_cross_marketplace_dependencies_on: record
            .get("allowCrossMarketplaceDependenciesOn")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        plugin_root: metadata
            .and_then(|m| m.get("pluginRoot"))
            .and_then(Value::as_str)
            .map(str::to_string),
        featured: record
            .get("featured")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        raw: value.clone(),
    })
}

/// The strict variant used when *adding* a marketplace: invalid manifests error out.
pub fn parse_required_marketplace_manifest(value: &Value) -> Result<MarketplaceManifest, String> {
    parse_marketplace_manifest(value).ok_or_else(|| "Marketplace manifest is invalid".to_string())
}

fn parse_marketplace_entry(value: &Value) -> Option<MarketplaceEntry> {
    let record = value.as_object()?;
    let name = record
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if name.is_empty() {
        return None;
    }
    let dependencies = record
        .get("dependencies")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(crate::types::normalize_dependency_ref)
                .collect()
        })
        .unwrap_or_default();
    let tags = record
        .get("tags")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Some(MarketplaceEntry {
        name: name.to_string(),
        category: record
            .get("category")
            .and_then(Value::as_str)
            .map(str::to_string),
        description: record
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        version: record
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_string),
        source: record.get("source").cloned(),
        cache_path: record
            .get("cachePath")
            .and_then(Value::as_str)
            .map(str::to_string),
        dependencies,
        strict: record
            .get("strict")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        tags,
        listing: parse_entry_store_listing(record),
        raw: record.clone(),
    })
}

/// `parseEntryStoreListing`: display metadata whitelist, camelCase output, in TS
/// insertion order; returns None when the entry carries no listing fields.
pub fn parse_entry_store_listing(entry: &Map<String, Value>) -> Option<Map<String, Value>> {
    let read_string = |key: &str| -> Option<String> {
        entry
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let read_string_map = |key: &str| -> Option<Map<String, Value>> {
        let record = entry.get(key)?.as_object()?;
        let filtered: Map<String, Value> = record
            .iter()
            .filter(|(_, value)| value.is_string())
            .map(|(locale, value)| (locale.clone(), value.clone()))
            .collect();
        (!filtered.is_empty()).then_some(filtered)
    };
    let read_list_map = |key: &str| -> Option<Map<String, Value>> {
        let record = entry.get(key)?.as_object()?;
        let filtered: Map<String, Value> = record
            .iter()
            .filter_map(|(locale, list)| {
                let items: Vec<Value> = list
                    .as_array()?
                    .iter()
                    .filter(|item| item.is_string())
                    .cloned()
                    .collect();
                (!items.is_empty()).then(|| (locale.clone(), Value::Array(items)))
            })
            .collect();
        (!filtered.is_empty()).then_some(filtered)
    };

    let mut listing = Map::new();
    if let Some(display_name) = read_string("displayName") {
        listing.insert("displayName".into(), Value::String(display_name));
    }
    if let Some(map) = read_string_map("displayName_i18n") {
        listing.insert("displayNameI18n".into(), Value::Object(map));
    }
    if let Some(map) = read_string_map("description_i18n") {
        listing.insert("descriptionI18n".into(), Value::Object(map));
    }
    for key in [
        "icon",
        "category",
        "homepage",
        "privacyPolicy",
        "termsOfService",
        "heroImage",
    ] {
        if let Some(value) = read_string(key) {
            listing.insert(key.into(), Value::String(value));
        }
    }
    if let Some(author) = normalize_author(entry.get("author")) {
        if let Some(name) = author.get("name").and_then(Value::as_str) {
            listing.insert("author".into(), Value::String(name.to_string()));
        }
        if let Some(url) = author.get("url").and_then(Value::as_str) {
            listing.insert("authorUrl".into(), Value::String(url.to_string()));
        }
    }
    let example_prompts: Vec<Value> = entry
        .get("examplePrompts")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.as_str().is_some_and(|text| !text.trim().is_empty()))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if !example_prompts.is_empty() {
        listing.insert("examplePrompts".into(), Value::Array(example_prompts));
    }
    if let Some(map) = read_list_map("examplePrompts_i18n") {
        listing.insert("examplePromptsI18n".into(), Value::Object(map));
    }
    // Only an explicit boolean `true` marks a paid-plan plugin (TS strictness rule).
    if entry.get("requiresPaidPlan") == Some(&Value::Bool(true)) {
        listing.insert("requiresPaidPlan".into(), Value::Bool(true));
    }
    (!listing.is_empty()).then_some(listing)
}

/// `normalizeAuthorValue`: string or `{name,url}` object.
pub fn normalize_author(value: Option<&Value>) -> Option<Map<String, Value>> {
    match value? {
        Value::String(text) => {
            let name = text.trim();
            (!name.is_empty()).then(|| {
                let mut map = Map::new();
                map.insert("name".into(), Value::String(name.to_string()));
                map
            })
        }
        Value::Object(record) => {
            let name = record
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let url = record
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if name.is_empty() && url.is_empty() {
                return None;
            }
            let mut map = Map::new();
            if !name.is_empty() {
                map.insert("name".into(), Value::String(name.to_string()));
            }
            if !url.is_empty() {
                map.insert("url".into(), Value::String(url.to_string()));
            }
            Some(map)
        }
        _ => None,
    }
}

pub fn find_plugin_manifest_path(root: &Path) -> Option<PathBuf> {
    for candidate in [
        ZCODE_MANIFEST_PATH,
        NCCODE_MANIFEST_PATH,
        CLAUDE_MANIFEST_PATH,
        CODEX_MANIFEST_PATH,
    ] {
        let path = root.join(candidate);
        if is_file(&path) {
            return Some(path);
        }
    }
    None
}

/// `findMarketplaceManifestPath` for a repository directory.
pub fn find_marketplace_manifest_file(root: &Path, explicit: Option<&str>) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(explicit) = explicit {
        candidates.push(root.join(explicit));
    }
    candidates.push(root.join(".claude-plugin/marketplace.json"));
    candidates.push(root.join("marketplace.json"));
    candidates.into_iter().find(|path| is_file(path))
}

/// `resolveInstalledPluginVersion`: the plugin's own manifest version wins, then the
/// marketplace entry version, then `0.0.0`.
pub fn resolve_installed_plugin_version(root: &Path, entry_version: Option<&str>) -> String {
    if let Some(path) = find_plugin_manifest_path(root) {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
                if let Some(version) = parsed.get("version").and_then(Value::as_str) {
                    if !version.trim().is_empty() {
                        return version.to_string();
                    }
                }
            }
        }
    }
    entry_version.unwrap_or(DEFAULT_VERSION).to_string()
}

/// Minimal frontmatter reader for component `.md` files: a leading `---` block with
/// `key: value` lines (name/description are the only consumers).
pub fn read_markdown_frontmatter(path: &Path) -> Map<String, Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Map::new();
    };
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return Map::new();
    }
    let mut out = Map::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" || trimmed == "..." {
            break;
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim();
            let value = value.trim().trim_matches('"').trim_matches('\'');
            if !key.is_empty() && !value.is_empty() {
                out.insert(key.to_string(), Value::String(value.to_string()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_marketplace_manifest_rejects_invalid_name() {
        // Name is lowercase alphanumeric ._- combination (≤128); uppercase/slash/empty/missing → None (fail-closed).
        assert!(parse_marketplace_manifest(&json!({"name": "", "plugins": []})).is_none());
        assert!(parse_marketplace_manifest(&json!({"name": "UPPER", "plugins": []})).is_none());
        assert!(parse_marketplace_manifest(&json!({"name": "has/slash", "plugins": []})).is_none());
        assert!(parse_marketplace_manifest(&json!({"plugins": []})).is_none());
        // Legal lowercase names can pass the name gate (do not fail due to missing plugins).assert!(parse_marketplace_manifest(&json!({"name": "zcode-official"})).is_some());
    }

    #[test]
    fn parse_marketplace_manifest_reads_array_plugins() {
        let manifest = parse_marketplace_manifest(&json!({
            "name": "zcode-official",
            "plugins": [
                {"name": "alpha", "version": "1.0.0", "description": "A"},
                {"name": "beta", "version": "2.0.0"},
            ],
        }))
        .expect("valid manifest");
        assert_eq!(manifest.name, "zcode-official");
        assert_eq!(manifest.plugins.len(), 2);
        assert_eq!(manifest.plugins[0].name, "alpha");
        assert_eq!(manifest.plugins[0].version.as_deref(), Some("1.0.0"));
        assert_eq!(manifest.plugins[1].name, "beta");
    }

    #[test]
    fn parse_marketplace_manifest_reads_object_form_plugins() {
        // Object form: The key is the plug-in name, which is injected into the entry (isomorphic to the array form).
        let manifest = parse_marketplace_manifest(&json!({
            "name": "zcode-official",
            "plugins": {
                "gamma": {"version": "3.1.4"},
            },
        }))
        .expect("valid manifest");
        assert_eq!(manifest.plugins.len(), 1);
        assert_eq!(manifest.plugins[0].name, "gamma");
        assert_eq!(manifest.plugins[0].version.as_deref(), Some("3.1.4"));
    }

    #[test]
    fn required_variant_surfaces_error_for_invalid() {
        assert!(parse_required_marketplace_manifest(&json!({"name": "BAD"})).is_err());
        let ok = parse_required_marketplace_manifest(&json!({"name": "official"})).expect("valid");
        assert_eq!(ok.name, "official");
    }
}
