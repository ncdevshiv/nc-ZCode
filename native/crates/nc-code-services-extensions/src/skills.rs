//! `skills` channel — skill discovery, enable flags and prompt activation.
//!
//! Port of `packages/services/src/skills/skillsService.ts` (plus the shared bounded directory
//! walk of `packages/shared/src/skill-scan-policy.ts` and `skillDiscoveryWalk.ts`).
//!
//! Two things are load-bearing and preserved exactly:
//! - **Enable flags live in `~/.zcode/cli/config.json` under `skills["<SKILL.md path>"].enable`**
//!   (`writeSkillEnabledMap`). Enabled is the default and is *not* written; disabling writes
//!   `{ enable: false }`; an empty `skills` section is removed instead of left as `{}`.
//! - **The scan policy is shared with the agent runtime**: excluded content directories,
//!   dot-directories except `.system`, depth 8, symlink directories deduped by realpath. A second
//!   copy with different rules would reproduce the recorded defect where the model can execute a
//!   skill the settings page cannot list.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::{AppPaths, ServiceArgs, ServiceError};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::plugins::{
    is_default_enabled_official_plugin, parse_path_list, read_installed_plugin_roots,
    read_plugin_manifest, resolve_inside, scan_official_plugin_cache_root_paths,
    INLINE_PLUGIN_MARKETPLACE, OFFICIAL_PLUGIN_MARKETPLACE,
};
use crate::text::locale_en_cmp;
use crate::user_config::{
    read_boolean_record, read_cli_config, read_storage_dir, read_string_array, resolve_config_path,
    resolve_plugin_storage_root,
};

