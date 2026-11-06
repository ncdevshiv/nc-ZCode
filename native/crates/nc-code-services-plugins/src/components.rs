//! Component enumeration for `plugins/list` and `plugins/describe` — port of
//! `adapters/src/plugins/plugin-components.ts`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::diagnostics::Diagnostic;
use crate::frontmatter::read_plugin_markdown_frontmatter;
use crate::fsutil::{is_directory, is_file, resolve_inside};
use crate::hooks::{
    inline_hook_event_names, inspect_plugin_hooks, list_plugin_hook_event_names, HookPluginContext,
};
use crate::manifest::{
    find_plugin_manifest_path, normalize_author, MarketplaceEntry, CLAUDE_MANIFEST_PATH,
    CODEX_MANIFEST_PATH, NCCODE_MANIFEST_PATH, ZCODE_MANIFEST_PATH,
};
use crate::mcp::{load_plugin_mcp_server_definitions, McpPluginContext};
use crate::types::{is_valid_namespaced_name, DEFAULT_VERSION};

pub const SKILL_FILE_NAME: &str = "SKILL.md";
const EXCLUDED_DIRECTORIES: [&str; 12] = [
    "node_modules",
    "dist",
    "build",
    "out",
    "target",
    "vendor",
    "coverage",
    ".cache",
    ".next",
    ".turbo",
    ".venv",
    "__pycache__",
];

pub struct ComponentContext<'a> {
    pub root_path: &'a Path,
    pub manifest: Option<&'a Map<String, Value>>,
    pub manifest_path: &'a Path,
    pub plugin_id: &'a str,
    pub marketplace: &'a str,
}

fn hook_context<'a>(ctx: &'a ComponentContext<'a>) -> Option<HookPluginContext<'a>> {
    Some(HookPluginContext {
        root_path: ctx.root_path,
        manifest: ctx.manifest?,
        manifest_path: ctx.manifest_path,
        plugin_id: ctx.plugin_id,
    })
}

fn mcp_context<'a>(ctx: &'a ComponentContext<'a>) -> Option<McpPluginContext<'a>> {
    Some(McpPluginContext {
        root_path: ctx.root_path,
        manifest: Some(ctx.manifest?),
        manifest_path: ctx.manifest_path,
        plugin_id: ctx.plugin_id,
        marketplace: ctx.marketplace,
    })
}

/// `enumeratePluginComponents`: group order agent / command / skill / hook / mcp.
pub fn enumerate_plugin_components(
    ctx: &ComponentContext<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<Value> {
    let mut groups = Vec::new();
    let agents = collect_markdown_components(
        ctx.root_path,
        ctx.manifest.and_then(|manifest| manifest.get("agents")),
        "agents",
    );
    if !agents.is_empty() {
        groups.push(json!({"kind": "agent", "items": agents}));
    }
    let commands = collect_markdown_components(
        ctx.root_path,
        ctx.manifest.and_then(|manifest| manifest.get("commands")),
        "commands",
    );
    if !commands.is_empty() {
        groups.push(json!({"kind": "command", "items": commands}));
    }
    let skills = collect_skill_components(
        ctx.root_path,
        ctx.manifest.and_then(|manifest| manifest.get("skills")),
    );
    if !skills.is_empty() {
        groups.push(json!({"kind": "skill", "items": skills}));
    }
    let hook_events = match hook_context(ctx) {
        Some(hook_ctx) => list_plugin_hook_event_names(&hook_ctx, diagnostics),
        None => Vec::new(),
    };
    if !hook_events.is_empty() {
        groups.push(json!({
            "kind": "hook",
            "items": hook_events.into_iter().map(|name| json!({"name": name})).collect::<Vec<_>>(),
        }));
    }
    let definitions = match mcp_context(ctx) {
        Some(mcp_ctx) => load_plugin_mcp_server_definitions(&mcp_ctx, diagnostics),
        None => Map::new(),
    };
    let mut mcp_names: Vec<String> = definitions
        .keys()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect();
    mcp_names.sort();
    if !mcp_names.is_empty() {
        groups.push(json!({
            "kind": "mcp",
            "items": mcp_names.into_iter().map(|name| json!({"name": name})).collect::<Vec<_>>(),
        }));
    }
    groups
}

fn collect_markdown_components(
    root_path: &Path,
    manifest_field: Option<&Value>,
    default_dir: &str,
) -> Vec<Value> {
    let mut items = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if let Some(record) = manifest_field.and_then(Value::as_object) {
        for (raw_name, raw_meta) in record {
            let name = raw_name.trim().to_string();
            if name.is_empty() || seen.contains(&name) {
                continue;
            }
            seen.insert(name.clone());
            let description = raw_meta
                .get("description")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|description| !description.is_empty())
                .map(str::to_string);
            items.push(match description {
                Some(description) => json!({"name": name, "description": description}),
                None => json!({"name": name}),
            });
        }
    }
    for dir in collect_component_dirs(root_path, manifest_field, default_dir) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut file_names: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if is_file(&path) && path.extension().and_then(|value| value.to_str()) == Some("md") {
                file_names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        file_names.sort();
        for file_name in file_names {
            let base_name = file_name.trim_end_matches(".md").to_string();
            let (frontmatter_name, description) =
                read_plugin_markdown_frontmatter(&dir.join(&file_name));
            let name = frontmatter_name.unwrap_or(base_name);
            if seen.contains(&name) {
                continue;
            }
            seen.insert(name.clone());
            items.push(match description {
                Some(description) => json!({"name": name, "description": description}),
                None => json!({"name": name}),
            });
        }
    }
    items
}

