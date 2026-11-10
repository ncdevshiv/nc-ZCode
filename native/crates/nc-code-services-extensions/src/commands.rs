//! `commands` channel — user and plugin slash commands.
//!
//! Port of `packages/services/src/commands/commandsService.ts`. Same roots
//! (`<home>/.zcode/commands`, `<home>/.agents/commands`, `<workspace>/.zcode/commands`,
//! `<workspace>/.agents/commands`), same plugin-command discovery, same enable-flag storage:
//! the flag lives in `~/.zcode/cli/config.json` under `command["<absolute file path>"].enable`,
//! keyed by the command **file path** — exactly where TypeScript writes it, so a command
//! disabled by the CLI stays disabled here and vice versa.
//!
//! Discovery order is load-bearing and preserved: per agent source, all workspace roots first,
//! then all user roots; within a scope `.zcode` is scanned before `.agents` and `.agents` is
//! skipped entirely when `.zcode` contributed at least one command. Commands are then deduped
//! by name with the first occurrence winning (project before user), and plugin commands are
//! appended after the user commands, sorted by name.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::atomic::{atomic_write_text, read_text_if_exists};
use nc_code_service_core::{AppPaths, ServiceArgs, ServiceError};
use serde_json::{json, Map, Value};

use crate::command_markdown::{
    generate_command_file_content, parse_command_file, CommandConfig, ParsedCommandFile,
};
use crate::plugins::{
    is_default_enabled_official_plugin, parse_path_list, read_installed_plugin_roots,
    read_plugin_manifest, resolve_inside, scan_official_plugin_cache_root_paths,
    INLINE_PLUGIN_MARKETPLACE, OFFICIAL_PLUGIN_MARKETPLACE,
};
use crate::text::locale_en_cmp;
use crate::user_config::{
    cli_config_path, read_boolean_record, read_cli_config, read_storage_dir, read_string_array,
    resolve_config_path, resolve_plugin_storage_root,
};

const ZCODE_COMMAND_AGENT_SOURCE: &str = "zcodeAgent";
const ENABLE_OVERRIDE_KEY: &str = "enable";
const COMMAND_FILE_EXTENSION: &str = ".md";

// `BUILTIN_ZCODE_SLASH_COMMAND_HELP_ENTRIES` names + aliases +
// `EXTRA_RESERVED_SLASH_COMMAND_NAMES` (`packages/shared/src/zcode-slash-command-help.ts`
// and `apps/zcode-cli/packages/bootstrap/src/slash-command-surface.ts`). A custom command
// with a reserved name never reaches the protocol directory.
const RESERVED_SLASH_COMMAND_NAMES: &[&str] = &[
    "help", "login", "logout", "compact", "init", "expert", "variant", "effort", "dwf", "fork",
    "locale", "language", "mcp", "plugins", "plugin", "mode", "model", "new", "clear", "resume",
    "continue", "rewind", "skill", "goal", "target", "compress", "plan",
];

/// `APP_PROTOCOL_VISIBLE_BUILTIN_SLASH_COMMAND_NAMES` (`goal`/`compact`/`init`) plus
/// `APP_PROTOCOL_APP_ONLY_BUILTIN_SLASH_COMMANDS` (`plan`). Summary/usage strings mirror
/// `BUILTIN_ZCODE_SLASH_COMMAND_HELP_ENTRIES`, the single help table.
fn builtin_protocol_slash_commands() -> Vec<Value> {
    let entry = |name: &str, description: &str, input_hint: &str| json!({"description": description, "inputHint": input_hint, "name": name, "source": "builtin"});
    vec![
        entry(
            "goal",
            "Show or set the current session goal.",
            "/goal [pause|resume|clear|replace <objective>|<objective>]",
        ),
        entry(
            "compact",
            "Compact the current conversation with optional instructions.",
            "/compact [instructions]",
        ),
        entry(
            "init",
            "Create or update workspace AGENTS.md instructions.",
            "/init [notes]",
        ),
        entry(
            "plan",
            "Switch to Plan mode and optionally send a task.",
            "/plan [task]",
        ),
    ]
}

/// `CommandAgentSourceDescriptor`. Only `zcodeAgent` exists in the contract today; the fields
/// that the TypeScript descriptor varies per source (extension, namespace separator, argument
/// hint support) are kept explicit so the port does not silently assume them.
struct CommandDescriptor {
    directory_source: &'static str,
    segments: [&'static str; 2],
}

const ZCODE_COMMAND_DESCRIPTOR: CommandDescriptor = CommandDescriptor {
    directory_source: "zcode",
    segments: [".zcode", "commands"],
};
const AGENTS_DESCRIPTOR: CommandDescriptor = CommandDescriptor {
    directory_source: "agents",
    segments: [".agents", "commands"],
};
const DIRECTORY_SOURCE_DESCRIPTORS: [&CommandDescriptor; 2] =
    [&ZCODE_COMMAND_DESCRIPTOR, &AGENTS_DESCRIPTOR];

pub struct CommandsService {
    paths: AppPaths,
}

impl CommandsService {
    pub fn new(paths: AppPaths) -> Self {
        Self { paths }
    }