const SKILL_FILE_NAME: &str = "SKILL.md";
const SKILL_META_FILE_NAME: &str = "_meta.json";
const MAX_SKILL_SCAN_DEPTH: usize = 8;
/// `SKILL_SCAN_EXCLUDED_DIRECTORY_NAMES`.
const EXCLUDED_DIRECTORY_NAMES: [&str; 12] = [
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
/// `SKILL_DISCOVERY_DOT_DIR_ALLOWLIST`.
const DOT_DIR_ALLOWLIST: [&str; 1] = [".system"];
/// `apps/nc-code-cli/packages/adapters/src/skills/index.ts:19`.
const MAX_DESCRIPTION_LENGTH: usize = 1024;
const PROVIDER: &str = "glm";
const GIT_MARKER: &str = ".git";

pub struct SkillsService {
    paths: AppPaths,
    is_desktop_runtime: Option<bool>,
}

impl SkillsService {
    pub fn new(paths: AppPaths) -> Self {
        // `createLocalServices` registers this channel with `isDesktopRuntime: true`; the native
        // host is that same trusted desktop process, so the user scope is available here.
        Self {
            paths,
            is_desktop_runtime: Some(true),
        }
    }

    /// Embeddings that are not the desktop runtime can restore the TypeScript rule
    /// (`None` = decide from `nc_code_PROCESS_LABEL`), which reports
    /// `userScopeReason: "desktop_only"` and skips user-scope discovery.
    pub fn with_is_desktop_runtime(paths: AppPaths, is_desktop_runtime: Option<bool>) -> Self {
        Self {
            paths,
            is_desktop_runtime,
        }
    }

    /// `resolveCapabilities`: the desktop/native host owns the user scope; a non-desktop
    /// process reports `desktop_only` exactly like the TypeScript service does when
    /// `nc_code_PROCESS_LABEL` is unset.
    pub fn capability(&self) -> (bool, Option<&'static str>) {
        let is_desktop = self.is_desktop_runtime.unwrap_or_else(|| {
            std::env::var("ZCODE_PROCESS_LABEL")
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
        });
        if is_desktop {
            (true, None)
        } else {
            (false, Some("desktop_only"))
        }
    }

    fn workspace_nc_code_root(&self, workspace_path: &Path) -> PathBuf {
        workspace_path.join(".zcode").join("skills")
    }

    fn user_nc_code_root(&self) -> PathBuf {
        self.paths.home_dir().join(".zcode").join("skills")
    }

    fn user_agents_root(&self) -> PathBuf {
        self.paths.home_dir().join(".agents").join("skills")
    }

    // ------------------------------------------------------------------ discovery

    async fn discover(&self, params: &DiscoverParams) -> DiscoverResult {
        let mut roots: Vec<RootDescriptor> = Vec::new();
        for root_path in
            dedupe_preserving_order(ancestor_workspace_roots(&params.workspace_path).await)
        {
            roots.push(RootDescriptor {
                scope: "workspace",
                root_path,
                plugin_name: None,
                plugin_id: None,
            });
        }
        if params.include_user_skills {
            // User skills are global resources; keeping both user roots (instead of letting
            // `.zcode` truncate `.agents`) preserves external-agent global skills.
            roots.push(RootDescriptor {
                scope: "user",
                root_path: self.user_nc_code_root(),
                plugin_name: None,
                plugin_id: None,
            });
            roots.push(RootDescriptor {
                scope: "user",
                root_path: self.user_agents_root(),
                plugin_name: None,
                plugin_id: None,
            });
        }
        roots.extend(self.resolve_plugin_skill_root_descriptors().await);

        let mut diagnostics: Vec<Value> = Vec::new();
        let mut skills: Vec<Value> = Vec::new();
        let mut seen_skill_paths: HashSet<String> = HashSet::new();
        let user_nc_code_name_keys = if params.include_user_skills {
            collect_skill_name_keys_in_root(&self.user_nc_code_root()).await
        } else {
            HashSet::new()
        };

        let scan_root_paths = dedupe_scan_roots_by_realpath(
            &roots
                .iter()
                .map(|root| root.root_path.clone())
                .collect::<Vec<_>>(),
        )
        .await;
        // `new Map(roots.map(...))` keeps the LAST descriptor for a duplicated path.
        let mut root_by_path: HashMap<PathBuf, usize> = HashMap::new();
        for (index, root) in roots.iter().enumerate() {
            root_by_path.insert(root.root_path.clone(), index);
        }

        for root_path in scan_root_paths {
            let Some(index) = root_by_path.get(&root_path) else {
                continue;
            };
            let root = &roots[*index];
            if !path_exists(&root.root_path).await {
                continue;
            }
            let skill_paths = collect_skill_markdown_paths(&root.root_path, &mut diagnostics).await;
            for skill_path in skill_paths {
                if root.scope == "user"
                    && root.root_path == self.user_agents_root()
                    && is_user_agents_skill_covered_by_zcode(
                        &skill_path,
                        &self.user_nc_code_root(),
                        &user_nc_code_name_keys,
                    )
                    .await
                {
                    continue;
                }
                let canonical = realpath(&skill_path).await;
                let canonical_text = canonical.to_string_lossy().into_owned();
                if !seen_skill_paths.insert(canonical_text.clone()) {
                    continue;
                }
                if !path_exists(&skill_path).await {
                    continue;
                }
                let markdown = match tokio::fs::read_to_string(&skill_path).await {
                    Ok(markdown) => markdown,
                    Err(error) => {
                        diagnostics.push(diagnostic(
                            "skill_read_failed",
                            "warning",
                            error.to_string(),
                            Some(skill_path.to_string_lossy().into_owned()),
                            None,
                        ));
                        continue;
                    }
                };
                let parsed = read_frontmatter(&markdown);
                let skill_folder_name =
                    skill_path.parent().map(parent_dir_name).unwrap_or_default();
                // A hand-written skill without frontmatter falls back to the folder name; only a
                // genuinely nameless skill is an error.
                let raw_name = if parsed.has_frontmatter {
                    parsed.name.trim().to_string()
                } else {
                    skill_folder_name.clone()
                };
                let resolved_name = if raw_name.is_empty() {
                    skill_folder_name.clone()
                } else {
                    raw_name
                };
                if resolved_name.is_empty() {
                    diagnostics.push(diagnostic(
                        "skill_missing_name",
                        "error",
                        format!(
                            "Skill frontmatter must include a name: {}",
                            skill_path.display()
                        ),
                        Some(skill_path.to_string_lossy().into_owned()),
                        None,
                    ));
                    continue;
                }
                let description = if parsed.has_frontmatter {
                    parsed.description.trim().to_string()
                } else {
                    String::new()
                };
                if description.chars().count() > MAX_DESCRIPTION_LENGTH {
                    diagnostics.push(diagnostic(
                        "skill_description_too_long",
                        "error",
                        format!(
                            "Skill description is too long (>{}): {resolved_name}",
                            MAX_DESCRIPTION_LENGTH
                        ),
                        Some(skill_path.to_string_lossy().into_owned()),
                        Some(resolved_name),
                    ));
                    continue;
                }
                let metadata = read_skill_metadata(&skill_path).await;
                let mut skill = Map::new();
                skill.insert(
                    "id".to_string(),
                    json!(build_skill_id(root.scope, &resolved_name, &canonical_text)),
                );
                skill.insert("name".to_string(), json!(resolved_name));
                skill.insert("description".to_string(), json!(description));
                skill.insert("body".to_string(), json!(parsed.body.trim()));
                // `path` is the SKILL.md file (not its folder) so mentions, logs and jumps all
                // locate the same artifact; `sourcePath` keeps the pre-realpath scan hit, which
                // is what a delete must target for an imported symlink.
                skill.insert("path".to_string(), json!(canonical_text));
                skill.insert(
                    "sourcePath".to_string(),
                    json!(skill_path.to_string_lossy()),
                );
                skill.insert("scope".to_string(), json!(root.scope));
                skill.insert("enabled".to_string(), json!(true));
                if let Some(plugin_name) = &root.plugin_name {
                    skill.insert("pluginName".to_string(), json!(plugin_name));
                }
                if let Some(plugin_id) = &root.plugin_id {
                    skill.insert("pluginId".to_string(), json!(plugin_id));
                }
                if let Some(metadata) = metadata {
                    skill.insert("metadata".to_string(), metadata);
                }
                skills.push(Value::Object(skill));
            }
        }

        skills.sort_by(|left, right| {
            let by_name = locale_en_cmp(
                left["name"].as_str().unwrap_or_default(),
                right["name"].as_str().unwrap_or_default(),
            );
            if by_name != std::cmp::Ordering::Equal {
                return by_name;
            }
            let by_scope = locale_en_cmp(
                left["scope"].as_str().unwrap_or_default(),
                right["scope"].as_str().unwrap_or_default(),
            );
            if by_scope != std::cmp::Ordering::Equal {
                return by_scope;
            }
            locale_en_cmp(
                left["path"].as_str().unwrap_or_default(),
                right["path"].as_str().unwrap_or_default(),
            )
        });
        DiscoverResult {
            skills,
            diagnostics,
        }
    }

    /// `resolvePluginSkillRootDescriptors`
    async fn resolve_plugin_skill_root_descriptors(&self) -> Vec<RootDescriptor> {
        let config = read_cli_config(&self.paths).await;
        let plugins = config
            .get("plugins")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if let Some(Value::Bool(false)) = plugins.get("enabled") {
            return Vec::new();
        }
        let enabled_plugins = read_boolean_record(plugins.get("enabledPlugins"));
        let suppressed_builtins = read_string_array(plugins.get("suppressedBuiltins"));
        let plugin_storage_root = resolve_plugin_storage_root(
            &self.paths,
            &resolve_config_path(&self.paths, &read_storage_dir(&config)),
        );

        let mut candidates: Vec<PluginCandidate> = Vec::new();
        for dir in read_string_array(plugins.get("dirs")) {
            candidates.push(PluginCandidate {
                default_enabled: true,
                marketplace: INLINE_PLUGIN_MARKETPLACE.to_string(),
                root_path: resolve_config_path(&self.paths, &dir),
            });
        }
        for root_path in scan_official_plugin_cache_root_paths(&plugin_storage_root).await {
            candidates.push(PluginCandidate {
                default_enabled: false,
                marketplace: OFFICIAL_PLUGIN_MARKETPLACE.to_string(),
                root_path,
            });
        }
        for record in read_installed_plugin_roots(&plugin_storage_root).await {
            candidates.push(PluginCandidate {
                default_enabled: false,
                marketplace: record.marketplace,
                root_path: record.install_path,
            });
        }

        let mut descriptors = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for candidate in candidates {
            let Some(manifest) = read_plugin_manifest(&candidate.root_path).await else {
                continue;
            };
            let plugin_id = format!("{}@{}", manifest.name, candidate.marketplace);
            if candidate.marketplace == OFFICIAL_PLUGIN_MARKETPLACE
                && suppressed_builtins.contains(&plugin_id)
            {
                continue;
            }
            if !seen.insert(plugin_id.clone()) {
                continue;
            }
            let default_enabled =
                candidate.default_enabled || is_default_enabled_official_plugin(&plugin_id);
            let enabled = match enabled_plugins.get(&plugin_id).and_then(Value::as_bool) {
                Some(value) => value,
                None => default_enabled,
            };
            if !enabled {
                continue;
            }
            for root_path in resolve_plugin_skill_roots(&manifest, &candidate.root_path) {
                descriptors.push(RootDescriptor {
                    scope: "plugin",
                    root_path,
                    plugin_name: Some(manifest.name.clone()),
                    plugin_id: Some(plugin_id.clone()),
                });
            }
        }
        descriptors
    }

    // ------------------------------------------------------------------ enable flags

    /// `readSkillEnabledMap`: normalized path -> enable flag.
    async fn read_skill_enabled_map(&self) -> HashMap<String, bool> {
        read_skill_enabled_map_from_config(&read_cli_config(&self.paths).await)
    }

    /// `writeSkillEnabledMap`: merge into the existing `skills` section, sort keys, and drop the
    /// section when it ends up empty. Enabled entries delete their override instead of writing
    /// `{ enable: true }`, so the skill follows plugin/default configuration again.
    async fn write_skill_enabled_map(
        &self,
        next: &HashMap<String, bool>,
    ) -> Result<(), ServiceError> {
        let mut config = read_cli_config(&self.paths).await;
        let mut skills_config = config
            .get("skills")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut entries: Vec<(String, bool)> = next
            .iter()
            .map(|(path, enable)| (normalize_skill_config_path(path), *enable))
            .collect();
        entries.sort_by(|left, right| locale_en_cmp(&left.0, &right.0));
        for (path, enable) in entries {
            if enable {
                skills_config.remove(&path);
            } else {
                let mut entry = Map::new();
                entry.insert("enable".to_string(), json!(false));
                skills_config.insert(path, Value::Object(entry));
            }
        }
        if skills_config.is_empty() {
            config.remove("skills");
        } else {
            config.insert("skills".to_string(), Value::Object(skills_config));
        }
        crate::user_config::write_cli_config(&self.paths, &config).await
    }

    async fn list(&self, params: &ListParams) -> Result<Value, ServiceError> {
        let (available, reason) = self.capability();
        let discovered = self
            .discover(&DiscoverParams {
                workspace_path: PathBuf::from(&params.workspace_path),
                include_user_skills: available,
            })
            .await;
        let enabled_by_path = self.read_skill_enabled_map().await;
        let skills: Vec<Value> = discovered
            .skills
            .into_iter()
            .map(|skill| {
                let mut skill = skill;
                let path = skill["path"].as_str().unwrap_or_default().to_string();
                let enabled = enabled_by_path
                    .get(&normalize_skill_config_path(&path))
                    .copied()
                    .unwrap_or(true);
                skill
                    .as_object_mut()
                    .expect("skill object")
                    .insert("enabled".to_string(), json!(enabled));
                skill
            })
            .collect();
        let mut capability = Map::new();
        capability.insert("userScopeAvailable".to_string(), json!(available));
        if let Some(reason) = reason {
            capability.insert("userScopeReason".to_string(), json!(reason));
        }
        Ok(json!({
            "skills": skills,
            "capability": Value::Object(capability),
            "diagnostics": discovered.diagnostics,
        }))
    }

    async fn find_skill(&self, params: &ListParams, skill_id: &str) -> Result<Value, ServiceError> {
        let listed = self.list(params).await?;
        listed["skills"]
            .as_array()
            .and_then(|skills| {
                skills
                    .iter()
                    .find(|skill| skill["id"].as_str() == Some(skill_id))
                    .cloned()
            })
            .ok_or_else(|| ServiceError::new(format!("Skill not found: {skill_id}")))
    }

    async fn set_enabled(&self, params: &EnabledParams) -> Result<(), ServiceError> {
        let (available, _) = self.capability();
        let discovered = self
            .discover(&DiscoverParams {
                workspace_path: PathBuf::from(&params.workspace_path),
                include_user_skills: available,
            })
            .await;
        let skill = discovered
            .skills
            .iter()
            .find(|skill| skill["id"].as_str() == Some(params.skill_id.as_str()))
            .ok_or_else(|| ServiceError::new(format!("Skill not found: {}", params.skill_id)))?;
        let path = skill["path"].as_str().unwrap_or_default().to_string();
        let mut enabled_by_path = self.read_skill_enabled_map().await;
        enabled_by_path.insert(normalize_skill_config_path(&path), params.enabled);
        self.write_skill_enabled_map(&enabled_by_path).await
    }

    async fn build_prompt_context(&self, params: &PromptParams) -> Result<Value, ServiceError> {
        let mentioned = collect_mentioned_skill_names(&params.prompt);
        if mentioned.is_empty() {
            return Ok(json!({
                "prompt": params.prompt,
                "activatedSkillNames": Vec::<String>::new(),
            }));
        }
        let listed = self
            .list(&ListParams {
                workspace_path: params.workspace_path.clone(),
                workspace_identity: params.workspace_identity.clone(),
            })
            .await?;
        let activated: Vec<&Value> = listed["skills"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .filter(|skill| {
                skill["enabled"].as_bool().unwrap_or(false)
                    && mentioned.contains(skill["name"].as_str().unwrap_or_default())
            })
            .collect();
        if activated.is_empty() {
            return Ok(json!({
                "prompt": params.prompt,
                "activatedSkillNames": Vec::<String>::new(),
            }));
        }
        let activated_names: Vec<String> = activated
            .iter()
            .map(|skill| skill["name"].as_str().unwrap_or_default().to_string())
            .collect();
        // Skills are a visible catalogue, not an unconditional context payload: only explicitly
        // mentioned, enabled skills enter the prompt, and every activation is audited.
        append_skills_audit_log(
            &self.paths,
            &params.workspace_path,
            params.workspace_identity.as_deref(),
            &activated_names,
        )
        .await?;
        let mut block = String::from("<available_skills>");
        for skill in &activated {
            block.push_str(&format!(
                "\n<activated_skill name=\"{}\" path=\"{}\">\n{}\n</activated_skill>",
                escape_xml_attribute(skill["name"].as_str().unwrap_or_default()),
                escape_xml_attribute(skill["path"].as_str().unwrap_or_default()),
                skill["body"].as_str().unwrap_or_default()
            ));
        }
        block.push_str("\n</available_skills>");
        Ok(json!({
            "prompt": format!("{}\n\n{}", params.prompt, block),
            "activatedSkillNames": activated_names,
        }))
    }

    async fn copy_to_common(
        &self,
        params: &ListParams,
        skill_id: &str,
    ) -> Result<Value, ServiceError> {
        let skill = self.find_skill(params, skill_id).await?;
        let path = skill["path"].as_str().unwrap_or_default();
        let scope = skill["scope"].as_str().unwrap_or_default();
        let source_dir = PathBuf::from(path)
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf();
        let common_root = if scope == "workspace" {
            self.workspace_nc_code_root(Path::new(&params.workspace_path))
        } else {
            self.user_nc_code_root()
        };
        let leaf = source_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let target_dir = common_root.join(&leaf);
        if path_exists(&target_dir).await {
            return Err(ServiceError::new(format!(
                "The skill with the same name already exists in the general directory: {leaf}"
            )));
        }
        tokio::fs::create_dir_all(&common_root).await?;
        copy_dir_all(&source_dir, &target_dir).await?;
        Ok(json!({ "newPath": target_dir.join(SKILL_FILE_NAME).to_string_lossy() }))
    }

    async fn remove_from_common(
        &self,
        params: &ListParams,
        skill_id: &str,
    ) -> Result<(), ServiceError> {
        let skill = self.find_skill(params, skill_id).await?;
        let path = skill["path"].as_str().unwrap_or_default();
        let normalized = path.to_lowercase().replace('\\', "/");
        let user_root = self
            .user_nc_code_root()
            .to_string_lossy()
            .replace('\\', "/")
            .to_lowercase();
        let workspace_root = self
            .workspace_nc_code_root(Path::new(&params.workspace_path))
            .to_string_lossy()
            .replace('\\', "/")
            .to_lowercase();
        if !normalized.contains(&format!("{user_root}/"))
            && !normalized.contains(&format!("{workspace_root}/"))
        {
            return Err(ServiceError::new(
                "This skill is not in the general directory",
            ));
        }
        remove_dir_force(Path::new(path).parent().unwrap_or(Path::new("."))).await
    }

    async fn delete_skill(&self, params: &ListParams, skill_id: &str) -> Result<(), ServiceError> {
        let skill = self.find_skill(params, skill_id).await?;
        if skill["scope"].as_str() == Some("plugin") {
            return Err(ServiceError::new(
                "The skills provided by the plug-in cannot be deleted individually, please uninstall the corresponding plug-in",
            ));
        }
        let source = skill["sourcePath"]
            .as_str()
            .unwrap_or_else(|| skill["path"].as_str().unwrap_or_default());
        let skill_dir = PathBuf::from(source)
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf();
        let leaf = skill_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Resolve the parent only: a leaf that is itself a symlink must stay unresolved so the
        // delete removes the link, not the imported target.
        let canonical_parent =
            match tokio::fs::canonicalize(skill_dir.parent().unwrap_or(Path::new("."))).await {
                Ok(path) => strip_verbatim(&path),
                Err(_) => {
                    return Err(ServiceError::new(format!(
                        "This skill cannot be deleted: {}",
                        skill["path"].as_str().unwrap_or_default()
                    )))
                }
            };
        let mut allowed = ancestor_workspace_roots(Path::new(&params.workspace_path)).await;
        allowed.push(self.user_nc_code_root());
        allowed.push(self.user_agents_root());
        let mut contained = false;
        for root in allowed {
            let canonical_root = realpath(&root).await;
            if let Some(relative) = lexically_relative(&canonical_root, &canonical_parent) {
                // The parent sitting **on** a controlled root is the common layout, so the empty
                // relative path is allowed too.
                if relative.as_os_str().is_empty()
                    || (!relative.starts_with("..") && !relative.is_absolute())
                {
                    contained = true;
                    break;
                }
            }
        }
        if !contained {
            return Err(ServiceError::new(format!(
                "This skill cannot be deleted: {}",
                skill["path"].as_str().unwrap_or_default()
            )));
        }
        remove_dir_force(&canonical_parent.join(&leaf)).await
    }
}

struct DiscoverParams {
    workspace_path: PathBuf,
    include_user_skills: bool,
}

struct DiscoverResult {
    skills: Vec<Value>,
    diagnostics: Vec<Value>,
}

struct ListParams {
    workspace_path: String,
    // Contract field: TS also only accepts it without using it (packages/services/src/skills/skills.ts:8),
    // and the audit log is written only on the buildPromptContext path (skillsService.ts:289).
    #[allow(dead_code)]
    workspace_identity: Option<String>,
}

struct EnabledParams {
    workspace_path: String,
    // Same as ListParams: a contract field that the TS behavior side never reads.
    #[allow(dead_code)]
    workspace_identity: Option<String>,
    skill_id: String,
    enabled: bool,
}

struct PromptParams {
    workspace_path: String,
    workspace_identity: Option<String>,
    prompt: String,
}

struct RootDescriptor {
    scope: &'static str,
    root_path: PathBuf,
    plugin_name: Option<String>,
    plugin_id: Option<String>,
}

struct PluginCandidate {
    default_enabled: bool,
    marketplace: String,
    root_path: PathBuf,
}

/// `SkillSummary.id`: `<provider>:<scope>:<name>:sha256(path)[0..12]`.
fn build_skill_id(scope: &str, name: &str, path: &str) -> String {
    let digest = Sha256::digest(path.as_bytes());
    let hash: String = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{PROVIDER}:{scope}:{name}:{hash}")
}

fn normalize_skill_config_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn read_skill_enabled_map_from_config(config: &Map<String, Value>) -> HashMap<String, bool> {
    let mut result = HashMap::new();
    let Some(skills) = config.get("skills").and_then(Value::as_object) else {
        return result;
    };
    for (path, value) in skills {
        if let Some(Value::Bool(enable)) = value.get("enable") {
            result.insert(normalize_skill_config_path(path), *enable);
        }
    }
    result
}

/// `SKILL_SETTINGS_DIR` = `<home>/.zcode/v2`; the audit log is append-only JSON lines.
async fn append_skills_audit_log(
    paths: &AppPaths,
    workspace_path: &str,
    workspace_identity: Option<&str>,
    activated_skill_names: &[String],
) -> Result<(), ServiceError> {
    let dir = paths.home_dir().join(".zcode").join("v2");
    tokio::fs::create_dir_all(&dir).await?;
    let line = json!({
        "createdAt": now_millis(),
        "workspacePath": workspace_path,
        "workspaceIdentity": workspace_identity,
        "activatedSkillNames": activated_skill_names,
    });
    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("skills-audit.log"))
        .await?;
    file.write_all(format!("{line}\n").as_bytes()).await?;
    Ok(())
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as u64)
        .unwrap_or_default()
}

