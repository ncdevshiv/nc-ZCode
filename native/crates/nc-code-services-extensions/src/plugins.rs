//! Plugin-root discovery shared by the `commands`, `skills` and `subagents` channels.
//!
//! The TypeScript backend carries this logic three times:
//! - `commandsService.ts` / `skillsService.ts` each inline a scan of
//!   `<cli storage>/plugins/cache/zcode-plugins-official/<plugin>/<version>/` plus
//!   `installed_plugins.json` (Dirent type, no transient-name filter);
//! - `subagentsService.ts` uses `scanOfficialPluginCacheRoots` from
//!   `packages/shared/src/node/officialPluginCache.ts`, which skips backup/seed-lock/temp
//!   entries and orders version directories numerically.
//!
//! Rather than quietly unifying them (which would change what each channel shows), both
//! variants are ported as named functions and each channel calls the one its TypeScript
//! source calls.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::text::locale_en_cmp;
use crate::user_config::{absolute_lexical, read_object_file};

pub const OFFICIAL_PLUGIN_MARKETPLACE: &str = "zcode-plugins-official";
pub const INLINE_PLUGIN_MARKETPLACE: &str = "inline";
const INSTALLED_PLUGINS_FILE: &str = "installed_plugins.json";

/// `DEFAULT_ENABLED_OFFICIAL_PLUGIN_IDS` (`packages/shared/src/plugin-marketplaces.ts`).
/// Keep in step with the CLI's official plugin definitions when either side changes.
pub const DEFAULT_ENABLED_OFFICIAL_PLUGIN_IDS: [&str; 10] = [
    "browser-use@zcode-plugins-official",
    "image-search@zcode-plugins-official",
    "documents@zcode-plugins-official",
    "pdf@zcode-plugins-official",
    "presentations@zcode-plugins-official",
    "spreadsheets@zcode-plugins-official",
    "node-repl-host@zcode-plugins-official",
    "skill-creator@zcode-plugins-official",
    "plugin-creator@zcode-plugins-official",
    "zcode-guide@zcode-plugins-official",
];

pub fn is_default_enabled_official_plugin(plugin_id: &str) -> bool {
    DEFAULT_ENABLED_OFFICIAL_PLUGIN_IDS.contains(&plugin_id)
}

/// The three accepted manifest locations, in the order TypeScript probes them.
fn manifest_candidates(root: &Path) -> [PathBuf; 3] {
    [
        root.join(".nc-code-plugin").join("plugin.json"),
        root.join(".claude-plugin").join("plugin.json"),
        root.join(".codex-plugin").join("plugin.json"),
    ]
}

/// `readPluginManifest` as used by `subagentsService.ts`: first parseable object wins, no
/// name validation.
pub async fn read_plugin_manifest_raw(root: &Path) -> Option<Map<String, Value>> {
    for candidate in manifest_candidates(root) {
        let raw = tokio::fs::read_to_string(candidate).await.ok()?;
        if let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(&raw) {
            return Some(parsed);
        }
    }
    None
}

/// `readPluginManifest` as used by `commandsService.ts`/`skillsService.ts`: the plugin name
/// must match `^[a-z0-9][a-z0-9._-]{0,127}$`, otherwise the whole plugin is skipped.
pub async fn read_plugin_manifest(root: &Path) -> Option<PluginManifest> {
    let entries = read_plugin_manifest_strict_candidates(root).await?;
    let name = entries
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if !is_valid_plugin_name(name) {
        return None;
    }
    Some(PluginManifest {
        name: name.to_string(),
        entries,
    })
}

pub struct PluginManifest {
    pub name: String,
    pub entries: Map<String, Value>,
}

/// Each manifest path is only tried in order when the read fails; a present-but-invalid file
/// yields no manifest (`commandsService.ts` returns `null`, not "try the next file").
async fn read_plugin_manifest_strict_candidates(root: &Path) -> Option<Map<String, Value>> {
    for candidate in manifest_candidates(root) {
        if !tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
            continue;
        }
        let raw = tokio::fs::read_to_string(candidate).await.ok()?;
        return match serde_json::from_str::<Value>(&raw) {
            Ok(Value::Object(parsed)) => Some(parsed),
            _ => None,
        };
    }
    None
}