    fn descriptor_for(
        agent_source: Option<&str>,
    ) -> Result<&'static CommandDescriptor, ServiceError> {
        match agent_source {
            None | Some(ZCODE_COMMAND_AGENT_SOURCE) => Ok(&ZCODE_COMMAND_DESCRIPTOR),
            Some(other) => Err(ServiceError::invalid_params(format!(
                "Unsupported agentSource: {other}"
            ))),
        }
    }

    /// `getPrimaryUserCommandsDirectory`: the `.zcode` user root, created on demand because the
    /// Windows file manager cannot open a missing path.
    async fn get_primary_user_commands_directory(
        &self,
        agent_source: Option<&str>,
    ) -> Result<Value, ServiceError> {
        let descriptor = Self::descriptor_for(agent_source)?;
        let path = self.user_root(descriptor);
        tokio::fs::create_dir_all(&path).await?;
        Ok(json!({ "path": path.to_string_lossy() }))
    }

    fn user_root(&self, descriptor: &CommandDescriptor) -> PathBuf {
        let mut root = self.paths.home_dir().to_path_buf();
        for segment in descriptor.segments {
            root = root.join(segment);
        }
        root
    }

    fn workspace_root(&self, workspace_path: &Path, descriptor: &CommandDescriptor) -> PathBuf {
        let mut root = workspace_path.to_path_buf();
        for segment in descriptor.segments {
            root = root.join(segment);
        }
        root
    }

    /// Protocol slash-command directory (`listProtocolSlashCommands`): the protocol-visible
    /// builtins (`goal`/`compact`/`init`), the composer-only `plan` entry, then the user and
    /// plugin custom commands filtered by reserved-name and `disable-noninteractive`, mapped to
    /// `{name, description, inputHint, source}` with `workflow` pinned right after `goal`.
    pub async fn protocol_slash_commands(
        &self,
        workspace_path: &str,
    ) -> Result<Value, ServiceError> {
        let listed = self
            .list(&ListParams {
                agent_source: None,
                workspace_path: Some(workspace_path.to_string()),
            })
            .await?;
        let mut custom: Vec<Value> = listed
            .get("commands")
            .and_then(Value::as_array)
            .map(|commands| {
                commands
                    .iter()
                    .filter(|command| {
                        !command.get("disableNonInteractive").and_then(Value::as_bool).unwrap_or(false)
                    })
                    .filter_map(|command| {
                        let raw_name = command.get("name")?.as_str()?.trim_start_matches('/');
                        let name = raw_name.to_string();
                        if RESERVED_SLASH_COMMAND_NAMES.contains(&name.as_str()) {
                            return None;
                        }
                        let argument_hint = command.get("argumentHint").and_then(Value::as_str).unwrap_or("");
                        let input_hint = if argument_hint.is_empty() {
                            format!("/{name}")
                        } else {
                            format!("/{name} {argument_hint}")
                        };
                        Some(json!({
                            "description": command.get("description").and_then(Value::as_str).unwrap_or(""),
                            "inputHint": input_hint,
                            "name": name,
                            "source": "custom",
                        }))
                    })
                    .collect()
            })
            .unwrap_or_default();
        custom.sort_by(|a, b| {
            a.get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .cmp(b.get("name").and_then(Value::as_str).unwrap_or(""))
        });
        let mut directory = builtin_protocol_slash_commands();
        directory.extend(custom.iter().cloned());
        // `pinWorkflowAfterGoal`: the zcode-guide plugin's `workflow` command sits after `goal`.
        if let Some(index) = directory
            .iter()
            .position(|entry| entry["name"] == json!("workflow"))
        {
            if let Some(goal_index) = directory
                .iter()
                .position(|entry| entry["name"] == json!("goal"))
            {
                let workflow = directory.remove(index);
                let insert_at = if index < goal_index {
                    goal_index
                } else {
                    goal_index + 1
                };
                directory.insert(insert_at, workflow);
            }
        }
        Ok(Value::Array(directory))
    }

    async fn list(&self, params: &ListParams) -> Result<Value, ServiceError> {
        let overrides = self.read_command_enabled_overrides().await;
        let mut commands: Vec<Map<String, Value>> = Vec::new();

        // ZCode merges every workspace root before any user root; interleaving project/user per
        // directory would let the user `.zcode` win over the workspace `.agents`.
        if let Some(workspace_path) = params.workspace_path.as_deref() {
            self.discover_from_directory_sources(
                Path::new(workspace_path),
                "project",
                Some(workspace_path),
                &overrides,
                &mut commands,
            )
            .await;
        }
        self.discover_from_directory_sources(
            self.paths.home_dir(),
            "global",
            None,
            &overrides,
            &mut commands,
        )
        .await;

        let user_commands = dedupe_commands_by_name(commands);
        let plugin_commands = if params.agent_source.is_none()
            || params.agent_source.as_deref() == Some(ZCODE_COMMAND_AGENT_SOURCE)
        {
            self.discover_plugin_commands(&overrides).await
        } else {
            Vec::new()
        };

        let mut combined: Vec<Value> = user_commands.clone();
        combined.extend(plugin_commands.iter().map(|command| command.value.clone()));
        Ok(json!({
            "commands": combined,
            "userCommands": user_commands,
            "pluginCommands": plugin_commands.iter().map(|command| command.value.clone()).collect::<Vec<Value>>(),
            "capability": { "userScopeAvailable": true },
        }))
    }