fn collect_skill_components(root_path: &Path, manifest_field: Option<&Value>) -> Vec<Value> {
    let mut items = Vec::new();
    let mut seen_files: HashSet<String> = HashSet::new();
    let mut seen_names: HashSet<String> = HashSet::new();
    for dir in collect_component_dirs(root_path, manifest_field, "skills") {
        for skill_file in scan_skill_files_under_root(&dir) {
            let key = skill_file.to_string_lossy().into_owned();
            if !seen_files.insert(key) {
                continue;
            }
            let fallback_name = skill_file
                .parent()
                .and_then(|parent| parent.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let (frontmatter_name, description) = read_plugin_markdown_frontmatter(&skill_file);
            let name = frontmatter_name.unwrap_or(fallback_name);
            if name.is_empty() || !seen_names.insert(name.clone()) {
                continue;
            }
            items.push(match description {
                Some(description) => json!({"name": name, "description": description}),
                None => json!({"name": name}),
            });
        }
    }
    items
}

fn collect_component_dirs(
    root_path: &Path,
    manifest_field: Option<&Value>,
    default_dir: &str,
) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut add = |raw: &str, dirs: &mut Vec<PathBuf>| {
        let Some(resolved) = resolve_inside(root_path, raw.trim_start_matches("./")) else {
            return;
        };
        let key = resolved.to_string_lossy().into_owned();
        if seen.insert(key) {
            dirs.push(resolved);
        }
    };
    let default_path = root_path.join(default_dir);
    if is_directory(&default_path) {
        add(default_dir, &mut dirs);
    }
    match manifest_field {
        Some(Value::String(single)) => add(single, &mut dirs),
        Some(Value::Array(items)) => {
            for item in items.iter().filter_map(Value::as_str) {
                add(item, &mut dirs);
            }
        }
        _ => {}
    }
    dirs
}

/// `scanSkillFilesUnderRootSync` with `followSymbolicLinks: false`: the root itself and
/// one level of subdirectories, never following symlinks (plugin content is untrusted).
pub fn scan_skill_files_under_root(root_path: &Path) -> Vec<PathBuf> {
    if std::fs::symlink_metadata(root_path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Vec::new();
    }
    let Ok(metadata) = std::fs::metadata(root_path) else {
        return Vec::new();
    };
    if !metadata.is_dir() {
        return Vec::new();
    }
    let mut files = Vec::new();
    let own = root_path.join(SKILL_FILE_NAME);
    if is_real_file(&own) {
        files.push(own);
    }
    let Ok(entries) = std::fs::read_dir(root_path) else {
        return files;
    };
    for entry in entries.flatten() {
        if !should_walk_skill_directory_entry(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        if !is_dir {
            continue;
        }
        let candidate = entry.path().join(SKILL_FILE_NAME);
        if is_real_file(&candidate) {
            files.push(candidate);
        }
    }
    files
}

fn should_walk_skill_directory_entry(name: &str) -> bool {
    if EXCLUDED_DIRECTORIES.contains(&name) {
        return false;
    }
    if !name.starts_with('.') {
        return true;
    }
    name == ".system"
}

fn is_real_file(path: &Path) -> bool {
    if std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return false;
    }
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
}

/// Count unique `SKILL.md` files below the resolved skill roots (enabled plugins only).
pub fn count_skill_files(roots: &[PathBuf]) -> usize {
    let mut seen: HashSet<String> = HashSet::new();
    for root in roots {
        for file in scan_skill_files_under_root(root) {
            seen.insert(file.to_string_lossy().into_owned());
        }
    }
    seen.len()
}