/// `collectMentionedSkillNames`: `$kebab-case` mentions.
fn collect_mentioned_skill_names(prompt: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let bytes: Vec<char> = prompt.chars().collect();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != '$' {
            index += 1;
            continue;
        }
        let mut cursor = index + 1;
        let mut segment = String::new();
        let mut saw_dash = false;
        while cursor < bytes.len() {
            let ch = bytes[cursor];
            if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
                segment.push(ch);
                saw_dash = false;
                cursor += 1;
                continue;
            }
            if ch == '-' && !segment.is_empty() && !saw_dash {
                // `(?:-[a-z0-9]+)*`: a dash must be followed by an alphanumeric run.
                let mut lookahead = cursor + 1;
                let mut tail = String::new();
                while lookahead < bytes.len()
                    && (bytes[lookahead].is_ascii_lowercase() || bytes[lookahead].is_ascii_digit())
                {
                    tail.push(bytes[lookahead]);
                    lookahead += 1;
                }
                if tail.is_empty() {
                    break;
                }
                segment.push('-');
                segment.push_str(&tail);
                saw_dash = true;
                cursor = lookahead;
                continue;
            }
            break;
        }
        if !segment.is_empty() {
            names.insert(segment);
            index = cursor;
        } else {
            index += 1;
        }
    }
    names
}

