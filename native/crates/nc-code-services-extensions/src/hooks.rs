//! The `hooks` channel — a port of TS `packages/services/src/hooks/{hooks,workspaceHookSettingsModel}.ts`.
//!
//! Layering: the domain side fully reuses `nc-code-host::workspace_hooks` (config discovery/parsing, bundle digest,
//! trust store) —— this file does only the three things the TS service layer does: **project** the discovery result into the UI's `Hook[]`,
//! write editable hooks back to the two configs by storageLevel, and persist pre-trust into the trust store
//! **via the canonical snapshot re-discovered** (the TS comments explicitly forbid service/UI from writing the store directly).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Map, Value};

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_host::store::now_ms;
use nc_code_host::workspace_hooks::{
    build_bundle, create_trust_records, read_sources, resolve_entries, resolve_runtime_root,
    rfc3339_utc, BundleSnapshot, CanonicalEntry, ConfigFileKind, EntryType, HooksConfig, RootGates,
    ShellConfig, SourceInput, TrustStore, TrustStoreStatus, TRUST_STORE_FILE,
};
use nc_code_rpc::WireValue;
use nc_code_service_core::atomic::atomic_write_text;
use nc_code_service_core::{AppPaths, ServiceError};

/// `HOOK_EVENTS` (hooksService.ts:30-38).
const HOOK_EVENTS: [&str; 7] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PostToolUseFailure",
    "Stop",
];

pub struct HooksService {
    paths: AppPaths,
}

/// The return of discover: (sources, snapshot, trusted digests, trust corrupt).
type DiscoverResult = (
    Vec<SourceInput>,
    Option<BundleSnapshot>,
    HashSet<String>,
    bool,
);

impl HooksService {
    pub fn new(paths: AppPaths) -> Arc<Self> {
        Arc::new(Self { paths })
    }

