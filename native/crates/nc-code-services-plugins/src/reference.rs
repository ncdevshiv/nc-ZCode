//! Conversation reference catalogs — port of
//! `bootstrap/src/nc-code-protocol/plugin-reference-catalog.ts` and
//! `skill-reference-catalog.ts`, plus `buildPluginReferenceCatalog`
//! (`packages/core/src/plugin-reference/catalog.ts`).
//!
//! Two projections share one source of truth:
//! - the plugin catalog is built from [`crate::discovery::DiscoveryOutcome`] (the same
//!   metadata `listPlugins` returns) and joins store-listing display fields from the
//!   overview;
//! - the skill catalog is the `skills` channel's discovery result, re-projected into the
//!   identifiers-only reference shape. Skill discovery stays owned by that channel — this
//!   module never re-implements the scan policy.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::discovery::PluginMetadata;
use crate::orchestration::OverviewData;

/// One audit-free identity entry (`PluginReferenceCatalogEntry`).
#[derive(Clone)]
pub struct CatalogEntry {
    pub plugin_id: String,
    pub name: String,
    pub marketplace: String,
    pub enabled: bool,
    pub conflicting_plugin_ids: Vec<String>,
    pub skill_qualified_names: Vec<String>,
    pub mcp_server_names: Vec<String>,
    pub subagent_names: Vec<String>,
}