fn escape_xml_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn diagnostic(
    code: &str,
    severity: &str,
    message: String,
    path: Option<String>,
    skill_name: Option<String>,
) -> Value {
    let mut entry = Map::new();
    entry.insert("code".to_string(), json!(code));
    entry.insert("severity".to_string(), json!(severity));
    entry.insert("message".to_string(), json!(message));
    if let Some(path) = path {
        entry.insert("path".to_string(), json!(path));
    }
    if let Some(skill_name) = skill_name {
        entry.insert("skillName".to_string(), json!(skill_name));
    }
    Value::Object(entry)
}

fn parent_dir_name(path: &Path) -> String {
    path.parent()
        .and_then(|parent| parent.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

async fn path_exists(path: &Path) -> bool {
    tokio::fs::try_exists(path).await.unwrap_or(false)
}

/// `fs.realpath` with the Windows verbatim prefix removed so both sides of a comparison are
/// directly comparable strings, as Node returns.
fn strip_verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(stripped) = text.strip_prefix(r"\\?\").map(str::to_string) {
        if let Some(unc) = stripped.strip_prefix("UNC\\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        return PathBuf::from(stripped);
    }
    path.to_path_buf()
}

async fn realpath(path: &Path) -> PathBuf {
    match tokio::fs::canonicalize(path).await {
        Ok(resolved) => strip_verbatim(&resolved),
        Err(_) => path.to_path_buf(),
    }
}

fn lexically_relative(root: &Path, target: &Path) -> Option<PathBuf> {
    let root_components: Vec<_> = root.components().collect();
    let target_components: Vec<_> = target.components().collect();
    let mut shared = 0usize;
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

/// `findWorktreeRoot` + `resolveAncestorWorkspaceRoots`: every level from `workspacePath` up to
/// the directory containing `.git`, contributing both `.zcode/skills` and `.agents/skills`.
async fn ancestor_workspace_roots(workspace_path: &Path) -> Vec<PathBuf> {
    let workspace_path = crate::user_config::absolute_lexical(workspace_path);
    let worktree_root = find_worktree_root(&workspace_path).await;
    let mut base_directories: Vec<PathBuf> = Vec::new();
    match worktree_root {
        None => base_directories.push(workspace_path.clone()),
        Some(root) => {
            let mut current = workspace_path.clone();
            loop {
                base_directories.push(current.clone());
                if current == root || current.parent().is_none() {
                    break;
                }
                current = current.parent().unwrap_or(&current).to_path_buf();
            }
        }
    }
    let mut roots = Vec::new();
    for dir in base_directories {
        roots.push(dir.join(".zcode").join("skills"));
        roots.push(dir.join(".agents").join("skills"));
    }
    roots
}

async fn find_worktree_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        if path_exists(&current.join(GIT_MARKER)).await {
            return Some(current);
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return None,
        }
    }
}

fn dedupe_preserving_order(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for path in paths {
        if seen.insert(path.clone()) {
            out.push(path);
        }
    }
    out
}

fn normalize_scan_root_path(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    text.trim_end_matches('/').to_string()
}

/// `filterNestedScanRoots`: drop an existing ancestor whose subtree is fully covered by an
/// existing descendant root; keep ancestors that do not exist yet (non-standard layouts).
async fn filter_nested_scan_roots(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut existing: HashSet<String> = HashSet::new();
    for path in paths {
        if path_exists(path).await {
            existing.insert(normalize_scan_root_path(path));
        }
    }
    let normalized: Vec<String> = paths
        .iter()
        .map(|path| normalize_scan_root_path(path))
        .collect();
    paths
        .iter()
        .cloned()
        .enumerate()
        .filter(|(index, _path)| {
            let current = normalized[*index].clone();
            if !existing.contains(&current) {
                return true;
            }
            !normalized.iter().enumerate().any(|(other_index, other)| {
                if other_index == *index || !existing.contains(other) {
                    return false;
                }
                other.starts_with(&format!("{current}/"))
            })
        })
        .map(|(_, path)| path)
        .collect()
}

async fn dedupe_scan_roots_by_realpath(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut result = Vec::new();
    for path in filter_nested_scan_roots(paths).await {
        let mut canonical = normalize_scan_root_path(&path);
        if path_exists(&path).await {
            canonical = normalize_scan_root_path(&realpath(&path).await);
        }
        if seen.insert(canonical) {
            result.push(path);
        }
    }
    result
}

fn should_walk_skill_directory_entry(name: &str) -> bool {
    if EXCLUDED_DIRECTORY_NAMES.contains(&name) {
        return false;
    }
    if !name.starts_with('.') {
        return true;
    }
    DOT_DIR_ALLOWLIST.contains(&name)
}

/// `walkSkillMarkdownPaths`: bounded DFS that yields every directory containing a `SKILL.md`.
async fn walk_skill_markdown_paths(root: &Path, diagnostics: &mut Vec<Value>) -> Vec<PathBuf> {
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    let mut visited_symlink_targets: HashSet<String> = HashSet::new();
    let mut discovered: Vec<PathBuf> = Vec::new();
    while let Some((dir, depth)) = stack.pop() {
        let entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(error) => {
                diagnostics.push(diagnostic(
                    "skill_scan_failed",
                    "warning",
                    error.to_string(),
                    Some(dir.to_string_lossy().into_owned()),
                    None,
                ));
                continue;
            }
        };
        let mut names: Vec<(String, std::fs::FileType)> = Vec::new();
        let mut entries = entries;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(file_type) = entry.file_type().await else {
                continue;
            };
            names.push((entry.file_name().to_string_lossy().into_owned(), file_type));
        }
        let mut has_skill_file = false;
        let mut child_directories = Vec::new();
        let mut child_symlinks = Vec::new();
        for (name, file_type) in names {
            if name == SKILL_FILE_NAME && !file_type.is_dir() {
                has_skill_file = true;
                continue;
            }
            if !should_walk_skill_directory_entry(&name) {
                continue;
            }
            let entry_path = dir.join(&name);
            if file_type.is_dir() {
                child_directories.push(entry_path);
            } else if file_type.is_symlink() {
                child_symlinks.push(entry_path);
            }
        }
        if has_skill_file {
            discovered.push(dir.join(SKILL_FILE_NAME));
        }
        if depth >= MAX_SKILL_SCAN_DEPTH {
            continue;
        }
        for child in child_directories {
            stack.push((child, depth + 1));
        }
        for symlink in child_symlinks {
            let metadata = match tokio::fs::metadata(&symlink).await {
                Ok(metadata) => metadata,
                Err(error) => {
                    diagnostics.push(diagnostic(
                        "skill_scan_failed",
                        "warning",
                        error.to_string(),
                        Some(symlink.to_string_lossy().into_owned()),
                        None,
                    ));
                    continue;
                }
            };
            if !metadata.is_dir() {
                continue;
            }
            let canonical = realpath(&symlink).await;
            if visited_symlink_targets.insert(canonical.to_string_lossy().into_owned()) {
                stack.push((symlink, depth + 1));
            }
        }
    }
    discovered
}