fn is_valid_plugin_name(name: &str) -> bool {
    let bytes: Vec<char> = name.chars().collect();
    if bytes.is_empty() || bytes.len() > 128 {
        return false;
    }
    let is_head = |value: char| value.is_ascii_lowercase() || value.is_ascii_digit();
    let is_tail = |value: char| {
        value.is_ascii_lowercase() || value.is_ascii_digit() || matches!(value, '.' | '_' | '-')
    };
    is_head(bytes[0]) && bytes[1..].iter().all(|value| is_tail(*value))
}

/// `parsePathList`: one string, or an array of strings; anything else contributes nothing.
pub fn parse_path_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(single)) => vec![single.clone()],
        other => other
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// `resolveInside`: relative paths only, and they must stay under `root`.
pub fn resolve_inside(root: &Path, raw: &str) -> Option<PathBuf> {
    let candidate = Path::new(raw);
    if candidate.is_absolute() {
        return None;
    }
    let resolved = absolute_lexical(&root.join(candidate));
    let root_lexical = absolute_lexical(root);
    let relative = pathdiff(&root_lexical, &resolved)?;
    if relative == PathBuf::new() {
        return Some(resolved);
    }
    if relative
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }
    Some(resolved)
}

/// `relative(rootPath, resolved)` for the containment checks above.
fn pathdiff(root: &Path, target: &Path) -> Option<PathBuf> {
    let root_components: Vec<_> = root.components().collect();
    let target_components: Vec<_> = target.components().collect();
    let mut shared = 0;
    while shared < root_components.len()
        && shared < target_components.len()
        && root_components[shared] == target_components[shared]
    {
        shared += 1;
    }
    let mut relative = PathBuf::new();
    for _ in shared..root_components.len() {
        relative.push("..");
    }
    for component in &target_components[shared..] {
        relative.push(component.as_os_str());
    }
    Some(relative)
}

/// The commands/skills variant: two-level scan, `lstat`-style directory check (so a symlinked
/// version directory still counts), sorted by full path with `localeCompare`.
pub async fn scan_official_plugin_cache_root_paths(plugin_storage_root: &Path) -> Vec<PathBuf> {
    let cache_root = plugin_storage_root
        .join("cache")
        .join(OFFICIAL_PLUGIN_MARKETPLACE);
    let Ok(entries) = tokio::fs::read_dir(&cache_root).await else {
        return Vec::new();
    };
    let mut entries = entries;
    let mut plugin_entries: Vec<PathBuf> = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        plugin_entries.push(entry.path());
    }
    let mut roots = Vec::new();
    for plugin_dir in plugin_entries {
        let Ok(version_entries) = tokio::fs::read_dir(&plugin_dir).await else {
            continue;
        };
        let mut version_entries = version_entries;
        while let Ok(Some(entry)) = version_entries.next_entry().await {
            let path = entry.path();
            // `lstat(path).isDirectory()` in TypeScript: follows nothing, but a directory
            // reached through a symlinked plugin directory still resolves as a directory.
            if tokio::fs::metadata(&path)
                .await
                .map(|meta| meta.is_dir())
                .unwrap_or(false)
            {
                roots.push(path);
            }
        }
    }
    roots.sort_by(|left, right| locale_en_cmp(&left.to_string_lossy(), &right.to_string_lossy()));
    roots
}

/// One entry of `installed_plugins.json` as read by commands/skills
/// (`readInstalledPluginRoots`): `id`, `marketplace` and an **absolute** `installPath` are all
/// required, and every value is trimmed.
pub struct InstalledPluginRoot {
    pub id: String,
    pub marketplace: String,
    pub install_path: PathBuf,
}