/// `buildPluginReferenceCatalog`: every discovered plugin (enabled or not) enters the
/// catalog; only enabled plugins participate in the same-name conflict projection.
pub fn build_catalog(plugins: &[PluginMetadata]) -> Vec<CatalogEntry> {
    let mut enabled_by_name: HashMap<&str, Vec<&str>> = HashMap::new();
    for plugin in plugins {
        if !plugin.enabled {
            continue;
        }
        enabled_by_name
            .entry(plugin.name.as_str())
            .or_default()
            .push(plugin.id.as_str());
    }
    plugins
        .iter()
        .map(|plugin| {
            let mut conflicting: Vec<String> = if plugin.enabled {
                enabled_by_name
                    .get(plugin.name.as_str())
                    .map(|ids| {
                        ids.iter()
                            .filter(|id| **id != plugin.id)
                            .map(|id| (*id).to_string())
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            conflicting.sort();
            let mut mcp_server_names: Vec<String> = plugin.mcp_server_names.clone();
            mcp_server_names.sort();
            CatalogEntry {
                plugin_id: plugin.id.clone(),
                name: plugin.name.clone(),
                marketplace: plugin.marketplace.clone(),
                enabled: plugin.enabled,
                conflicting_plugin_ids: conflicting,
                skill_qualified_names: qualified_component_names(plugin, "skill"),
                mcp_server_names,
                subagent_names: qualified_component_names(plugin, "agent"),
            }
        })
        .collect()
}

/// `collectDeclared*QualifiedNames`: `<plugin>:<component>` for one component group.
fn qualified_component_names(plugin: &PluginMetadata, kind: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for group in &plugin.components {
        if group.get("kind").and_then(Value::as_str) != Some(kind) {
            continue;
        }
        let Some(items) = group.get("items").and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            let Some(name) = item.get("name").and_then(Value::as_str) else {
                continue;
            };
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let qualified = format!("{}:{name}", plugin.name);
            if !names.contains(&qualified) {
                names.push(qualified);
            }
        }
    }
    names.sort();
    names
}

/// Store-listing display projection (`PluginReferenceListingDisplay`). Display-only:
/// it never participates in identity, permissions or the runtime reminder.
#[derive(Clone, Default)]
pub struct ListingDisplay {
    pub category: Option<String>,
    pub icon: Option<String>,
    pub display_name: Option<String>,
    pub display_name_i18n: Option<Value>,
    pub description: Option<String>,
    pub description_i18n: Option<Value>,
}

/// `resolveReferenceListingDisplayByPluginId`: available, then installed, then restorable
/// rows; later rows only fill fields they actually carry.
pub fn listing_display_by_plugin_id(overview: &OverviewData) -> HashMap<String, ListingDisplay> {
    let mut display_by_id: HashMap<String, ListingDisplay> = HashMap::new();
    for summary in overview
        .available
        .iter()
        .chain(overview.installed.iter())
        .chain(overview.restorable.iter())
    {
        let Some(id) = summary
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let listing = summary.get("listing");
        let read = |key: &str| {
            listing
                .and_then(|listing| listing.get(key))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        };
        let display = ListingDisplay {
            category: read("category"),
            icon: read("icon"),
            display_name: read("displayName"),
            display_name_i18n: read_i18n(listing, "displayNameI18n"),
            description: summary
                .get("description")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string),
            description_i18n: read_i18n(listing, "descriptionI18n"),
        };
        if display.category.is_none()
            && display.icon.is_none()
            && display.display_name.is_none()
            && display.display_name_i18n.is_none()
            && display.description.is_none()
            && display.description_i18n.is_none()
        {
            continue;
        }
        let entry = display_by_id.entry(id.to_string()).or_default();
        merge_display(entry, display);
    }
    display_by_id
}

fn read_i18n(listing: Option<&Value>, key: &str) -> Option<Value> {
    listing
        .and_then(|listing| listing.get(key))
        .filter(|value| value.is_object())
        .cloned()
}

fn merge_display(target: &mut ListingDisplay, source: ListingDisplay) {
    if source.category.is_some() {
        target.category = source.category;
    }
    if source.icon.is_some() {
        target.icon = source.icon;
    }
    if source.display_name.is_some() {
        target.display_name = source.display_name;
    }
    if source.display_name_i18n.is_some() {
        target.display_name_i18n = source.display_name_i18n;
    }
    if source.description.is_some() {
        target.description = source.description;
    }
    if source.description_i18n.is_some() {
        target.description_i18n = source.description_i18n;
    }
}

/// `toReferenceCatalogEntry`: identifiers stay authoritative; `rootPath` is never
/// projected. `category` is only added by `referenceCatalogWithCategory`.
pub fn to_catalog_value(
    entry: &CatalogEntry,
    display: Option<&ListingDisplay>,
    include_category: bool,
) -> Value {
    let mut map = Map::new();
    if include_category {
        let category = display
            .and_then(|display| display.category.clone())
            .unwrap_or_else(|| "other".to_string());
        map.insert("category".into(), Value::String(category));
    }
    map.insert("pluginId".into(), Value::String(entry.plugin_id.clone()));
    map.insert("name".into(), Value::String(entry.name.clone()));
    map.insert(
        "marketplace".into(),
        Value::String(entry.marketplace.clone()),
    );
    if let Some(display) = display {
        if let Some(icon) = &display.icon {
            map.insert("icon".into(), Value::String(icon.clone()));
        }
        if let Some(display_name) = &display.display_name {
            map.insert("displayName".into(), Value::String(display_name.clone()));
        }
        if let Some(display_name_i18n) = &display.display_name_i18n {
            map.insert("displayNameI18n".into(), display_name_i18n.clone());
        }
        if let Some(description) = &display.description {
            map.insert("description".into(), Value::String(description.clone()));
        }
        if let Some(description_i18n) = &display.description_i18n {
            map.insert("descriptionI18n".into(), description_i18n.clone());
        }
    }
    map.insert("enabled".into(), Value::Bool(entry.enabled));
    map.insert(
        "conflictingPluginIds".into(),
        Value::Array(
            entry
                .conflicting_plugin_ids
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
    map.insert(
        "skillQualifiedNames".into(),
        Value::Array(
            entry
                .skill_qualified_names
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
    map.insert(
        "mcpServerNames".into(),
        Value::Array(
            entry
                .mcp_server_names
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
    map.insert(
        "subagentNames".into(),
        Value::Array(
            entry
                .subagent_names
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
    Value::Object(map)
}

/// Full `plugins/referenceCatalog` result for the workspace authority.
pub fn reference_catalog_value(
    overview: &OverviewData,
    plugins: &[PluginMetadata],
    include_category: bool,
) -> Value {
    let entries = build_catalog(plugins);
    let display_by_id = listing_display_by_plugin_id(overview);
    json!({
        "authority": "workspace",
        "plugins": entries
            .iter()
            .map(|entry| to_catalog_value(entry, display_by_id.get(&entry.plugin_id), include_category))
            .collect::<Vec<Value>>(),
    })
}

/// `toReferenceCatalogEntry` for skills: `glm:<scope>:<path>` keeps same-name skills from
/// different roots as distinct rows.
pub fn skill_catalog_value(skills_list: &Value) -> Value {
    let skills: Vec<Value> = skills_list
        .get("skills")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(to_skill_entry).collect())
        .unwrap_or_default();
    json!({ "authority": "workspace", "skills": skills })
}

fn to_skill_entry(skill: &Value) -> Option<Value> {
    let path = skill.get("path").and_then(Value::as_str)?;
    if path.is_empty() {
        return None;
    }
    let scope = match skill.get("scope").and_then(Value::as_str) {
        Some("plugin") => "plugin",
        Some("workspace") => "workspace",
        _ => "user",
    };
    let mut entry = Map::new();
    entry.insert("id".into(), Value::String(format!("glm:{scope}:{path}")));
    entry.insert(
        "name".into(),
        Value::String(
            skill
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
    );
    entry.insert(
        "description".into(),
        Value::String(
            skill
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
    );
    entry.insert("path".into(), Value::String(path.to_string()));
    entry.insert("scope".into(), Value::String(scope.to_string()));
    entry.insert("enabled".into(), Value::Bool(true));
    if let Some(plugin_name) = skill.get("pluginName").and_then(Value::as_str) {
        entry.insert("pluginName".into(), Value::String(plugin_name.to_string()));
    }
    Some(Value::Object(entry))
}

/// `plugin_suggested_reference_*` diagnostics are `severity: "error"` and point at the
/// stable id, exactly like the CLI handler's `diagnostic()` helper.
pub fn suggested_reference_diagnostic(stable_id: &str, code: &str, message: &str) -> Value {
    json!({
        "code": code,
        "message": message,
        "severity": "error",
        "pluginId": stable_id,
    })
}

/// Trust check for a suggested plugin id (`stableId`): last `@` split, both segments
/// `[A-Za-z0-9][A-Za-z0-9._-]*`, marketplace must be the official one
/// (`SUGGESTED_PLUGIN_MARKETPLACE` in the CLI handler).
pub fn is_trusted_suggested_id(stable_id: &str) -> bool {
    let Some(index) = stable_id.rfind('@') else {
        return false;
    };
    if index == 0 || index == stable_id.len() - 1 {
        return false;
    }
    let name = &stable_id[..index];
    let marketplace = &stable_id[index + 1..];
    marketplace == crate::types::OFFICIAL_MARKETPLACE_ID
        && is_reference_segment(name)
        && is_reference_segment(marketplace)
}

fn is_reference_segment(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

/// Split a trusted `name@marketplace` stable id.
pub fn split_suggested_id(stable_id: &str) -> (String, String) {
    let Some(index) = stable_id.rfind('@') else {
        return (String::new(), String::new());
    };
    if index == 0 {
        return (String::new(), String::new());
    }
    (
        stable_id[..index].to_string(),
        stable_id[index + 1..].to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(name: &str, id: &str, enabled: bool, components: Vec<Value>) -> PluginMetadata {
        PluginMetadata {
            id: id.into(),
            name: name.into(),
            description: None,
            version: Some("1.0.0".into()),
            enabled,
            source: "inline".into(),
            marketplace: "inline".into(),
            author: None,
            author_url: None,
            homepage: None,
            skill_count: 0,
            skill_root_count: 0,
            command_root_count: 0,
            components,
            declared_mcp_server_names: Vec::new(),
            mcp_server_names: Vec::new(),
            hook_details: Vec::new(),
            root_path: std::path::PathBuf::from("/tmp/demo"),
            user_config: None,
            configured_options: Map::new(),
            data_path: std::path::PathBuf::from("/tmp/demo-data"),
        }
    }

    #[test]
    fn catalog_projects_skills_agents_conflicts_and_mcp_names() {
        let components = vec![
            json!({"kind": "skill", "items": [{"name": " alpha "}, {"name": "beta"}]}),
            json!({"kind": "agent", "items": [{"name": "reviewer"}]}),
        ];
        let mut first = plugin("demo", "demo@inline", true, components.clone());
        first.mcp_server_names = vec!["plugin:demo:svc".into()];
        let second = plugin("demo", "demo@other", true, components);
        let disabled = plugin("demo", "demo@third", false, Vec::new());
        let entries = build_catalog(&[first, second, disabled]);
        assert_eq!(
            entries[0].skill_qualified_names,
            vec!["demo:alpha", "demo:beta"]
        );
        assert_eq!(entries[0].subagent_names, vec!["demo:reviewer"]);
        assert_eq!(entries[0].mcp_server_names, vec!["plugin:demo:svc"]);
        assert_eq!(entries[0].conflicting_plugin_ids, vec!["demo@other"]);
        assert_eq!(entries[1].conflicting_plugin_ids, vec!["demo@inline"]);
        assert!(entries[2].conflicting_plugin_ids.is_empty());
    }

    #[test]
    fn trusted_suggested_ids_require_the_official_shape() {
        assert!(is_trusted_suggested_id("demo@zcode-plugins-official"));
        assert!(is_trusted_suggested_id("a.b-c_d@zcode-plugins-official"));
        assert!(!is_trusted_suggested_id("demo"));
        assert!(!is_trusted_suggested_id("@zcode-plugins-official"));
        assert!(!is_trusted_suggested_id("demo@"));
        assert!(!is_trusted_suggested_id("de mo@zcode-plugins-official"));
        assert!(!is_trusted_suggested_id("demo@other"));
        assert_eq!(
            split_suggested_id("demo@zcode-plugins-official"),
            ("demo".to_string(), "zcode-plugins-official".to_string())
        );
    }

    #[test]
    fn skill_entries_use_the_glm_scope_path_identity() {
        let value = json!({
            "skills": [
                {"id": "x", "name": "s", "description": "d", "path": "C:/w/SKILL.md", "scope": "workspace"},
                {"id": "y", "name": "p", "description": "", "path": "C:/p/SKILL.md", "scope": "plugin", "pluginName": "demo"}
            ]
        });
        let catalog = skill_catalog_value(&value);
        assert_eq!(catalog["authority"], json!("workspace"));
        assert_eq!(
            catalog["skills"][0]["id"],
            json!("glm:workspace:C:/w/SKILL.md")
        );
        assert_eq!(catalog["skills"][0]["enabled"], json!(true));
        assert_eq!(catalog["skills"][1]["scope"], json!("plugin"));
        assert_eq!(catalog["skills"][1]["pluginName"], json!("demo"));
    }
}