async fn collect_skill_markdown_paths(root: &Path, diagnostics: &mut Vec<Value>) -> Vec<PathBuf> {
    let mut discovered: HashSet<PathBuf> = HashSet::new();
    for path in walk_skill_markdown_paths(root, diagnostics).await {
        discovered.insert(path);
    }
    let mut paths: Vec<PathBuf> = discovered.into_iter().collect();
    paths.sort_by(|left, right| locale_en_cmp(&left.to_string_lossy(), &right.to_string_lossy()));
    paths
}

async fn read_skill_name_key(skill_path: &Path) -> String {
    let fallback = parent_dir_name(skill_path);
    let Ok(content) = tokio::fs::read_to_string(skill_path).await else {
        return fallback.trim().to_lowercase();
    };
    let parsed = read_frontmatter(&content);
    let raw_name = if parsed.has_frontmatter {
        parsed.name.trim().to_string()
    } else {
        fallback.clone()
    };
    let resolved = if raw_name.is_empty() {
        fallback
    } else {
        raw_name
    };
    resolved.trim().to_lowercase()
}

async fn collect_skill_name_keys_in_root(root: &Path) -> HashSet<String> {
    let mut keys = HashSet::new();
    if !path_exists(root).await {
        return keys;
    }
    let mut diagnostics = Vec::new();
    for skill_path in collect_skill_markdown_paths(root, &mut diagnostics).await {
        keys.insert(read_skill_name_key(&skill_path).await);
    }
    keys
}