pub async fn read_installed_plugin_roots(plugin_storage_root: &Path) -> Vec<InstalledPluginRoot> {
    let entries = read_object_file(&plugin_storage_root.join(INSTALLED_PLUGINS_FILE)).await;
    let Some(plugins) = entries.get("plugins").and_then(Value::as_array) else {
        return Vec::new();
    };
    plugins
        .iter()
        .filter_map(Value::as_object)
        .filter_map(|record| {
            let id = record.get("id").and_then(Value::as_str)?.trim().to_string();
            let marketplace = record
                .get("marketplace")
                .and_then(Value::as_str)?
                .trim()
                .to_string();
            let install_path = record
                .get("installPath")
                .and_then(Value::as_str)?
                .trim()
                .to_string();
            let path = PathBuf::from(&install_path);
            if id.is_empty()
                || marketplace.is_empty()
                || install_path.is_empty()
                || !path.is_absolute()
            {
                return None;
            }
            Some(InstalledPluginRoot {
                id,
                marketplace,
                install_path: path,
            })
        })
        .collect()
}

/// One entry of `installed_plugins.json` as read by subagents
/// (`readInstalledPluginRecords`): only `id`, `installPath` and `name` are type-checked, values
/// are not trimmed and absoluteness is not required.
pub struct InstalledPluginRecord {
    pub id: String,
    pub install_path: PathBuf,
    pub name: String,
    pub scope: Option<String>,
    pub project_path: Option<String>,
}