/// `resolveComponentRoots` for `skills` / `commands` (default dir first).
pub fn resolve_component_roots(root_path: &Path, manifest_field: Option<&Value>) -> Vec<PathBuf> {
    collect_component_dirs(root_path, manifest_field, "skills")
}

pub fn resolve_command_roots(root_path: &Path, manifest_field: Option<&Value>) -> Vec<PathBuf> {
    collect_component_dirs(root_path, manifest_field, "commands")
}

/// Parsed manifest plus the file it came from (`None` for a synthetic strict:false entry).
pub type ReadManifest = (Map<String, Value>, Option<PathBuf>);

/// `readPluginManifestFromRoot`: strict name validation; `strict: false` entries fall
/// back to a synthetic manifest built from the marketplace entry.
pub fn read_manifest_from_root(
    root_path: &Path,
    entry: Option<&MarketplaceEntry>,
) -> Result<Option<ReadManifest>, String> {
    if let Some(manifest_path) = find_plugin_manifest_path(root_path) {
        let text = std::fs::read_to_string(&manifest_path)
            .map_err(|error| format!("Failed to read plugin manifest: {error}"))?;
        let parsed: Value = serde_json::from_str(&text)
            .map_err(|error| format!("Failed to read plugin manifest: {error}"))?;
        let Some(mut manifest) = parsed.as_object().cloned() else {
            return Err("Plugin manifest must be a JSON object".to_string());
        };
        let name = manifest
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if !is_valid_namespaced_name(&name) {
            return Err(format!("Invalid plugin name: {name}"));
        }
        manifest.insert("name".into(), Value::String(name));
        if !manifest.get("version").is_some_and(Value::is_string) {
            manifest.insert("version".into(), Value::String(DEFAULT_VERSION.into()));
        }
        return Ok(Some((manifest, Some(manifest_path))));
    }
    if entry.is_some_and(|entry| !entry.strict) {
        let entry = entry.expect("checked above");
        return Ok(Some((create_manifest_from_marketplace_entry(entry), None)));
    }
    Ok(None)
}

/// `createManifestFromMarketplaceEntry`: strip marketplace-only display/installation keys.
pub fn create_manifest_from_marketplace_entry(entry: &MarketplaceEntry) -> Map<String, Value> {
    let mut manifest = entry.raw.clone();
    for key in [
        "source",
        "category",
        "tags",
        "strict",
        "displayName",
        "displayName_i18n",
        "description_i18n",
        "icon",
        "privacyPolicy",
        "termsOfService",
        "heroImage",
        "examplePrompts",
        "examplePrompts_i18n",
        "requiresPaidPlan",
    ] {
        manifest.remove(key);
    }
    manifest.insert("name".into(), Value::String(entry.name.clone()));
    manifest.insert(
        "version".into(),
        Value::String(
            entry
                .version
                .clone()
                .unwrap_or_else(|| DEFAULT_VERSION.into()),
        ),
    );
    manifest
}

/// `toManifestDisplayMetadata` (`describe` metadata projection).
pub fn manifest_display_metadata(manifest: &Map<String, Value>) -> Option<Value> {
    let author = normalize_author(manifest.get("author"));
    let homepage = manifest
        .get("homepage")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|homepage| !homepage.is_empty())
        .map(str::to_string);
    let mut metadata = Map::new();
    if let Some(name) = author
        .as_ref()
        .and_then(|author| author.get("name"))
        .and_then(Value::as_str)
    {
        metadata.insert("author".into(), Value::String(name.to_string()));
    }
    if let Some(url) = author
        .as_ref()
        .and_then(|author| author.get("url"))
        .and_then(Value::as_str)
    {
        metadata.insert("authorUrl".into(), Value::String(url.to_string()));
    }
    if let Some(homepage) = homepage {
        metadata.insert("homepage".into(), Value::String(homepage));
    }
    if let Some(version) = manifest.get("version").and_then(Value::as_str) {
        metadata.insert("version".into(), Value::String(version.to_string()));
    }
    (!metadata.is_empty()).then_some(Value::Object(metadata))
}

