//! Dependency closure resolution for marketplace installs and validation — port of
//! `resolveDependencyClosure` / `resolveDependencyClosureFromManifest`.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use nc_code_service_core::ServiceError;

use crate::manifest::{MarketplaceEntry, MarketplaceManifest};
use crate::marketplace_store::load_marketplace_manifest;
use crate::types::{normalize_dependency_ref, parse_plugin_id, qualify_dependency};
use serde_json::Value;

struct Frame {
    plugin_id: String,
    marketplace: String,
    dependencies: Vec<String>,
    next: usize,
}

/// Async port of the recursive DFS: dependencies are pushed before their dependents
/// (post-order), cross-marketplace edges outside the allow-set fail, and a cycle reports
/// the full visiting chain.
pub async fn resolve_dependency_closure(
    storage_root: &Path,
    marketplace: &str,
    name: &str,
    allow_cross_marketplaces: &HashSet<String>,
    root_manifest: Option<&MarketplaceManifest>,
) -> Result<Vec<String>, ServiceError> {
    let root_id = format!("{name}@{marketplace}");
    let mut manifests: HashMap<String, Option<MarketplaceManifest>> = HashMap::new();
    if let Some(manifest) = root_manifest {
        manifests.insert(marketplace.to_string(), Some(manifest.clone()));
    }
    let mut visiting: Vec<String> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut closure: Vec<String> = Vec::new();

    let root = build_frame(
        &root_id,
        &root_id,
        marketplace,
        allow_cross_marketplaces,
        storage_root,
        &mut manifests,
    )
    .await?;
    visiting.push(root.plugin_id.clone());
    let mut stack: Vec<Frame> = vec![root];

    while let Some(frame) = stack.last_mut() {
        if frame.next < frame.dependencies.len() {
            let dependency =
                qualify_dependency(&frame.dependencies[frame.next], &frame.marketplace);
            frame.next += 1;
            if visiting.contains(&dependency) {
                let mut chain = visiting.clone();
                chain.push(dependency.clone());
                return Err(ServiceError::new(format!(
                    "Plugin dependency cycle: {}",
                    chain.join(" -> ")
                )));
            }
            if visited.contains(&dependency) {
                continue;
            }
            let child = build_frame(
                &dependency,
                &frame.plugin_id,
                marketplace,
                allow_cross_marketplaces,
                storage_root,
                &mut manifests,
            )
            .await?;
            visiting.push(child.plugin_id.clone());
            stack.push(child);
        } else {
            let done = stack.pop().expect("frame");
            visiting.pop();
            visited.insert(done.plugin_id.clone());
            closure.push(done.plugin_id);
        }
    }
    Ok(closure)
}

async fn build_frame(
    plugin_id: &str,
    required_by: &str,
    root_marketplace: &str,
    allow_cross_marketplaces: &HashSet<String>,
    storage_root: &Path,
    manifests: &mut HashMap<String, Option<MarketplaceManifest>>,
) -> Result<Frame, ServiceError> {
    let (name, marketplace) = parse_plugin_id(plugin_id).map_err(ServiceError::new)?;
    if marketplace != root_marketplace && !allow_cross_marketplaces.contains(&marketplace) {
        return Err(ServiceError::new(format!(
            "Cross-marketplace dependency is blocked: {plugin_id} required by {required_by}"
        )));
    }
    if !manifests.contains_key(&marketplace) {
        let manifest = load_marketplace_manifest(storage_root, &marketplace).await;
        manifests.insert(marketplace.clone(), manifest);
    }
    let manifest = manifests
        .get(&marketplace)
        .and_then(|manifest| manifest.as_ref())
        .ok_or_else(|| {
            ServiceError::new(format!(
                "Marketplace not found for dependency: {marketplace}"
            ))
        })?;
    let entry = manifest
        .plugins
        .iter()
        .find(|entry| entry.name == name)
        .ok_or_else(|| {
            ServiceError::new(format!(
                "Dependency not found: {plugin_id} required by {required_by}"
            ))
        })?;
    Ok(Frame {
        plugin_id: plugin_id.to_string(),
        marketplace,
        dependencies: normalize_dependencies(entry),
        next: 0,
    })
}

fn normalize_dependencies(entry: &MarketplaceEntry) -> Vec<String> {
    entry
        .raw
        .get("dependencies")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(normalize_dependency_ref).collect())
        .unwrap_or_else(|| entry.dependencies.clone())
}

/// `inferComponentTypes` for marketplace catalog entries.
pub fn infer_component_types(raw: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut types = Vec::new();
    for (key, kind) in [
        ("agents", "agent"),
        ("commands", "command"),
        ("skills", "skill"),
        ("hooks", "hook"),
        ("mcpServers", "mcp"),
        ("lspServers", "lsp"),
    ] {
        if raw.contains_key(key) {
            types.push(kind.to_string());
        }
    }
    types
}

/// `inferComponentTypesFromMetadata` for loaded plugins.
pub fn infer_component_types_from_metadata(
    plugin: &crate::discovery::PluginMetadata,
) -> Vec<String> {
    let mut types = Vec::new();
    let has_agent = plugin.components.iter().any(|group| {
        group.get("kind").and_then(Value::as_str) == Some("agent")
            && group
                .get("items")
                .and_then(Value::as_array)
                .is_some_and(|items| !items.is_empty())
    });
    if has_agent {
        types.push("agent".to_string());
    }
    if plugin.command_root_count > 0 {
        types.push("command".to_string());
    }
    if plugin.skill_root_count > 0 || plugin.skill_count > 0 {
        types.push("skill".to_string());
    }
    if !plugin.declared_mcp_server_names.is_empty() || !plugin.mcp_server_names.is_empty() {
        types.push("mcp".to_string());
    }
    if !plugin.hook_details.is_empty() {
        types.push("hook".to_string());
    }
    types
}