    /// `discoverCommandsFromDirectorySources`
    async fn discover_from_directory_sources(
        &self,
        base: &Path,
        scope: &str,
        project_path: Option<&str>,
        overrides: &HashMap<String, bool>,
        commands: &mut Vec<Map<String, Value>>,
    ) {
        for descriptor in DIRECTORY_SOURCE_DESCRIPTORS {
            let before = commands.len();
            let commands_root = self.workspace_root(base, descriptor);
            if !tokio::fs::try_exists(&commands_root).await.unwrap_or(false) {
                // `discoverCommandsRoot` returns 0 when the root cannot be accessed.
                continue;
            }
            let _ = discover_user_commands_recursive(
                &commands_root,
                &commands_root,
                descriptor,
                scope,
                project_path,
                overrides,
                commands,
            )
            .await;
            // `.zcode` wins the scope: once it yields commands, `.agents` is not consulted.
            if descriptor.directory_source == "zcode" && commands.len() > before {
                break;
            }
        }
    }

    async fn discover_plugin_commands(
        &self,
        overrides: &HashMap<String, bool>,
    ) -> Vec<PluginCommand> {
        let roots = self.resolve_plugin_command_root_descriptors().await;
        let mut commands: Vec<PluginCommand> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for root in roots {
            collect_plugin_commands_recursive(
                &root.root_path,
                &root.root_path,
                &root,
                overrides,
                &mut seen,
                &mut commands,
            )
            .await;
        }
        commands.sort_by(|left, right| locale_en_cmp(&left.name, &right.name));
        commands
    }