async fn is_user_agents_skill_covered_by_zcode(
    skill_path: &Path,
    user_nc_code_root: &Path,
    user_nc_code_name_keys: &HashSet<String>,
) -> bool {
    let leaf = skill_path
        .parent()
        .and_then(|parent| parent.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if path_exists(&user_nc_code_root.join(leaf).join(SKILL_FILE_NAME)).await {
        return true;
    }
    user_nc_code_name_keys.contains(&read_skill_name_key(skill_path).await)
}

/// `resolvePluginSkillRoots`
fn resolve_plugin_skill_roots(
    manifest: &crate::plugins::PluginManifest,
    root_path: &Path,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for raw in parse_path_list(manifest.entries.get("skills")) {
        if let Some(resolved) = resolve_inside(root_path, &raw) {
            roots.push(resolved);
        }
    }
    if roots.is_empty() && manifest.entries.get("skills").is_none() {
        let default_root = root_path.join("skills");
        if std::fs::symlink_metadata(default_root.join(SKILL_FILE_NAME)).is_ok()
            || std::fs::symlink_metadata(&default_root).is_ok()
        {
            roots.push(default_root);
        }
    }
    roots
}

async fn read_skill_metadata(skill_path: &Path) -> Option<Value> {
    let meta_path = skill_path.parent()?.join(SKILL_META_FILE_NAME);
    let raw = tokio::fs::read_to_string(meta_path).await.ok()?;
    let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(&raw) else {
        return None;
    };
    let mut metadata = Map::new();
    for key in ["slug", "version", "ownerId"] {
        if let Some(value) = parsed
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            metadata.insert(key.to_string(), json!(value));
        }
    }
    if let Some(published_at) = parsed.get("publishedAt").and_then(Value::as_f64) {
        if published_at.is_finite() {
            metadata.insert("publishedAt".to_string(), json!(published_at));
        }
    }
    if metadata.is_empty() {
        None
    } else {
        Some(Value::Object(metadata))
    }
}