pub async fn read_installed_plugin_records(
    plugin_storage_root: &Path,
) -> Vec<InstalledPluginRecord> {
    let path = plugin_storage_root.join(INSTALLED_PLUGINS_FILE);
    let Ok(raw) = tokio::fs::read_to_string(path).await else {
        return Vec::new();
    };
    let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    let Some(plugins) = parsed.get("plugins").and_then(Value::as_array) else {
        return Vec::new();
    };
    plugins
        .iter()
        .filter_map(Value::as_object)
        .filter_map(|record| {
            let id = record.get("id").and_then(Value::as_str)?;
            let install_path = record.get("installPath").and_then(Value::as_str)?;
            let name = record.get("name").and_then(Value::as_str)?;
            Some(InstalledPluginRecord {
                id: id.to_string(),
                install_path: PathBuf::from(install_path),
                name: name.to_string(),
                scope: record
                    .get("scope")
                    .and_then(Value::as_str)
                    .filter(|value| matches!(*value, "user" | "workspace"))
                    .map(str::to_string),
                project_path: record
                    .get("projectPath")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect()
}

/// `<plugins storage>/cache/zcode-plugins-official/<name>/<version>/` in the
/// `packages/shared/src/node/officialPluginCache.ts` shape: transient entries are skipped and
/// version directories are ordered numerically descending (newest first).
pub struct OfficialPluginCacheRoot {
    pub name: String,
    pub version_roots: Vec<PathBuf>,
}

fn is_transient_cache_entry_name(name: &str) -> bool {
    name.contains(".backup") || name.contains(".seed-lock") || name.contains(".tmp-")
}

pub async fn scan_official_plugin_cache_roots(
    plugin_storage_root: &Path,
) -> Vec<OfficialPluginCacheRoot> {
    let cache_root = plugin_storage_root
        .join("cache")
        .join(OFFICIAL_PLUGIN_MARKETPLACE);
    let Ok(mut entries) = tokio::fs::read_dir(&cache_root).await else {
        return Vec::new();
    };
    let mut plugin_names: Vec<(String, PathBuf)> = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        if !entry
            .file_type()
            .await
            .map(|kind| kind.is_dir())
            .unwrap_or(false)
        {
            continue;
        }
        plugin_names.push((
            entry.file_name().to_string_lossy().into_owned(),
            entry.path(),
        ));
    }
    let mut roots: Vec<OfficialPluginCacheRoot> = Vec::new();
    for (name, plugin_dir) in plugin_names {
        let Ok(mut versions) = tokio::fs::read_dir(&plugin_dir).await else {
            continue;
        };
        let mut names: Vec<String> = Vec::new();
        while let Ok(Some(entry)) = versions.next_entry().await {
            let version = entry.file_name().to_string_lossy().into_owned();
            if !entry
                .file_type()
                .await
                .map(|kind| kind.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            if is_transient_cache_entry_name(&version) {
                continue;
            }
            names.push(version);
        }
        // `right.localeCompare(left, undefined, { numeric: true, sensitivity: "base" })`.
        names.sort_by(|left, right| natural_cmp(right, left));
        if !names.is_empty() {
            roots.push(OfficialPluginCacheRoot {
                name,
                version_roots: names
                    .iter()
                    .map(|version| plugin_dir.join(version))
                    .collect(),
            });
        }
    }
    roots.sort_by(|left, right| locale_en_cmp(&left.name, &right.name));
    roots
}

/// Numeric-aware, case-insensitive comparison (`Intl` `numeric: true`, `sensitivity: "base"`).
fn natural_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    let left_chunks = split_numeric_chunks(left);
    let right_chunks = split_numeric_chunks(right);
    for index in 0..std::cmp::min(left_chunks.len(), right_chunks.len()) {
        let a = &left_chunks[index];
        let b = &right_chunks[index];
        match (a, b) {
            (Chunk::Number(x), Chunk::Number(y)) => {
                if x != y {
                    return x.cmp(y);
                }
            }
            (Chunk::Text(x), Chunk::Text(y)) => {
                let folded = locale_en_cmp(&x.to_lowercase(), &y.to_lowercase());
                if folded != std::cmp::Ordering::Equal {
                    return folded;
                }
            }
            (Chunk::Number(_), Chunk::Text(_)) => return std::cmp::Ordering::Less,
            (Chunk::Text(_), Chunk::Number(_)) => return std::cmp::Ordering::Greater,
        }
    }
    left_chunks.len().cmp(&right_chunks.len())
}

enum Chunk {
    Number(u128),
    Text(String),
}

fn split_numeric_chunks(value: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut digits = false;
    for ch in value.chars() {
        let is_digit = ch.is_ascii_digit();
        if !current.is_empty() && is_digit != digits {
            chunks.push(make_chunk(std::mem::take(&mut current), digits));
        }
        digits = is_digit;
        current.push(ch);
    }
    if !current.is_empty() {
        chunks.push(make_chunk(current, digits));
    }
    chunks
}

fn make_chunk(text: String, digits: bool) -> Chunk {
    if digits {
        if let Ok(value) = text.parse::<u128>() {
            return Chunk::Number(value);
        }
    }
    Chunk::Text(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_name_validation_matches_the_typescript_regex() {
        assert!(is_valid_plugin_name("pdf"));
        assert!(is_valid_plugin_name("a0._-x"));
        assert!(!is_valid_plugin_name(""));
        assert!(!is_valid_plugin_name("Pdf"));
        assert!(!is_valid_plugin_name("_pdf"));
        assert!(!is_valid_plugin_name(&"a".repeat(129)));
    }

    #[test]
    fn resolve_inside_rejects_absolute_and_escaping_paths() {
        let root = PathBuf::from("/plugins/demo");
        let resolved = resolve_inside(&root, "commands");
        // absolute_lexical appends a drive letter to the root path on Windows: assert per component, do not pin the POSIX literal.
        assert!(
            resolved
                .as_ref()
                .is_some_and(|path| path.ends_with(PathBuf::from("/plugins/demo/commands"))),
            "{resolved:?}"
        );
        assert_eq!(resolve_inside(&root, "/etc/passwd"), None);
        assert_eq!(resolve_inside(&root, "../outside"), None);
    }

    #[test]
    fn numeric_version_ordering_is_descending_and_case_insensitive() {
        let mut names = vec![
            "1.9.0".to_string(),
            "1.10.0".to_string(),
            "1.2.0".to_string(),
        ];
        names.sort_by(|left, right| natural_cmp(right, left));
        assert_eq!(names, vec!["1.10.0", "1.9.0", "1.2.0"]);
    }
}