    /// `resolvePluginCommandRootDescriptors`
    async fn resolve_plugin_command_root_descriptors(&self) -> Vec<PluginRootDescriptor> {
        let config = read_cli_config(&self.paths).await;
        let plugins = config
            .get("plugins")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let plugins_enabled = match plugins.get("enabled") {
            Some(Value::Bool(value)) => *value,
            _ => true,
        };
        if !plugins_enabled {
            return Vec::new();
        }
        let enabled_plugins = read_boolean_record(plugins.get("enabledPlugins"));
        let suppressed_builtins = read_string_array(plugins.get("suppressedBuiltins"));
        let plugin_storage_root = resolve_plugin_storage_root(
            &self.paths,
            &resolve_config_path(&self.paths, &read_storage_dir(&config)),
        );

        let mut candidates: Vec<PluginRootCandidate> = Vec::new();
        for dir in read_string_array(plugins.get("dirs")) {
            candidates.push(PluginRootCandidate {
                default_enabled: true,
                marketplace: INLINE_PLUGIN_MARKETPLACE.to_string(),
                root_path: resolve_config_path(&self.paths, &dir),
            });
        }
        for root_path in scan_official_plugin_cache_root_paths(&plugin_storage_root).await {
            candidates.push(PluginRootCandidate {
                default_enabled: false,
                marketplace: OFFICIAL_PLUGIN_MARKETPLACE.to_string(),
                root_path,
            });
        }
        for record in read_installed_plugin_roots(&plugin_storage_root).await {
            candidates.push(PluginRootCandidate {
                default_enabled: false,
                marketplace: record.marketplace,
                root_path: record.install_path,
            });
        }

        let mut descriptors = Vec::new();
        let mut seen_plugin_ids: HashSet<String> = HashSet::new();
        for candidate in candidates {
            let Some(manifest) = read_plugin_manifest(&candidate.root_path).await else {
                continue;
            };
            let plugin_id = format!("{}@{}", manifest.name, candidate.marketplace);
            // An uninstalled builtin only leaves `suppressedBuiltins` behind in the CLI config;
            // scanning the official cache directly has to honour it, or the removed plugin keeps
            // contributing commands.
            if candidate.marketplace == OFFICIAL_PLUGIN_MARKETPLACE
                && suppressed_builtins.contains(&plugin_id)
            {
                continue;
            }
            if !seen_plugin_ids.insert(plugin_id.clone()) {
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
            for root_path in resolve_plugin_command_roots(&manifest, &candidate.root_path) {
                descriptors.push(PluginRootDescriptor {
                    plugin_enabled: enabled,
                    plugin_marketplace: candidate.marketplace.clone(),
                    plugin_name: manifest.name.clone(),
                    root_path,
                });
            }
        }
        descriptors
    }

    // ---------------------------------------------------------------- enable flags

    /// `readCommandEnabledOverridesFromUserConfig`
    async fn read_command_enabled_overrides(&self) -> HashMap<String, bool> {
        read_command_enabled_overrides(&read_cli_config(&self.paths).await)
    }

    async fn set_command_enabled_override(
        &self,
        file_path: &str,
        enabled: bool,
    ) -> Result<(), ServiceError> {
        let mut config = read_cli_config(&self.paths).await;
        apply_command_enabled_override(&mut config, file_path, enabled);
        write_cli_config_map(&self.paths, &config).await
    }

    // ---------------------------------------------------------------- writers

    async fn write_command_file(&self, params: &CommandFileParams) -> Result<Value, ServiceError> {
        let descriptor = Self::descriptor_for(params.agent_source.as_deref())?;
        let target = self.commands_root_for_storage(
            descriptor,
            &params.storage_level,
            params.workspace_path.as_deref(),
        )?;
        tokio::fs::create_dir_all(&target.commands_root).await?;

        let file_name = command_file_name(&params.config.name);
        let file_path = target.commands_root.join(&file_name);
        if file_path_exists(&file_path).await {
            return Err(ServiceError::new(format!(
                "Command file already exists: {file_name}"
            )));
        }

        let content = generate_command_file_content(&params.config, None);
        atomic_write_text(&file_path, &content).await?;
        // A freshly written command is enabled: clear any stale path-keyed override.
        self.set_command_enabled_override(&file_path.to_string_lossy(), true)
            .await?;

        let parsed = parse_command_file(&content, &file_path.to_string_lossy());
        let name = command_name(&target.commands_root, &file_path);
        Ok(user_command_json(
            &parsed, &name, descriptor, &target, &file_path, true,
        ))
    }

    async fn update_command_file(&self, params: &UpdateParams) -> Result<Value, ServiceError> {
        let descriptor = Self::descriptor_for(params.agent_source.as_deref())?;
        let target = self.commands_root_for_storage(
            descriptor,
            &params.storage_level,
            params.workspace_path.as_deref(),
        )?;
        let file_name = command_file_name(&params.config.name);
        let new_file_path = target.commands_root.join(&file_name);
        let overrides = self.read_command_enabled_overrides().await;
        let existing_content = match params.old_file_path.as_deref() {
            Some(path) => read_text_if_exists(Path::new(path)).await.ok().flatten(),
            None => None,
        };

        if let Some(old_path) = params.old_file_path.as_deref() {
            if old_path != new_file_path.to_string_lossy() {
                // The old file may already be gone; ENOENT here is a normal rename outcome.
                let _ = tokio::fs::remove_file(old_path).await;
            }
        }
        if params.old_file_path.as_deref() != Some(new_file_path.to_string_lossy().as_ref())
            && file_path_exists(&new_file_path).await
        {
            return Err(ServiceError::new(format!(
                "Command file already exists: {file_name}"
            )));
        }

        let content = generate_command_file_content(&params.config, existing_content.as_deref());
        if let Some(parent) = new_file_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        atomic_write_text(&new_file_path, &content).await?;

        let old_path = params.old_file_path.as_deref().unwrap_or_default();
        let new_path_text = new_file_path.to_string_lossy().into_owned();
        if !old_path.is_empty() && old_path != new_path_text {
            if let Some(previous_enabled) = overrides.get(old_path) {
                // Disable state is keyed by file path, so a rename must migrate the override or
                // the freshly renamed command silently becomes enabled again.
                let mut config = read_cli_config(&self.paths).await;
                apply_command_enabled_override(&mut config, old_path, true);
                apply_command_enabled_override(&mut config, &new_path_text, *previous_enabled);
                write_cli_config_map(&self.paths, &config).await?;
            }
        }

        let parsed = parse_command_file(&content, &new_path_text);
        let name = command_name(&target.commands_root, &new_file_path);
        let enabled = *self
            .read_command_enabled_overrides()
            .await
            .get(&new_path_text)
            .unwrap_or(&true);
        Ok(user_command_json(
            &parsed,
            &name,
            descriptor,
            &target,
            &new_file_path,
            enabled,
        ))
    }

    async fn delete_command_file(&self, file_path: &str) -> Result<(), ServiceError> {
        match tokio::fs::remove_file(file_path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        self.set_command_enabled_override(file_path, true).await
    }

    fn commands_root_for_storage(
        &self,
        descriptor: &CommandDescriptor,
        storage_level: &str,
        workspace_path: Option<&str>,
    ) -> Result<StorageTarget, ServiceError> {
        if storage_level == "project" {
            let workspace = workspace_path
                .filter(|value| !value.is_empty())
                .ok_or_else(|| ServiceError::new("Missing workspace path for project command"))?;
            return Ok(StorageTarget {
                commands_root: self.workspace_root(Path::new(workspace), descriptor),
                scope: "project",
                project_path: Some(workspace.to_string()),
            });
        }
        Ok(StorageTarget {
            commands_root: self.user_root(descriptor),
            scope: "global",
            project_path: None,
        })
    }
}

struct StorageTarget {
    commands_root: PathBuf,
    scope: &'static str,
    project_path: Option<String>,
}

struct PluginRootDescriptor {
    plugin_enabled: bool,
    plugin_marketplace: String,
    plugin_name: String,
    root_path: PathBuf,
}

struct PluginRootCandidate {
    default_enabled: bool,
    marketplace: String,
    root_path: PathBuf,
}

struct PluginCommand {
    name: String,
    value: Value,
}

/// `buildCommandLocation`
fn build_location(
    descriptor: &CommandDescriptor,
    commands_root: &Path,
    scope: &str,
    project_path: Option<&str>,
) -> Map<String, Value> {
    let mut location = Map::new();
    location.insert("source".to_string(), json!(descriptor.directory_source));
    location.insert(
        "scope".to_string(),
        json!(if scope == "project" {
            "project"
        } else {
            "user"
        }),
    );
    location.insert(
        "directoryPath".to_string(),
        json!(commands_root.to_string_lossy()),
    );
    if let Some(project_path) = project_path {
        location.insert("projectPath".to_string(), json!(project_path));
    }
    location
}

fn build_command_id(scope: &str, name: &str, location: &Map<String, Value>) -> String {
    format!(
        "{ZCODE_COMMAND_AGENT_SOURCE}:{}:{scope}:{name}",
        location
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or_default()
    )
}

fn build_plugin_command_id(
    file_path: &str,
    name: &str,
    marketplace: &str,
    plugin_name: &str,
) -> String {
    format!("plugin:{marketplace}:{plugin_name}:{name}:{file_path}")
}

/// `{ ...parsed, name, agentSource, location, id, source, enabled, scope, projectPath? }`
fn user_command_json(
    parsed: &ParsedCommandFile,
    name: &str,
    descriptor: &CommandDescriptor,
    target: &StorageTarget,
    file_path: &Path,
    enabled: bool,
) -> Value {
    let mut command = parsed.to_json();
    let object = command
        .as_object_mut()
        .expect("parsed command is an object");
    object.insert("name".to_string(), json!(name));
    object.insert("agentSource".to_string(), json!(ZCODE_COMMAND_AGENT_SOURCE));
    let location = build_location(
        descriptor,
        &target.commands_root,
        target.scope,
        target.project_path.as_deref(),
    );
    object.insert("location".to_string(), Value::Object(location.clone()));
    object.insert(
        "id".to_string(),
        json!(build_command_id(target.scope, name, &location)),
    );
    object.insert("source".to_string(), json!("user"));
    object.insert("enabled".to_string(), json!(enabled));
    object.insert("scope".to_string(), json!(target.scope));
    if let Some(project_path) = &target.project_path {
        object.insert("projectPath".to_string(), json!(project_path));
    }
    let _ = file_path;
    Value::Object(object.clone())
}

/// `getCommandFileName`
fn command_file_name(raw_name: &str) -> String {
    let stripped = raw_name.strip_prefix('/').unwrap_or(raw_name);
    format!("{stripped}{COMMAND_FILE_EXTENSION}")
}

/// `getCommandName`
fn command_name(root_dir: &Path, file_path: &Path) -> String {
    let relative = file_path
        .strip_prefix(root_dir)
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|_| file_path.to_string_lossy().into_owned());
    let without_extension = crate::command_markdown::strip_markdown_extension(&relative);
    let joined = without_extension
        .split(['/', '\\'])
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    format!("/{joined}")
}

fn read_command_enabled_overrides(config: &Map<String, Value>) -> HashMap<String, bool> {
    let mut overrides = HashMap::new();
    let Some(command_config) = config.get("command").and_then(Value::as_object) else {
        return overrides;
    };
    for (file_path, entry) in command_config {
        if let Some(Value::Bool(enabled)) = entry.get(ENABLE_OVERRIDE_KEY) {
            overrides.insert(file_path.clone(), *enabled);
        }
    }
    overrides
}

/// `setCommandEnabledOverride`
fn apply_command_enabled_override(config: &mut Map<String, Value>, file_path: &str, enabled: bool) {
    let mut command_config = config
        .get("command")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if enabled {
        command_config.remove(file_path);
    } else {
        let mut entry = Map::new();
        entry.insert(ENABLE_OVERRIDE_KEY.to_string(), json!(false));
        command_config.insert(file_path.to_string(), Value::Object(entry));
    }
    if command_config.is_empty() {
        config.remove("command");
    } else {
        config.insert("command".to_string(), Value::Object(command_config));
    }
}

async fn write_cli_config_map(
    paths: &AppPaths,
    config: &Map<String, Value>,
) -> Result<(), ServiceError> {
    let path = cli_config_path(paths);
    let contents = format!(
        "{}\n",
        serde_json::to_string_pretty(&Value::Object(config.clone()))?
    );
    atomic_write_text(&path, &contents).await?;
    Ok(())
}

async fn file_path_exists(path: &Path) -> bool {
    tokio::fs::try_exists(path).await.unwrap_or(false)
}

async fn discover_user_commands_recursive(
    root_dir: &Path,
    current_dir: &Path,
    descriptor: &CommandDescriptor,
    scope: &str,
    project_path: Option<&str>,
    overrides: &HashMap<String, bool>,
    commands: &mut Vec<Map<String, Value>>,
) -> usize {
    let before = commands.len();
    let Ok(mut entries) = tokio::fs::read_dir(current_dir).await else {
        return 0;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let full_path = entry.path();
        // `lstat` semantics: a directory reached through a symlink is not a directory here.
        let Ok(metadata) = tokio::fs::symlink_metadata(&full_path).await else {
            continue;
        };
        if metadata.is_dir() {
            let _ = Box::pin(discover_user_commands_recursive(
                root_dir,
                &full_path,
                descriptor,
                scope,
                project_path,
                overrides,
                commands,
            ))
            .await;
            continue;
        }
        if !name.to_lowercase().ends_with(COMMAND_FILE_EXTENSION) {
            continue;
        }
        let Ok(content) = tokio::fs::read_to_string(&full_path).await else {
            continue;
        };
        let path_text = full_path.to_string_lossy().into_owned();
        let parsed = parse_command_file(&content, &path_text);
        let name = command_name(root_dir, &full_path);
        let location = build_location(descriptor, root_dir, scope, project_path);
        let mut command = parsed.to_json().as_object().cloned().unwrap_or_default();
        command.insert("name".to_string(), json!(name));
        command.insert("agentSource".to_string(), json!(ZCODE_COMMAND_AGENT_SOURCE));
        command.insert("location".to_string(), Value::Object(location.clone()));
        command.insert(
            "id".to_string(),
            json!(build_command_id(scope, &name, &location)),
        );
        command.insert("source".to_string(), json!("user"));
        command.insert(
            "enabled".to_string(),
            json!(*overrides.get(&path_text).unwrap_or(&true)),
        );
        command.insert("scope".to_string(), json!(scope));
        if let Some(project_path) = project_path {
            command.insert("projectPath".to_string(), json!(project_path));
        }
        commands.push(command);
    }
    commands.len() - before
}

/// `resolvePluginCommandRoots`
fn resolve_plugin_command_roots(
    manifest: &crate::plugins::PluginManifest,
    root_path: &Path,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for raw in parse_path_list(manifest.entries.get("commands")) {
        if let Some(resolved) = resolve_inside(root_path, &raw) {
            roots.push(resolved);
        }
    }
    if roots.is_empty() && manifest.entries.get("commands").is_none() {
        let default_root = root_path.join("commands");
        if std::fs::symlink_metadata(&default_root).is_ok() {
            roots.push(default_root);
        }
    }
    roots
}

async fn collect_plugin_commands_recursive(
    root_dir: &Path,
    current_dir: &Path,
    descriptor: &PluginRootDescriptor,
    overrides: &HashMap<String, bool>,
    seen: &mut HashSet<String>,
    commands: &mut Vec<PluginCommand>,
) {
    let Ok(mut entries) = tokio::fs::read_dir(current_dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let full_path = entry.path();
        let Ok(metadata) = tokio::fs::symlink_metadata(&full_path).await else {
            continue;
        };
        if metadata.is_dir() {
            Box::pin(collect_plugin_commands_recursive(
                root_dir, &full_path, descriptor, overrides, seen, commands,
            ))
            .await;
            continue;
        }
        if !name.to_lowercase().ends_with(COMMAND_FILE_EXTENSION) {
            continue;
        }
        let Ok(content) = tokio::fs::read_to_string(&full_path).await else {
            continue;
        };
        let path_text = full_path.to_string_lossy().into_owned();
        let parsed = parse_command_file(&content, &path_text);
        let command_name = command_name(root_dir, &full_path);
        let key = path_text.to_lowercase().replace('\\', "/");
        if !seen.insert(key) {
            continue;
        }
        // A single broken plugin file must not hide the plugin's other commands, so parse
        // failures above simply skip this entry.
        let mut command = parsed.to_json().as_object().cloned().unwrap_or_default();
        command.insert(
            "enabled".to_string(),
            json!(descriptor.plugin_enabled && *overrides.get(&path_text).unwrap_or(&true)),
        );
        command.insert("filePath".to_string(), json!(path_text));
        command.insert(
            "id".to_string(),
            json!(build_plugin_command_id(
                &path_text,
                &command_name,
                &descriptor.plugin_marketplace,
                &descriptor.plugin_name
            )),
        );
        command.insert("name".to_string(), json!(command_name));
        command.insert(
            "pluginEnabled".to_string(),
            json!(descriptor.plugin_enabled),
        );
        command.insert(
            "pluginMarketplace".to_string(),
            json!(descriptor.plugin_marketplace),
        );
        command.insert("pluginName".to_string(), json!(descriptor.plugin_name));
        command.insert("scope".to_string(), json!("global"));
        command.insert("source".to_string(), json!("plugin"));
        commands.push(PluginCommand {
            name: command_name,
            value: Value::Object(command),
        });
    }
}

/// `dedupeCommandsByName`: first occurrence wins, insertion order preserved.
fn dedupe_commands_by_name(commands: Vec<Map<String, Value>>) -> Vec<Value> {
    let mut selected: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for command in commands {
        let name = command
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if seen.insert(name) {
            selected.push(Value::Object(command));
        }
    }
    selected
}

#[derive(Debug, Clone, Default)]
struct ListParams {
    agent_source: Option<String>,
    workspace_path: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct CommandFileParams {
    config: CommandConfig,
    agent_source: Option<String>,
    storage_level: String,
    workspace_path: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct UpdateParams {
    config: CommandConfig,
    agent_source: Option<String>,
    old_file_path: Option<String>,
    storage_level: String,
    workspace_path: Option<String>,
}

fn read_command_config(value: &Value) -> Result<CommandConfig, ServiceError> {
    let object = value
        .as_object()
        .ok_or_else(|| ServiceError::invalid_params("params.config must be an object"))?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ServiceError::invalid_params("params.config.name must be a string"))?;
    let prompt = object
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or_else(|| ServiceError::invalid_params("params.config.prompt must be a string"))?;
    Ok(CommandConfig {
        name: name.to_string(),
        prompt: prompt.to_string(),
        description: object
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        argument_hint: object
            .get("argumentHint")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn optional_str(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_string)
}

#[async_trait::async_trait]
impl Channel for CommandsService {
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
                let object = params.as_object().cloned().unwrap_or_default();
                let result = self
                    .list(&ListParams {
                        agent_source: optional_str(&object, "agentSource"),
                        workspace_path: optional_str(&object, "workspacePath"),
                    })
                    .await?;
                Ok(WireValue::Json(result))
            }
            "writeCommandFile" => {
                let params = args.required_json(0, "params")?;
                let object = params.as_object().cloned().unwrap_or_default();
                let config_value = object
                    .get("config")
                    .ok_or_else(|| ServiceError::invalid_params("params.config is required"))?;
                let file_params = CommandFileParams {
                    config: read_command_config(config_value)?,
                    agent_source: optional_str(&object, "agentSource"),
                    storage_level: optional_str(&object, "storageLevel").unwrap_or_default(),
                    workspace_path: optional_str(&object, "workspacePath"),
                };
                let command = self.write_command_file(&file_params).await?;
                Ok(WireValue::Json(json!({ "command": command })))
            }
            "updateCommandFile" => {
                let params = args.required_json(0, "params")?;
                let object = params.as_object().cloned().unwrap_or_default();
                let config_value = object
                    .get("config")
                    .ok_or_else(|| ServiceError::invalid_params("params.config is required"))?;
                let update = UpdateParams {
                    config: read_command_config(config_value)?,
                    agent_source: optional_str(&object, "agentSource"),
                    old_file_path: optional_str(&object, "oldFilePath"),
                    storage_level: optional_str(&object, "storageLevel").unwrap_or_default(),
                    workspace_path: optional_str(&object, "workspacePath"),
                };
                let command = self.update_command_file(&update).await?;
                Ok(WireValue::Json(json!({ "command": command })))
            }
            "deleteCommandFile" => {
                let params = args.required_json(0, "params")?;
                let file_path =
                    params
                        .get("filePath")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            ServiceError::invalid_params("params.filePath must be a string")
                        })?;
                self.delete_command_file(file_path).await?;
                Ok(WireValue::Undefined)
            }
            "setCommandEnabled" => {
                let params = args.required_json(0, "params")?;
                let file_path =
                    params
                        .get("filePath")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            ServiceError::invalid_params("params.filePath must be a string")
                        })?;
                let enabled = params
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| {
                        ServiceError::invalid_params("params.enabled must be a boolean")
                    })?;
                self.set_command_enabled_override(file_path, enabled)
                    .await?;
                Ok(WireValue::Undefined)
            }
            "getPrimaryUserCommandsDirectory" => {
                let params = args.json(0)?.unwrap_or(Value::Null);
                let agent_source = match &params {
                    Value::Object(object) => optional_str(object, "agentSource"),
                    _ => None,
                };
                Ok(WireValue::Json(
                    self.get_primary_user_commands_directory(agent_source.as_deref())
                        .await?,
                ))
            }
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(tag: &str) -> (AppPaths, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "zcode-commands-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(root.join("home")).unwrap();
        (AppPaths::new(root.join("home"), root.join("data")), root)
    }

    fn config(name: &str, prompt: &str) -> CommandConfig {
        CommandConfig {
            name: name.to_string(),
            prompt: prompt.to_string(),
            description: Some("says things".to_string()),
            argument_hint: None,
        }
    }

    #[tokio::test]
    async fn write_list_disable_rename_delete_round_trip() {
        let (paths, root) = temp_paths("round-trip");
        let service = CommandsService::new(paths.clone());
        let written = service
            .write_command_file(&CommandFileParams {
                config: config("/greet", "hello world"),
                storage_level: String::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let file_path =
            PathBuf::from(written["location"]["directoryPath"].as_str().unwrap()).join("greet.md");
        assert!(file_path.exists());
        assert_eq!(written["name"], json!("/greet"));
        assert_eq!(written["enabled"], json!(true));
        assert_eq!(written["id"], json!("zcodeAgent:zcode:global:/greet"));
        assert_eq!(
            std::fs::read_to_string(&file_path).unwrap(),
            "---\ndescription: says things\n---\n\nhello world"
        );

        // Duplicate creation is rejected by file name, like TypeScript.
        let error = service
            .write_command_file(&CommandFileParams {
                config: config("/greet", "again"),
                storage_level: String::new(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(error.message, "Command file already exists: greet.md");

        let listed = service
            .list(&ListParams {
                workspace_path: None,
                agent_source: None,
            })
            .await
            .unwrap();
        assert_eq!(listed["userCommands"].as_array().unwrap().len(), 1);
        assert_eq!(listed["commands"].as_array().unwrap().len(), 1);

        let path_text = file_path.to_string_lossy().into_owned();
        service
            .set_command_enabled_override(&path_text, false)
            .await
            .unwrap();
        let listed = service.list(&ListParams::default()).await.unwrap();
        assert_eq!(listed["userCommands"][0]["enabled"], json!(false));
        // The flag lives in the CLI config, keyed by the absolute command file path.
        let stored = read_cli_config(&paths).await;
        assert_eq!(
            stored
                .get("command")
                .and_then(Value::as_object)
                .and_then(|command| command.get(&path_text))
                .map(|entry| entry["enable"].clone()),
            Some(json!(false)),
            "{stored:?}"
        );

        // Renaming moves the file and carries the disabled flag to the new path.
        let updated = service
            .update_command_file(&UpdateParams {
                config: config("/hello", "renamed body"),
                old_file_path: Some(path_text.clone()),
                storage_level: String::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!file_path.exists());
        let new_path = updated["filePath"].as_str().unwrap().to_string();
        assert!(PathBuf::from(&new_path).exists());
        assert_eq!(updated["enabled"], json!(false));

        service.delete_command_file(&new_path).await.unwrap();
        assert!(!PathBuf::from(&new_path).exists());
        let stored = read_cli_config(&paths).await;
        assert!(!stored.contains_key("command"), "{stored:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn project_scope_requires_a_workspace_path_and_beats_user_scope() {
        let (paths, root) = temp_paths("scope");
        let service = CommandsService::new(paths.clone());
        let error = service
            .write_command_file(&CommandFileParams {
                config: config("/p", "x"),
                storage_level: "project".to_string(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(error.message, "Missing workspace path for project command");

        let workspace = root.join("ws");
        std::fs::create_dir_all(workspace.join(".zcode").join("commands")).unwrap();
        std::fs::write(
            workspace.join(".zcode").join("commands").join("local.md"),
            "---\ndescription: project level\n---\n\nbody",
        )
        .unwrap();
        std::fs::create_dir_all(workspace.join(".agents").join("commands")).unwrap();
        std::fs::write(
            workspace.join(".agents").join("commands").join("legacy.md"),
            "legacy",
        )
        .unwrap();

        let listed = service
            .list(&ListParams {
                workspace_path: Some(workspace.to_string_lossy().into_owned()),
                agent_source: None,
            })
            .await
            .unwrap();
        let names: Vec<&str> = listed["userCommands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["/local"]);
        assert_eq!(listed["userCommands"][0]["scope"], json!("project"));
        assert_eq!(
            listed["userCommands"][0]["projectPath"].as_str().unwrap(),
            workspace.to_string_lossy().as_ref()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn agents_root_is_used_when_nc_code_root_is_empty() {
        let (paths, root) = temp_paths("agents-fallback");
        let home = paths.home_dir().to_path_buf();
        std::fs::create_dir_all(home.join(".agents").join("commands")).unwrap();
        std::fs::write(home.join(".agents").join("commands").join("a.md"), "prompt").unwrap();
        let service = CommandsService::new(paths);
        let listed = service.list(&ListParams::default()).await.unwrap();
        assert_eq!(listed["userCommands"][0]["name"], json!("/a"));
        assert_eq!(
            listed["userCommands"][0]["location"]["source"],
            json!("agents")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn unknown_agent_source_is_rejected_not_silently_defaulted() {
        let (paths, root) = temp_paths("bad-source");
        let service = CommandsService::new(paths);
        let error = service
            .get_primary_user_commands_directory(Some("claudeCode"))
            .await
            .unwrap_err();
        assert_eq!(error.name, "ZodError");
        assert!(error.message.contains("claudeCode"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn override_helpers_match_the_typescript_shape() {
        let mut config = Map::new();
        apply_command_enabled_override(&mut config, "/x/a.md", false);
        assert_eq!(
            config["command"]["/x/a.md"],
            json!({"enable": false}),
            "{config:?}"
        );
        assert_eq!(read_command_enabled_overrides(&config).len(), 1);
        apply_command_enabled_override(&mut config, "/x/a.md", true);
        assert!(!config.contains_key("command"));
        assert!(read_command_enabled_overrides(&config).is_empty());
    }

    #[test]
    fn names_and_file_names_follow_the_typescript_rules() {
        assert_eq!(command_file_name("/sub/nested"), "sub/nested.md");
        assert_eq!(command_file_name("no-slash"), "no-slash.md");
        assert_eq!(
            command_name(
                Path::new("/root/commands"),
                Path::new("/root/commands/sub/Deep.MD")
            ),
            "/sub/Deep"
        );
    }
}