/// `cp(sourceDir, targetDir, { recursive: true })`: Node's default dereferences symlinks
/// (`verbatimSymlinks: false`), so the copy is decided from the followed metadata.
fn copy_dir_all<'a>(
    source: &'a Path,
    target: &'a Path,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ServiceError>> + Send + 'a>> {
    Box::pin(async move {
        tokio::fs::create_dir_all(target).await?;
        let mut entries = tokio::fs::read_dir(source).await?;
        while let Some(entry) = entries.next_entry().await? {
            let destination = target.join(entry.file_name());
            let Ok(metadata) = tokio::fs::metadata(entry.path()).await else {
                continue;
            };
            if metadata.is_dir() {
                Box::pin(copy_dir_all(&entry.path(), &destination)).await?;
            } else if metadata.is_file() {
                tokio::fs::copy(entry.path(), destination).await?;
            }
        }
        Ok(())
    })
}

async fn remove_dir_force(path: &Path) -> Result<(), ServiceError> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            // A plain file at that path is removed the same way `rm -rf` tolerates it.
            match tokio::fs::remove_file(path).await {
                Ok(()) => Ok(()),
                Err(_) => Err(error.into()),
            }
        }
    }
}

// --------------------------------------------------------------- frontmatter

pub struct ParsedFrontmatter {
    pub has_frontmatter: bool,
    pub name: String,
    pub description: String,
    pub body: String,
}

/// `readFrontmatter`: CRLF-normalized, frontmatter delimited by `---`, strict YAML first with a
/// documented loose fallback for historical non-strict metadata.
fn read_frontmatter(content: &str) -> ParsedFrontmatter {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let Some(frontmatter_text) = extract_frontmatter(&normalized) else {
        return ParsedFrontmatter {
            has_frontmatter: false,
            name: String::new(),
            description: String::new(),
            body: normalized.trim().to_string(),
        };
    };
    let (metadata_text, leaked_body) = split_frontmatter_and_leaked_body(&frontmatter_text.text);
    let body_after = normalized[frontmatter_text.consumed..].trim();
    let mut parts: Vec<&str> = Vec::new();
    if !leaked_body.trim().is_empty() {
        parts.push(leaked_body.trim());
    }
    if !body_after.is_empty() {
        parts.push(body_after);
    }
    let body = parts.join("\n\n").trim().to_string();
    let loose = read_loose_frontmatter_fields(&metadata_text);
    if has_yaml_unsafe_loose_inline_field(&metadata_text) {
        // Historical Chinese descriptions are written unquoted as `Trigger scene: …`; `: ` makes the
        // strict parser see a mapping, so the loose reading is used directly.
        return ParsedFrontmatter {
            has_frontmatter: true,
            name: loose.name,
            description: loose.description,
            body,
        };
    }
    match crate::yaml_lite::parse(&metadata_text) {
        Ok(Value::Object(parsed)) => {
            let name = frontmatter_string(parsed.get("name"));
            let description = frontmatter_string(parsed.get("description"));
            ParsedFrontmatter {
                has_frontmatter: true,
                name: if name.is_empty() { loose.name } else { name },
                description: if description.is_empty() {
                    loose.description
                } else {
                    description
                },
                body,
            }
        }
        _ => ParsedFrontmatter {
            has_frontmatter: true,
            name: loose.name,
            description: loose.description,
            body,
        },
    }
}

struct FrontmatterMatch {
    text: String,
    /// Byte offset just past the closing fence (including its newline) in the normalized text.
    consumed: usize,
}

/// `/^---\n([\s\S]*?)\n---(?:\n|$)/` applied to the normalized content.
fn extract_frontmatter(normalized: &str) -> Option<FrontmatterMatch> {
    if !normalized.starts_with("---\n") {
        return None;
    }
    let rest = &normalized[4..];
    let mut cursor = 0usize;
    loop {
        let found = rest[cursor..].find("\n---")? + cursor;
        let after = found + "\n---".len();
        let terminates = after == rest.len() || rest[after..].starts_with('\n');
        if terminates {
            let text = rest[cursor..found].to_string();
            let consumed = 4 + after + if after < rest.len() { 1 } else { 0 };
            return Some(FrontmatterMatch { text, consumed });
        }
        cursor = found + 1;
    }
}

/// `splitFrontmatterAndLeakedBody`: a Markdown heading inside the frontmatter block is really
/// body text written before the closing fence, so it moves back to the body.
fn split_frontmatter_and_leaked_body(frontmatter_text: &str) -> (String, String) {
    let lines: Vec<&str> = frontmatter_text.split('\n').collect();
    let leaked_start = lines.iter().enumerate().find_map(|(index, line)| {
        if index > 0 && is_markdown_heading(line) {
            Some(index)
        } else {
            None
        }
    });
    match leaked_start {
        None => (frontmatter_text.to_string(), String::new()),
        Some(index) => (
            lines[..index].join("\n").trim_end().to_string(),
            lines[index..].join("\n").trim().to_string(),
        ),
    }
}

fn is_markdown_heading(line: &str) -> bool {
    let hashes = line.chars().take_while(|ch| *ch == '#').count();
    if hashes == 0 || hashes > 6 {
        return false;
    }
    let rest = &line[hashes..];
    rest.starts_with([' ', '\t'])
        && rest
            .trim_start()
            .chars()
            .next()
            .is_some_and(|ch| !ch.is_whitespace())
}