/// `readComponentsAtRoot`: manifest read failure degrades to the default-directory scan.
pub fn read_components_at_root(
    root_path: &Path,
    entry: Option<&MarketplaceEntry>,
    marketplace: &str,
    plugin_id: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> (Vec<Value>, Option<Value>) {
    let loaded = read_manifest_from_root(root_path, entry).ok().flatten();
    let (manifest, manifest_path) = match &loaded {
        Some((manifest, manifest_path)) => (
            Some(manifest.clone()),
            manifest_path
                .clone()
                .unwrap_or_else(|| root_path.to_path_buf()),
        ),
        None => (None, root_path.to_path_buf()),
    };
    let ctx = ComponentContext {
        root_path,
        manifest: manifest.as_ref(),
        manifest_path: &manifest_path,
        plugin_id,
        marketplace,
    };
    let components = enumerate_plugin_components(&ctx, diagnostics);
    let metadata = manifest.as_ref().and_then(manifest_display_metadata);
    (components, metadata)
}

/// Manifest scanner used by discovery: first existing manifest path wins
/// (`findManifest`), then strict parse.
pub fn find_manifest(root_path: &Path) -> Option<PathBuf> {
    for candidate in [
        ZCODE_MANIFEST_PATH,
        NCCODE_MANIFEST_PATH,
        CLAUDE_MANIFEST_PATH,
        CODEX_MANIFEST_PATH,
    ] {
        let path = root_path.join(candidate);
        if is_file(&path) {
            return Some(path);
        }
    }
    None
}

/// Hook inspection used by discovery; kept here so `components` owns the projection.
pub fn inspect_hooks_for_discovery(
    ctx: &ComponentContext<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> (Vec<Value>, Map<String, Value>) {
    match hook_context(ctx) {
        Some(hook_ctx) => inspect_plugin_hooks(&hook_ctx, diagnostics),
        None => (Vec::new(), Map::new()),
    }
}

/// Manifest-only hook events (`collectHookComponents` fallback when manifest is null).
pub fn manifest_hook_events(manifest: Option<&Map<String, Value>>) -> Vec<String> {
    manifest
        .and_then(|manifest| manifest.get("hooks"))
        .map(inline_hook_event_names)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_plugin(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "zcode-plugin-components-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("plugin root");
        root
    }

    #[test]
    fn markdown_skill_and_mcp_components_are_enumerated() {
        let root = temp_plugin("enum");
        std::fs::create_dir_all(root.join("commands")).unwrap();
        std::fs::write(
            root.join("commands/deploy.md"),
            "---\nname: deploy\ndescription: Ship it\n---\nbody",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("skills/reviewer")).unwrap();
        std::fs::write(
            root.join("skills/reviewer/SKILL.md"),
            "---\nname: reviewer\ndescription: >\n  Reviews\n  diffs\n---\n",
        )
        .unwrap();
        let mut manifest = Map::new();
        manifest.insert("name".into(), json!("demo"));
        manifest.insert(
            "mcpServers".into(),
            json!({"docs": {"type": "stdio", "command": "node"}}),
        );
        manifest.insert("hooks".into(), json!({"PreToolUse": []}));
        let manifest_path = root.join(".zcode-plugin/plugin.json");
        let ctx = ComponentContext {
            root_path: &root,
            manifest: Some(&manifest),
            manifest_path: &manifest_path,
            plugin_id: "demo@mp",
            marketplace: "mp",
        };
        let mut diagnostics = Vec::new();
        let groups = enumerate_plugin_components(&ctx, &mut diagnostics);
        let kinds: Vec<&str> = groups
            .iter()
            .map(|group| group["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, vec!["command", "skill", "hook", "mcp"]);
        assert_eq!(groups[1]["items"][0]["name"], json!("reviewer"));
        assert_eq!(groups[1]["items"][0]["description"], json!("Reviews diffs"));
        // Component enumeration only projects event names (TS `listPluginHookEventNames`); matcher checksum
        // `plugin_hook_invalid` diagnoses `inspectPluginHooks` that belong to the same discovery link.
        assert!(!diagnostics.iter().any(|d| d.code == "plugin_hook_invalid"));
        let (hook_details, hook_events) = inspect_hooks_for_discovery(&ctx, &mut diagnostics);
        assert!(hook_details.is_empty());
        assert!(hook_events.is_empty());
        assert!(diagnostics.iter().any(|d| d.code == "plugin_hook_invalid"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn strict_false_entry_synthesizes_manifest() {
        let root = temp_plugin("synthetic");
        let mut raw = Map::new();
        raw.insert("name".into(), json!("demo"));
        raw.insert("strict".into(), json!(false));
        raw.insert("source".into(), json!("./demo"));
        let entry = MarketplaceEntry {
            name: "demo".into(),
            category: None,
            description: None,
            version: Some("1.2.3".into()),
            source: Some(json!("./demo")),
            cache_path: None,
            dependencies: Vec::new(),
            strict: false,
            tags: Vec::new(),
            listing: None,
            raw,
        };
        let manifest = read_manifest_from_root(&root, Some(&entry))
            .expect("read")
            .expect("synthetic");
        assert_eq!(manifest.0["name"], json!("demo"));
        assert_eq!(manifest.0["version"], json!("1.2.3"));
        assert!(manifest.0.get("source").is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}