    /// `workspaceKey` = `identity?.trim() || workspacePath`（AGENTS.md Workspace Identity）。
    fn target(params: &Value) -> Result<(String, String), ServiceError> {
        let workspace_path = params
            .get("workspacePath")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ServiceError::invalid_params("workspacePath is required"))?
            .to_string();
        let identity = params
            .get("workspaceIdentity")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let key = identity.unwrap_or(&workspace_path).to_string();
        Ok((workspace_path, key))
    }

    /// The trust store is the same file the runtime uses: host builds the store under that workspace's data directory
    /// (nc-code-host/src/lib.rs:149 `data_dir/security/workspace-hook-trust-v1.json`).
    /// On the TS side `loadHooks` reads the same-named file at home level (hooksService.ts:169) —— the paths differ, but
    /// **display and runtime must agree** is higher-priority correctness, so we follow host; the difference is recorded.
    fn trust_path(&self, workspace_path: &str, workspace_key: &str) -> PathBuf {
        self.paths
            .app_config_dir()
            .join("agent")
            .join(
                self.paths
                    .workspace_hash(workspace_path, Some(workspace_key)),
            )
            .join("security")
            .join(TRUST_STORE_FILE)
    }

    fn project_config_path(workspace_path: &str) -> PathBuf {
        Path::new(workspace_path).join(".zcode").join("config.json")
    }

    fn user_config_path(&self) -> PathBuf {
        self.paths
            .home_dir()
            .join(".zcode")
            .join("cli")
            .join("config.json")
    }

    /// The discovery trio: source reads (a broken config = a whole-read error, TS rejects the entire load),
    /// runtime root-gate merging, and the canonical bundle snapshot (assembled like admission.rs:169-180).
    fn discover(
        &self,
        workspace_path: &str,
        workspace_key: &str,
    ) -> Result<DiscoverResult, ServiceError> {
        let workspace = Path::new(workspace_path);
        let (sources, errors) = read_sources(workspace, None);
        if let Some(error) = errors.first() {
            // On the TS side this error throws the whole load (fail-closed); it is not degraded into an empty list.
            return Err(ServiceError::named(
                "ZCodeWorkspaceHooksConfigUnreadable",
                format!("{}: {}", error.path.display(), error.message),
            ));
        }
        let roots: Vec<RootGates> = sources
            .iter()
            .map(|source| RootGates {
                enabled: source.hooks.enabled,
                timeout_ms: source.hooks.timeout_ms,
                max_output_bytes: source.hooks.max_output_bytes,
            })
            .collect();
        let runtime = resolve_runtime_root(&roots);
        let snapshot = build_bundle(
            workspace_key,
            workspace,
            &sources,
            &runtime,
            &rfc3339_utc(now_ms()),
        );
        let trust = TrustStore::new(self.trust_path(workspace_path, workspace_key));
        let (status, records) = trust.records();
        let trusted: HashSet<String> = records
            .into_iter()
            .filter(|record| record.workspace_identity == workspace_key)
            .map(|record| record.hook_declaration_digest)
            .collect();
        let corrupt = matches!(status, TrustStoreStatus::Corrupt);
        Ok((sources, snapshot, trusted, corrupt))
    }

    /// `loadHooks` (hooksService.ts:214-258): project bundle → 4 legacy groups → user zcode,
    /// in the same order as TS (the UI list order depends on it).
    pub async fn load(&self, params: &Value) -> Result<Value, ServiceError> {
        let (workspace_path, workspace_key) = Self::target(params)?;
        let (sources, snapshot, trusted, corrupt) =
            self.discover(&workspace_path, &workspace_key)?;

        let mut hooks: Vec<Value> = Vec::new();
        if let Some(snapshot) = snapshot.as_ref() {
            let bundle_digest = snapshot.bundle_digest.clone();
            for (index, entry) in snapshot.hooks.iter().enumerate() {
                let source = sources.get(entry.source_file_index).ok_or_else(|| {
                    ServiceError::new(format!(
                        "Workspace Hook source is missing for {}",
                        entry.review_item_id
                    ))
                })?;
                let trust_state = if trusted.contains(&entry.hook_declaration_digest) {
                    "trusted_persistent"
                } else {
                    "pending_trust"
                };
                let workspace_hook = json!({
                    "workspaceIdentity": workspace_key,
                    "bundleDigest": bundle_digest,
                    "hookDeclarationDigest": entry.hook_declaration_digest,
                    "trustState": trust_state,
                    "reviewItemId": entry.review_item_id,
                    "sourceFileIndex": entry.source_file_index,
                    "sourceRootEnabled": entry.source_root_enabled,
                    "declarationEnabled": entry.declaration_enabled,
                    "runtimeHooksEnabled": entry.runtime_hooks_enabled,
                    "configuredEnabled": entry.configured_enabled,
                    "sourcePath": source.canonical_path.to_string_lossy(),
                });
                let _ = index;
                hooks.push(hook_from_entry(
                    entry,
                    source,
                    &entry.review_item_id,
                    project_location(&source.canonical_path, &workspace_path),
                    Some(workspace_hook),
                ));
            }
        }
        for (source, scope) in [("agents", "project"), ("claude", "project")] {
            hooks.extend(legacy_hooks(
                &Path::new(&workspace_path)
                    .join(root_dir_name(source))
                    .join(SETTINGS_FILE),
                source,
                scope,
                self.location_for(source, Some(&workspace_path)),
            ));
        }
        hooks.extend(
            self.user_nc_code_hooks(&workspace_path, &workspace_key)
                .await?,
        );
        for (source, scope) in [("agents", "user"), ("claude", "user")] {
            hooks.extend(legacy_hooks(
                &self
                    .paths
                    .home_dir()
                    .join(root_dir_name(source))
                    .join(SETTINGS_FILE),
                source,
                scope,
                self.location_for(source, None),
            ));
        }
        let hooks_enabled = hooks
            .iter()
            .any(|hook| hook.get("enabled").and_then(Value::as_bool) == Some(true));
        let mut result = Map::new();
        result.insert("hooks".to_string(), Value::Array(hooks));
        result.insert("hooksEnabled".to_string(), json!(hooks_enabled));
        if let Some(snapshot) = snapshot.as_ref() {
            result.insert("workspaceHookSnapshot".to_string(), snapshot.to_json());
        }
        if corrupt {
            result.insert("trustStoreCorrupt".to_string(), json!(true));
        }
        Ok(Value::Object(result))
    }

    /// The user-level zcode hook (TS fromUserZCodeSource): unrelated to the bundle, it parses the hooks
    /// section of `~/.zcode/cli/config.json` separately and resolves each entry once more.
    async fn user_nc_code_hooks(
        &self,
        workspace_path: &str,
        workspace_key: &str,
    ) -> Result<Vec<Value>, ServiceError> {
        let path = self.user_config_path();
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(_) => return Ok(Vec::new()), // missing/unreadable = no user declarations (TS null → [])
        };
        let value: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(_) => return Ok(Vec::new()),
        };
        let Some(hooks_value) = value.get("hooks") else {
            return Ok(Vec::new());
        };
        let hooks_config = match HooksConfig::parse(hooks_value) {
            Ok(config) => config,
            Err(_) => return Ok(Vec::new()),
        };
        let source = SourceInput {
            canonical_path: path.clone(),
            base_dir: path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.paths.home_dir().to_path_buf()),
            discovery_order: 0,
            config_file_kind: ConfigFileKind::ZcodeConfigJson,
            explicit_project_config: false,
            editable: true,
            hooks: hooks_config,
        };
        let roots: Vec<RootGates> = vec![RootGates {
            enabled: source.hooks.enabled,
            timeout_ms: source.hooks.timeout_ms,
            max_output_bytes: source.hooks.max_output_bytes,
        }];
        let runtime = resolve_runtime_root(&roots);
        let sources = [source];
        let entries = resolve_entries(Path::new(workspace_path), &sources, &runtime);
        let _ = workspace_key;
        Ok(entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                hook_from_entry(
                    entry,
                    &sources[0],
                    &format!("hook-nc-code-user-{index}"),
                    self.location_for("zcode", None),
                    None,
                )
            })
            .collect())
    }

    /// SettingsDirectoryLocation (hooksService.ts:71-81): the user's directoryPath is expanded from home.
    fn location_for(&self, source: &str, workspace_path: Option<&str>) -> Value {
        let (scope, directory) = match (source, workspace_path) {
            ("zcode", Some(ws)) => ("project", Path::new(ws).join(".zcode")),
            ("zcode", None) => ("user", self.paths.home_dir().join(".zcode").join("cli")),
            (_, Some(ws)) => ("project", Path::new(ws).join(root_dir_name(source))),
            (_, None) => ("user", self.paths.home_dir().join(root_dir_name(source))),
        };
        let mut location = Map::new();
        location.insert("source".into(), json!(source));
        location.insert("scope".into(), json!(scope));
        location.insert("directoryPath".into(), json!(directory.to_string_lossy()));
        if let Some(ws) = workspace_path {
            location.insert("projectPath".into(), json!(ws));
        }
        Value::Object(location)
    }

    /// `saveHooks` (hooksService.ts:287-308): filter editable hooks by location and write the user/project
    /// zcode config separately (merge the existing config, changing only hooks.enabled and hooks.events).
    pub async fn save(&self, params: &Value) -> Result<(), ServiceError> {
        let (workspace_path, _workspace_key) = Self::target(params)?;
        let hooks = params
            .get("hooks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let project_config = Self::project_config_path(&workspace_path);
        let editable = |hook: &Value| hook.get("editable").and_then(Value::as_bool) != Some(false);
        let source_is = |hook: &Value, name: &str| {
            hook.get("location")
                .and_then(|loc| loc.get("source"))
                .and_then(Value::as_str)
                == Some(name)
        };
        let scope_is = |hook: &Value, scope: &str| {
            hook.get("location")
                .and_then(|loc| loc.get("scope"))
                .and_then(Value::as_str)
                == Some(scope)
        };
        let user_hooks: Vec<Value> = hooks
            .iter()
            .filter(|hook| editable(hook) && source_is(hook, "zcode") && scope_is(hook, "user"))
            .cloned()
            .collect();
        let project_hooks: Vec<Value> = hooks
            .iter()
            .filter(|hook| {
                if !(editable(hook) && source_is(hook, "zcode") && scope_is(hook, "project")) {
                    return false;
                }
                // configuredState.sourcePath must equal the current project config (guards against writing the wrong file).
                hook.get("configuredState")
                    .and_then(|state| state.get("sourcePath"))
                    .and_then(Value::as_str)
                    .map(|path| Path::new(path) == project_config.as_path())
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        self.write_nc_code_hooks_config(None, &user_hooks).await?;
        self.write_nc_code_hooks_config(Some(&workspace_path), &project_hooks)
            .await?;
        Ok(())
    }

    /// Write editable hooks back into the zcode config (hooksService.ts:270-285): merge the existing config,
    /// overriding only hooks.enabled and hooks.events.
    async fn write_nc_code_hooks_config(
        &self,
        workspace_path: Option<&str>,
        hooks: &[Value],
    ) -> Result<(), ServiceError> {
        let path = match workspace_path {
            Some(ws) => Self::project_config_path(ws),
            None => self.user_config_path(),
        };
        let existing: Value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_else(|| json!({}));
        let existing_enabled = existing
            .get("hooks")
            .and_then(|hooks| hooks.get("enabled"))
            .and_then(Value::as_bool);
        let enabled = resolve_next_root_enabled(existing_enabled, hooks);
        let events = to_nc_code_hooks_events(hooks);
        let mut hooks_section = existing
            .get("hooks")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_else(Map::new);
        if let Some(enabled) = enabled {
            hooks_section.insert("enabled".into(), json!(enabled));
        }
        hooks_section.insert("events".into(), events);
        let mut root = existing.as_object().cloned().unwrap_or_else(Map::new);
        root.insert("hooks".into(), Value::Object(hooks_section));
        let text = format!("{}\n", serde_json::to_string_pretty(&Value::Object(root))?);
        atomic_write_text(&path, &text)
            .await
            .map_err(|error| ServiceError::new(format!("write hooks config: {error}")))
    }

    /// `grantWorkspaceHookTrust` (the authority logic injected at hooksService.ts:31-36): it must rediscover the
    /// canonical snapshot (service/UI writing the store directly is forbidden), then persist trust after verifying the bundle/declaration digests match.
    pub async fn grant(&self, params: &Value) -> Result<Value, ServiceError> {
        let (workspace_path, workspace_key) = Self::target(params)?;
        let bundle_digest = params
            .get("bundleDigest")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let declaration_digest = params
            .get("hookDeclarationDigest")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let (sources, snapshot, _trusted, corrupt) =
            self.discover(&workspace_path, &workspace_key)?;
        if corrupt {
            return Ok(grant_result(
                false,
                Some("workspace_hooks_trust_store_corrupt"),
            ));
        }
        let Some(snapshot) = snapshot else {
            return Ok(grant_result(
                false,
                Some("workspace_hooks_snapshot_mismatch"),
            ));
        };
        // The bundle digest must match what the UI read (guards against TOCTOU: the snapshot changing between read and grant).
        if snapshot.bundle_digest != bundle_digest {
            return Ok(grant_result(false, Some("workspace_hooks_bundle_changed")));
        }
        // The declaration digest must be present in the current snapshot (the rediscovered canonical set is the only authority).
        let Some(entry) = snapshot
            .hooks
            .iter()
            .find(|entry| entry.hook_declaration_digest == declaration_digest)
        else {
            return Ok(grant_result(
                false,
                Some("workspace_hooks_snapshot_mismatch"),
            ));
        };
        let _ = (&sources, entry);
        let records = create_trust_records(
            &snapshot.hooks,
            std::slice::from_ref(&declaration_digest),
            &workspace_key,
            &snapshot.bundle_digest,
            &rfc3339_utc(now_ms()),
            None,
        )
        .map_err(ServiceError::new)?;
        let trust = TrustStore::new(self.trust_path(&workspace_path, &workspace_key));
        trust.load();
        trust.upsert(records).map_err(|error| {
            ServiceError::new(format!("trust store write failed: {}", error.message))
        })?;
        Ok(grant_result(true, None))
    }
}

fn grant_result(accepted: bool, reason: Option<&str>) -> Value {
    let mut result = Map::new();
    result.insert("accepted".into(), json!(accepted));
    if let Some(reason) = reason {
        result.insert("reasonCode".into(), json!(reason));
    }
    Value::Object(result)
}

/// SETTINGS_FILE（hooksService.ts:34）。
const SETTINGS_FILE: &str = "settings.json";

/// The root directory name (getRootDir: agents→.agents, claude→.claude).
fn root_dir_name(source: &str) -> &str {
    if source == "agents" {
        ".agents"
    } else {
        ".claude"
    }
}

fn shell_to_json(shell: &ShellConfig) -> Value {
    match shell {
        ShellConfig::Enabled(flag) => json!(flag),
        ShellConfig::Script(script) => json!(script),
    }
}

/// HookConfiguredState（toConfiguredState，workspaceHookSettingsModel.ts:102-113）。
fn configured_state(entry: &CanonicalEntry, source: &SourceInput) -> Value {
    json!({
        "sourceRootEnabled": entry.source_root_enabled,
        "declarationEnabled": entry.declaration_enabled,
        "runtimeHooksEnabled": entry.runtime_hooks_enabled,
        "configuredEnabled": entry.configured_enabled,
        "sourcePath": source.canonical_path.to_string_lossy(),
    })
}

/// canonical entry → the UI's Hook JSON (mirrors toHook, workspaceHookSettingsModel.ts:115-159).
/// timeout takes resolved_timeout_ms/1000 (parity decision: host parses strictly, so the raw timeout is unavailable).
fn hook_from_entry(
    entry: &CanonicalEntry,
    source: &SourceInput,
    id: &str,
    location: Value,
    workspace_hook: Option<Value>,
) -> Value {
    let mut hook = Map::new();
    hook.insert("id".into(), json!(id));
    hook.insert("event".into(), json!(entry.event.as_str()));
    if let Some(matcher) = &entry.matcher {
        hook.insert("matcher".into(), json!(matcher));
    }
    hook.insert("type".into(), json!(entry.entry_type.as_str()));
    hook.insert("command".into(), json!(entry.command));
    match entry.entry_type {
        EntryType::Process => {
            if let Some(args) = &entry.args {
                hook.insert("args".into(), json!(args));
            }
        }
        EntryType::Command => {
            if let Some(asynchronous) = entry.asynchronous {
                hook.insert("async".into(), json!(asynchronous));
            }
            if let Some(shell) = &entry.shell {
                hook.insert("shell".into(), shell_to_json(shell));
            }
        }
    }
    if let Some(status_message) = &entry.status_message {
        hook.insert("statusMessage".into(), json!(status_message));
    }
    hook.insert(
        "timeout".into(),
        json!(entry.resolved_timeout_ms as f64 / 1000.0),
    );
    hook.insert("enabled".into(), json!(entry.configured_enabled));
    hook.insert("editable".into(), json!(entry.editable));
    hook.insert("configuredState".into(), configured_state(entry, source));
    if let Some(workspace_hook) = workspace_hook {
        hook.insert("workspaceHook".into(), workspace_hook);
    }
    hook.insert("location".into(), location);
    Value::Object(hook)
}

/// legacy settings.json → Hook[] (mirrors fromLegacyHooksConfig, workspaceHookSettingsModel.ts:216-249).
fn legacy_hooks(path: &Path, source: &str, scope: &str, location: Value) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(config) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let Some(events) = config.get("hooks").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut hooks = Vec::new();
    let mut counter = 0usize;
    for (event_name, matchers) in events {
        if !HOOK_EVENTS.contains(&event_name.as_str()) {
            continue;
        }
        let Some(matchers) = matchers.as_array() else {
            continue;
        };
        for matcher in matchers {
            let matcher_text = matcher
                .get("matcher")
                .and_then(Value::as_str)
                .map(str::to_string);
            let Some(definitions) = matcher.get("hooks").and_then(Value::as_array) else {
                continue;
            };
            for definition in definitions {
                let Some(hook_type) = definition.get("type").and_then(Value::as_str) else {
                    continue;
                };
                if hook_type != "command" && hook_type != "process" {
                    continue;
                }
                let Some(command) = definition.get("command").and_then(Value::as_str) else {
                    continue;
                };
                let mut hook = Map::new();
                hook.insert(
                    "id".into(),
                    json!(format!("hook-{source}-{scope}-{counter}")),
                );
                counter += 1;
                hook.insert("event".into(), json!(event_name));
                if let Some(matcher) = &matcher_text {
                    hook.insert("matcher".into(), json!(matcher));
                }
                hook.insert("type".into(), json!(hook_type));
                hook.insert("command".into(), json!(command));
                if hook_type == "process" {
                    if let Some(args) = definition.get("args").and_then(Value::as_array) {
                        hook.insert("args".into(), json!(args));
                    }
                } else {
                    if let Some(asynchronous) = definition.get("async").and_then(Value::as_bool) {
                        hook.insert("async".into(), json!(asynchronous));
                    }
                    if let Some(shell) = definition.get("shell") {
                        hook.insert("shell".into(), shell.clone());
                    }
                }
                if let Some(status) = definition.get("statusMessage").and_then(Value::as_str) {
                    hook.insert("statusMessage".into(), json!(status));
                }
                let timeout = match hook_type {
                    "command" => definition.get("timeout").and_then(Value::as_f64),
                    _ => definition
                        .get("timeoutMs")
                        .and_then(Value::as_f64)
                        .map(|ms| (ms / 1000.0).round()),
                };
                if let Some(timeout) = timeout {
                    hook.insert("timeout".into(), json!(timeout));
                }
                // A legacy hook is always non-editable and off by default (TS fromLegacyHooksConfig:241-242).
                hook.insert("enabled".into(), json!(false));
                hook.insert("editable".into(), json!(false));
                hook.insert("location".into(), location.clone());
                hooks.push(Value::Object(hook));
            }
        }
    }
    hooks
}

/// The location for the project scope (project_location).
fn project_location(canonical_path: &Path, workspace_path: &str) -> Value {
    json!({
        "source": "zcode",
        "scope": "project",
        "directoryPath": canonical_path.parent().map(|p| p.to_string_lossy()).unwrap_or_default(),
        "projectPath": workspace_path,
    })
}

/// toZCodeHooksEvents (workspaceHookSettingsModel.ts:251-267): group by event+matcher.
fn to_nc_code_hooks_events(hooks: &[Value]) -> Value {
    let mut events: Map<String, Value> = Map::new();
    for hook in hooks {
        let Some(event) = hook.get("event").and_then(Value::as_str) else {
            continue;
        };
        let matchers = events
            .entry(event.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let list = matchers.as_array_mut().expect("matchers array");
        let matcher_text = hook
            .get("matcher")
            .and_then(Value::as_str)
            .map(str::to_string);
        let existing = list
            .iter_mut()
            .find(|item| item.get("matcher").and_then(Value::as_str) == matcher_text.as_deref());
        let matcher = match existing {
            Some(matcher) => matcher,
            None => {
                let mut matcher = Map::new();
                if let Some(matcher_text) = &matcher_text {
                    matcher.insert("matcher".into(), json!(matcher_text));
                }
                matcher.insert("hooks".into(), Value::Array(Vec::new()));
                list.push(Value::Object(matcher));
                list.last_mut().expect("pushed matcher")
            }
        };
        let definition_hooks = matcher
            .as_object_mut()
            .expect("matcher object")
            .entry("hooks".to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        definition_hooks
            .as_array_mut()
            .expect("hooks array")
            .push(writable_hook(hook));
    }
    Value::Object(events)
}

/// getWritableHook + resolveWritableDeclarationEnabled（workspaceHookSettingsModel.ts:62-91）。
fn writable_hook(hook: &Value) -> Value {
    let configured = hook.get("configuredState");
    let enabled = hook
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let declaration_enabled = match configured {
        None => enabled,
        Some(state) => {
            let configured_enabled = state
                .get("configuredEnabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if enabled == configured_enabled {
                state
                    .get("declarationEnabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(enabled)
            } else {
                enabled
            }
        }
    };
    let hook_type = hook
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("command");
    let mut definition = Map::new();
    definition.insert("type".into(), json!(hook_type));
    definition.insert(
        "command".into(),
        hook.get("command").cloned().unwrap_or(Value::Null),
    );
    definition.insert("enabled".into(), json!(declaration_enabled));
    if let Some(status) = hook.get("statusMessage") {
        definition.insert("statusMessage".into(), status.clone());
    }
    if hook_type == "command" {
        if hook.get("async").and_then(Value::as_bool) == Some(true) {
            definition.insert("async".into(), json!(true));
        }
        if let Some(shell) = hook.get("shell") {
            definition.insert("shell".into(), shell.clone());
        }
        if let Some(timeout) = hook.get("timeout") {
            definition.insert("timeout".into(), timeout.clone());
        }
    } else {
        if let Some(args) = hook.get("args").and_then(Value::as_array) {
            if !args.is_empty() {
                definition.insert("args".into(), json!(args));
            }
        }
        if let Some(timeout) = hook.get("timeout").and_then(Value::as_f64) {
            definition.insert("timeoutMs".into(), json!(timeout * 1000.0));
        }
    }
    Value::Object(definition)
}

/// resolveNextRootEnabled (workspaceHookSettingsModel.ts:269-282): set the root switch to true when any hook is turned on.
fn resolve_next_root_enabled(existing: Option<bool>, hooks: &[Value]) -> Option<bool> {
    let any_enabled = hooks.iter().any(|hook| {
        hook.get("enabled").and_then(Value::as_bool) != Some(true)
            || hook.get("configuredState").is_none()
    });
    if any_enabled {
        Some(true)
    } else {
        existing
    }
}

#[async_trait]
impl Channel for HooksService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let params = match args.into_iter().next() {
            Some(WireValue::Json(value)) => value,
            _ => Value::Object(Map::new()),
        };
        let to_failure = |error: ServiceError| ChannelFailure::error(error.to_string());
        match command {
            "loadHooks" => self
                .load(&params)
                .await
                .map(WireValue::Json)
                .map_err(to_failure),
            "saveHooks" => self
                .save(&params)
                .await
                .map(|()| WireValue::Undefined)
                .map_err(to_failure),
            "grantWorkspaceHookTrust" => self
                .grant(&params)
                .await
                .map(WireValue::Json)
                .map_err(to_failure),
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;

    fn temp_workspace() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zcode-hooks-e2e-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(dir.join(".zcode")).expect("mkdir");
        dir
    }

    fn temp_paths() -> AppPaths {
        let root = std::env::temp_dir().join(format!("zcode-hooks-paths-{}", std::process::id()));
        AppPaths::new(root.join("home"), root.join("data"))
    }

    #[tokio::test]
    async fn load_hooks_projects_workspace_hook_and_reports_enabled() {
        let workspace = temp_workspace();
        let config = json!({
            "hooks": {
                "enabled": true,
                "events": {
                    "PreToolUse": [
                        {"hooks": [{"type": "command", "command": "echo hi", "timeout": 5}]}
                    ],
                },
            },
        });
        std::fs::write(
            workspace.join(".zcode").join("config.json"),
            config.to_string(),
        )
        .expect("write config");
        let service = HooksService::new(temp_paths());
        let params = json!({"workspacePath": workspace.to_string_lossy()});
        let result = service.load(&params).await.expect("loadHooks");
        let hooks = result
            .get("hooks")
            .and_then(Value::as_array)
            .expect("hooks array");
        assert!(!hooks.is_empty(), "workspace hook must be projected");
        let hook = &hooks[0];
        assert_eq!(hook["event"], "PreToolUse");
        assert_eq!(hook["command"], "echo hi");
        assert_eq!(hook["type"], "command");
        assert_eq!(result["hooksEnabled"], json!(true));
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn channel_dispatch_load_and_reject_unknown() {
        let workspace = temp_workspace();
        let service = HooksService::new(temp_paths());
        let ctx = ChannelContext::new("hooks-test", ClientMode::DesktopContinuous);
        let params = WireValue::Json(json!({"workspacePath": workspace.to_string_lossy()}));
        let result = service
            .call(&ctx, "loadHooks", vec![params], CancelToken::new())
            .await
            .expect("loadHooks dispatch");
        let WireValue::Json(value) = result else {
            panic!("loadHooks must return JSON");
        };
        assert!(value.get("hooks").and_then(Value::as_array).is_some());
        let error = service
            .call(&ctx, "bogus", vec![], CancelToken::new())
            .await
            .expect_err("unknown method");
        assert!(error.to_string().contains("Method not found"));
        let _ = std::fs::remove_dir_all(&workspace);
    }
}