struct LooseFields {
    name: String,
    description: String,
}

/// `readLooseFrontmatterFields`
fn read_loose_frontmatter_fields(frontmatter_text: &str) -> LooseFields {
    let lines: Vec<String> = frontmatter_text.split('\n').map(str::to_string).collect();
    let mut name = String::new();
    let mut description = String::new();
    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index].clone();
        let colon = match line.find(':') {
            Some(colon) => colon,
            None => {
                index += 1;
                continue;
            }
        };
        let key = line[..colon].trim();
        let raw_value = line[colon + 1..].trim();
        if key == "name" {
            name = loose_inline_string(raw_value);
        } else if key == "description" {
            let block_style = raw_value.chars().next();
            if matches!(block_style, Some('|') | Some('>')) {
                let mut block_lines: Vec<String> = Vec::new();
                let mut next = index + 1;
                while next < lines.len() {
                    let next_line = lines[next].clone();
                    if !next_line.starts_with(' ') && !next_line.starts_with('\t') {
                        break;
                    }
                    block_lines.push(strip_leading_one_or_two_spaces(&next_line));
                    index = next;
                    next += 1;
                }
                description = if block_style == Some('>') {
                    block_lines.join(" ")
                } else {
                    block_lines.join("\n")
                };
            } else {
                description = loose_inline_string(raw_value);
            }
        }
        index += 1;
    }
    LooseFields { name, description }
}

/// `line.replace(/^\s{1,2}/, "")` — a block-scalar continuation loses at most two leading
/// characters of indentation, exactly as the TypeScript regular expression does.
fn strip_leading_one_or_two_spaces(line: &str) -> String {
    let mut remaining = line;
    for _ in 0..2 {
        let mut chars = remaining.chars();
        match chars.next() {
            Some(ch) if ch == ' ' || ch == '\t' => remaining = chars.as_str(),
            _ => break,
        }
    }
    remaining.to_string()
}

fn loose_inline_string(value: &str) -> String {
    let trimmed = value.trim();
    trimmed
        .strip_prefix(['"', '\''])
        .and_then(|inner| inner.strip_suffix(['"', '\'']))
        .unwrap_or(trimmed)
        .to_string()
}

/// `hasYamlUnsafeLooseInlineField`
fn has_yaml_unsafe_loose_inline_field(frontmatter_text: &str) -> bool {
    for line in frontmatter_text.split('\n') {
        let Some(colon) = line.find(':') else {
            continue;
        };
        let key = line[..colon].trim();
        if key != "name" && key != "description" {
            continue;
        }
        let raw_value = line[colon + 1..].trim();
        if raw_value.is_empty()
            || raw_value.starts_with('"')
            || raw_value.starts_with('\'')
            || raw_value.starts_with('|')
            || raw_value.starts_with('>')
        {
            continue;
        }
        if raw_value.contains(": ") {
            return true;
        }
    }
    false
}

/// `readFrontmatterString`: strings pass through, null/undefined become `""`, anything else is
/// stringified.
fn frontmatter_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(text)) => text.to_string(),
        Some(Value::Number(number)) => number.to_string(),
        Some(other) => other.to_string(),
    }
}

#[async_trait::async_trait]
impl Channel for SkillsService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        match command {
            "list" => {
                let params = args.required_json(0, "params")?;
                let list = read_list_params(&params)?;
                Ok(WireValue::Json(self.list(&list).await?))
            }
            "setEnabled" => {
                let params = args.required_json(0, "params")?;
                let object = params.as_object().cloned().unwrap_or_default();
                let enabled = object
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| {
                        ServiceError::invalid_params("params.enabled must be a boolean")
                    })?;
                self.set_enabled(&EnabledParams {
                    workspace_path: require_string(&object, "workspacePath")?,
                    workspace_identity: optional_string(&object, "workspaceIdentity"),
                    skill_id: require_string(&object, "skillId")?,
                    enabled,
                })
                .await?;
                Ok(WireValue::Undefined)
            }
            "buildPromptContext" => {
                let params = args.required_json(0, "params")?;
                let object = params.as_object().cloned().unwrap_or_default();
                let context = self
                    .build_prompt_context(&PromptParams {
                        workspace_path: require_string(&object, "workspacePath")?,
                        workspace_identity: optional_string(&object, "workspaceIdentity"),
                        prompt: require_string(&object, "prompt")?,
                    })
                    .await?;
                Ok(WireValue::Json(context))
            }
            "copyToCommon" => {
                let params = args.required_json(0, "params")?;
                let list = read_list_params(&params)?;
                let skill_id = params
                    .get("skillId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ServiceError::invalid_params("params.skillId must be a string")
                    })?;
                Ok(WireValue::Json(self.copy_to_common(&list, skill_id).await?))
            }
            "removeFromCommon" => {
                let params = args.required_json(0, "params")?;
                let list = read_list_params(&params)?;
                let skill_id = params
                    .get("skillId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ServiceError::invalid_params("params.skillId must be a string")
                    })?;
                self.remove_from_common(&list, skill_id).await?;
                Ok(WireValue::Undefined)
            }
            "deleteSkill" => {
                let params = args.required_json(0, "params")?;
                let list = read_list_params(&params)?;
                let skill_id = params
                    .get("skillId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ServiceError::invalid_params("params.skillId must be a string")
                    })?;
                self.delete_skill(&list, skill_id).await?;
                Ok(WireValue::Undefined)
            }
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

fn read_list_params(params: &Value) -> Result<ListParams, ServiceError> {
    let object = params
        .as_object()
        .ok_or_else(|| ServiceError::invalid_params("params must be an object"))?;
    Ok(ListParams {
        workspace_path: require_string(object, "workspacePath")?,
        workspace_identity: optional_string(object, "workspaceIdentity"),
    })
}

fn require_string(object: &Map<String, Value>, key: &str) -> Result<String, ServiceError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ServiceError::invalid_params(format!("params.{key} must be a string")))
}

fn optional_string(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_string)
}
