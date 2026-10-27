//! 配置发现与解析（`workspace-hook-config.ts` 的 Rust 版）。
//!
//! 格式纪律（与 TS schema 逐条对应）：
//! - **root / matcher / events 是 strict**：未知键 = 配置不可读（不是静默忽略）。
//!   静默忽略会让"用户以为关掉了 hook，实际还在跑"成为可能。
//! - **hook 定义本身是 passthrough**：未知字段保留但不参与判定/摘要（TS 同）。
//! - 数值门禁：`timeoutMs`/`timeout` 必须是有限正数；`args` 必须是字符串数组。
//! - 解析失败是**读取错误**（`workspace_hooks_config_unreadable`），与写入失败分开。

use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `WORKSPACE_HOOK_DIGEST_SCHEMA_VERSION`：进摘要的格式版本，不能随手改。
pub const WORKSPACE_HOOK_DIGEST_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 32_768;

pub const EVENT_NAMES: [HookEventName; 7] = [
    HookEventName::SessionStart,
    HookEventName::UserPromptSubmit,
    HookEventName::PreToolUse,
    HookEventName::PermissionRequest,
    HookEventName::PostToolUse,
    HookEventName::PostToolUseFailure,
    HookEventName::Stop,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookEventName {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    PostToolUseFailure,
    Stop,
}

impl HookEventName {
    pub fn as_str(self) -> &'static str {
        match self {
            HookEventName::SessionStart => "SessionStart",
            HookEventName::UserPromptSubmit => "UserPromptSubmit",
            HookEventName::PreToolUse => "PreToolUse",
            HookEventName::PermissionRequest => "PermissionRequest",
            HookEventName::PostToolUse => "PostToolUse",
            HookEventName::PostToolUseFailure => "PostToolUseFailure",
            HookEventName::Stop => "Stop",
        }
    }
}

/// `shell: true | string`。`false` 在 TS schema 里非法，这里在 `validate` 里拒绝。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum ShellConfig {
    Enabled(bool),
    Script(String),
}

/// 单条 hook 定义（`process` 或 `command`）。未知字段被忽略（passthrough）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HookDefinition {
    Process {
        command: String,
        #[serde(default)]
        enabled: Option<bool>,
        #[serde(default)]
        args: Option<Vec<String>>,
        #[serde(default, rename = "timeoutMs")]
        timeout_ms: Option<f64>,
        #[serde(default, rename = "statusMessage")]
        status_message: Option<String>,
    },
    Command {
        command: String,
        #[serde(default)]
        enabled: Option<bool>,
        #[serde(default, rename = "async")]
        asynchronous: Option<bool>,
        #[serde(default)]
        shell: Option<ShellConfig>,
        #[serde(default)]
        timeout: Option<f64>,
        #[serde(default, rename = "timeoutMs")]
        timeout_ms: Option<f64>,
        #[serde(default, rename = "statusMessage")]
        status_message: Option<String>,
    },
}

impl HookDefinition {
    pub fn command(&self) -> &str {
        match self {
            HookDefinition::Process { command, .. } | HookDefinition::Command { command, .. } => {
                command
            }
        }
    }

    pub fn enabled(&self) -> Option<bool> {
        match self {
            HookDefinition::Process { enabled, .. } | HookDefinition::Command { enabled, .. } => {
                *enabled
            }
        }
    }

    pub fn status_message(&self) -> Option<&str> {
        match self {
            HookDefinition::Process { status_message, .. }
            | HookDefinition::Command { status_message, .. } => status_message.as_deref(),
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.command().trim().is_empty() {
            return Err(ConfigError::invalid("hook.command: must not be empty"));
        }
        let numbers = match self {
            HookDefinition::Process {
                timeout_ms, args, ..
            } => {
                if args
                    .as_ref()
                    .is_some_and(|args| args.iter().any(|arg| arg.contains('\0')))
                {
                    return Err(ConfigError::invalid("hook.args: must not contain NUL"));
                }
                [*timeout_ms, None]
            }
            HookDefinition::Command {
                timeout,
                timeout_ms,
                shell,
                ..
            } => {
                if matches!(shell, Some(ShellConfig::Enabled(false))) {
                    return Err(ConfigError::invalid(
                        "hook.shell: must be true or a non-empty string",
                    ));
                }
                if matches!(shell, Some(ShellConfig::Script(script)) if script.trim().is_empty()) {
                    return Err(ConfigError::invalid(
                        "hook.shell: string form must not be empty",
                    ));
                }
                [*timeout, *timeout_ms]
            }
        };
        for value in numbers.into_iter().flatten() {
            if !value.is_finite() || value <= 0.0 {
                return Err(ConfigError::invalid(
                    "hook.timeout/timeoutMs: must be a finite positive number",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MatcherConfig {
    #[serde(default)]
    pub matcher: Option<String>,
    pub hooks: Vec<HookDefinition>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HookEvents {
    #[serde(default, rename = "SessionStart")]
    pub session_start: Option<Vec<MatcherConfig>>,
    #[serde(default, rename = "UserPromptSubmit")]
    pub user_prompt_submit: Option<Vec<MatcherConfig>>,
    #[serde(default, rename = "PreToolUse")]
    pub pre_tool_use: Option<Vec<MatcherConfig>>,
    #[serde(default, rename = "PermissionRequest")]
    pub permission_request: Option<Vec<MatcherConfig>>,
    #[serde(default, rename = "PostToolUse")]
    pub post_tool_use: Option<Vec<MatcherConfig>>,
    #[serde(default, rename = "PostToolUseFailure")]
    pub post_tool_use_failure: Option<Vec<MatcherConfig>>,
    #[serde(default, rename = "Stop")]
    pub stop: Option<Vec<MatcherConfig>>,
}

impl HookEvents {
    pub fn for_event(&self, event: HookEventName) -> &[MatcherConfig] {
        match event {
            HookEventName::SessionStart => self.session_start.as_deref().unwrap_or(&[]),
            HookEventName::UserPromptSubmit => self.user_prompt_submit.as_deref().unwrap_or(&[]),
            HookEventName::PreToolUse => self.pre_tool_use.as_deref().unwrap_or(&[]),
            HookEventName::PermissionRequest => self.permission_request.as_deref().unwrap_or(&[]),
            HookEventName::PostToolUse => self.post_tool_use.as_deref().unwrap_or(&[]),
            HookEventName::PostToolUseFailure => {
                self.post_tool_use_failure.as_deref().unwrap_or(&[])
            }
            HookEventName::Stop => self.stop.as_deref().unwrap_or(&[]),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HooksConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub timeout_ms: Option<f64>,
    #[serde(default)]
    pub max_output_bytes: Option<f64>,
    #[serde(default)]
    pub events: Option<HookEvents>,
}

impl HooksConfig {
    /// 解析并校验一份 `hooks` 段（root 的未知键在这里被拒绝）。
    pub fn parse(value: &Value) -> Result<Self, ConfigError> {
        let config: HooksConfig = serde_json::from_value(value.clone())
            .map_err(|error| ConfigError::invalid(format!("hooks: {error}")))?;
        for value in [config.timeout_ms, config.max_output_bytes]
            .into_iter()
            .flatten()
        {
            if !value.is_finite() || value <= 0.0 {
                return Err(ConfigError::invalid(
                    "hooks.timeoutMs/maxOutputBytes: must be finite positive numbers",
                ));
            }
        }
        if let Some(events) = &config.events {
            for event in EVENT_NAMES {
                for matcher in events.for_event(event) {
                    if matcher.hooks.is_empty() {
                        return Err(ConfigError::invalid(
                            "hooks.events[].hooks: must contain at least one hook",
                        ));
                    }
                    if matcher
                        .matcher
                        .as_deref()
                        .is_some_and(|matcher| matcher.trim().is_empty())
                    {
                        return Err(ConfigError::invalid(
                            "hooks.events[].matcher: must not be empty",
                        ));
                    }
                    for hook in &matcher.hooks {
                        hook.validate()?;
                    }
                }
            }
        }
        Ok(config)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub message: String,
}

impl ConfigError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// 运行根（`enabled`/`timeoutMs`/`maxOutputBytes` 的合并结果）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeRoot {
    pub enabled: bool,
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
}

/// 一份配置里参与合并的三个根字段（其余字段不跨文件合并）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RootGates {
    pub enabled: Option<bool>,
    pub timeout_ms: Option<f64>,
    pub max_output_bytes: Option<f64>,
}

/// `resolveWorkspaceHookRuntimeRoot`：后者覆盖前者；`enabled` 一旦为 true 就保持。
pub fn resolve_runtime_root(roots: &[RootGates]) -> RuntimeRoot {
    let mut enabled = false;
    let mut timeout_ms = DEFAULT_TIMEOUT_MS as f64;
    let mut max_output_bytes = DEFAULT_MAX_OUTPUT_BYTES as f64;
    for root in roots {
        if root.enabled == Some(true) {
            enabled = true;
        }
        if let Some(value) = root.timeout_ms {
            timeout_ms = value;
        }
        if let Some(value) = root.max_output_bytes {
            max_output_bytes = value;
        }
    }
    RuntimeRoot {
        enabled,
        timeout_ms: rounded_positive(timeout_ms),
        max_output_bytes: rounded_positive(max_output_bytes) as usize,
    }
}

/// `resolveWorkspaceHookTimeoutMs`：`timeoutMs` 优先，`timeout`（秒）次之，
/// 最后是默认值；结果按 JS `Math.max(1, Math.round(x))` 取整。
pub fn resolve_timeout_ms(hook: &HookDefinition, default_timeout_ms: u64) -> u64 {
    let raw = match hook {
        HookDefinition::Command {
            timeout,
            timeout_ms,
            ..
        } => timeout_ms.or(timeout.map(|seconds| seconds * 1000.0)),
        HookDefinition::Process { timeout_ms, .. } => *timeout_ms,
    };
    match raw {
        Some(value) => rounded_positive(value),
        None => default_timeout_ms.max(1),
    }
}

fn rounded_positive(value: f64) -> u64 {
    let rounded = value.round();
    if !rounded.is_finite() || rounded < 1.0 {
        1
    } else if rounded > u64::MAX as f64 {
        u64::MAX
    } else {
        rounded as u64
    }
}

/// 三档门禁（`resolveWorkspaceHookConfiguredGates`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gates {
    pub source_root_enabled: bool,
    pub declaration_enabled: bool,
    pub runtime_hooks_enabled: bool,
    pub configured_enabled: bool,
}

pub fn configured_gates(
    source_enabled: Option<bool>,
    declaration_enabled: Option<bool>,
    runtime_hooks_enabled: bool,
) -> Gates {
    let source_root_enabled = source_enabled != Some(false);
    let declaration_enabled = declaration_enabled != Some(false);
    Gates {
        source_root_enabled,
        declaration_enabled,
        runtime_hooks_enabled,
        configured_enabled: source_root_enabled && declaration_enabled && runtime_hooks_enabled,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFileKind {
    ZcodeJson,
    ZcodeConfigJson,
    Explicit,
}

impl ConfigFileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ConfigFileKind::ZcodeJson => "zcode.json",
            ConfigFileKind::ZcodeConfigJson => ".zcode/config.json",
            ConfigFileKind::Explicit => "explicit",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPathRef {
    pub path: PathBuf,
    pub explicit_project_config: bool,
}

/// 一份成功读到的 hook 配置源。
#[derive(Debug, Clone, PartialEq)]
pub struct SourceInput {
    pub canonical_path: PathBuf,
    pub base_dir: PathBuf,
    /// 在**全部候选 refs** 里的下标（含没有 `hooks` 段而被跳过的文件），与 TS 一致。
    pub discovery_order: usize,
    pub config_file_kind: ConfigFileKind,
    pub explicit_project_config: bool,
    pub editable: bool,
    pub hooks: HooksConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceReadError {
    pub path: PathBuf,
    pub message: String,
    /// `workspace_hooks_config_unreadable` 与写入失败分开。
    pub reason_code: &'static str,
}

/// 发现候选配置：从 worktree 根向下两档（根 + `.zcode/`），再加显式路径。
///
/// `getProjectConfigDirectories`：向上走到含 `.git` 的目录（目录或文件都算），
/// 反转成"根在前"；没有 `.git` 时只看起点。顺序即 discoveryOrder，进摘要。
pub fn discover_config_paths(working_dir: &Path, explicit: Option<&Path>) -> Vec<ConfigPathRef> {
    let start = absolute_lexical(working_dir);
    let mut directories = Vec::new();
    let mut current = start.clone();
    let mut found_root = false;
    loop {
        directories.push(current.clone());
        if current.join(".git").exists() {
            found_root = true;
            break;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => break,
        }
    }
    if found_root {
        directories.reverse();
    }

    let mut refs: Vec<ConfigPathRef> = Vec::new();
    for directory in directories {
        for candidate in [
            directory.join("zcode.json"),
            directory.join(".zcode").join("config.json"),
        ] {
            if candidate.exists() {
                refs.push(ConfigPathRef {
                    path: candidate,
                    explicit_project_config: false,
                });
            }
        }
    }
    if let Some(explicit) = explicit {
        let explicit = absolute_lexical(explicit);
        if explicit.exists() {
            refs.push(ConfigPathRef {
                path: explicit,
                explicit_project_config: true,
            });
        }
    }
    // 同一文件被 auto-discovery 与显式路径各发现一次 → 只保留先出现的（顺序进摘要，
    // 重排会让既有信任记录全部失效）。
    let mut seen = std::collections::HashSet::new();
    refs.retain(|reference| seen.insert(reference.path.clone()));
    refs
}

/// 读所有候选源；单个文件坏掉只是**读取错误**，不影响其它源。
pub fn read_sources(
    working_dir: &Path,
    explicit: Option<&Path>,
) -> (Vec<SourceInput>, Vec<SourceReadError>) {
    let working_dir = absolute_lexical(working_dir);
    let refs = discover_config_paths(&working_dir, explicit);
    let mut sources = Vec::new();
    let mut errors = Vec::new();
    for (discovery_order, reference) in refs.iter().enumerate() {
        let text = match fs::read_to_string(&reference.path) {
            Ok(text) => text,
            Err(error) => {
                errors.push(SourceReadError {
                    path: reference.path.clone(),
                    message: format!("read failed: {error}"),
                    reason_code: "workspace_hooks_config_unreadable",
                });
                continue;
            }
        };
        let value: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(error) => {
                errors.push(SourceReadError {
                    path: reference.path.clone(),
                    message: format!("invalid JSON: {error}"),
                    reason_code: "workspace_hooks_config_unreadable",
                });
                continue;
            }
        };
        let Some(hooks_value) = value.get("hooks") else {
            // 没有 hooks 段的配置文件不是错误：它只是不贡献声明（但 discoveryOrder 仍占位）。
            continue;
        };
        let hooks = match HooksConfig::parse(hooks_value) {
            Ok(hooks) => hooks,
            Err(error) => {
                errors.push(SourceReadError {
                    path: reference.path.clone(),
                    message: error.message,
                    reason_code: "workspace_hooks_config_unreadable",
                });
                continue;
            }
        };
        let config_directory = reference
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| working_dir.clone());
        let base_dir =
            if config_directory.file_name().and_then(|name| name.to_str()) == Some(".zcode") {
                config_directory
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or(config_directory.clone())
            } else {
                config_directory.clone()
            };
        let config_file_kind = if reference.explicit_project_config {
            ConfigFileKind::Explicit
        } else if reference.path.file_name().and_then(|name| name.to_str()) == Some("zcode.json") {
            ConfigFileKind::ZcodeJson
        } else {
            ConfigFileKind::ZcodeConfigJson
        };
        let editable = !reference.explicit_project_config
            && reference.path == working_dir.join(".zcode").join("config.json");
        sources.push(SourceInput {
            canonical_path: reference.path.clone(),
            base_dir,
            discovery_order,
            config_file_kind,
            explicit_project_config: reference.explicit_project_config,
            editable,
            hooks,
        });
    }
    (sources, errors)
}

/// 词法绝对化（等价 `path.resolve`：不做符号链接解析，只清理 `.`/`..`）。
pub fn absolute_lexical(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// 相对路径（分隔符统一成 `/`；相同路径回落到文件名），进摘要。
pub fn relative_source_path(workspace_path: &Path, source_path: &Path) -> String {
    let workspace = absolute_lexical(workspace_path);
    let source = absolute_lexical(source_path);
    let workspace_components: Vec<_> = workspace.components().collect();
    let source_components: Vec<_> = source.components().collect();
    let mut shared = 0;
    while shared < workspace_components.len()
        && shared < source_components.len()
        && workspace_components[shared] == source_components[shared]
    {
        shared += 1;
    }
    let mut relative = PathBuf::new();
    for _ in shared..workspace_components.len() {
        relative.push("..");
    }
    for component in &source_components[shared..] {
        relative.push(component.as_os_str());
    }
    let rendered = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/")
        .replace('\\', "/");
    if rendered.is_empty() {
        source
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| rendered.clone())
    } else {
        rendered
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_hooks() -> Value {
        json!({
            "enabled": true,
            "events": {
                "PreToolUse": [
                    {"matcher": "Bash", "hooks": [{"type": "command", "command": "node check.js"}]},
                    {"hooks": [{"type": "process", "command": "git", "args": ["status", "--short"]}]}
                ],
                "Stop": [
                    {"hooks": [{"type": "command", "command": "echo done", "async": true, "shell": true}]}
                ]
            }
        })
    }

    #[test]
    fn parses_the_documented_shape_and_ignores_unknown_hook_fields() {
        let mut value = sample_hooks();
        value["events"]["PreToolUse"][0]["hooks"][0]["futureField"] = json!({"x": 1});
        let config = HooksConfig::parse(&value).expect("passthrough hook fields are allowed");
        let events = config.events.expect("events");
        let pre = events.for_event(HookEventName::PreToolUse);
        assert_eq!(pre.len(), 2);
        assert_eq!(pre[0].matcher.as_deref(), Some("Bash"));
        assert_eq!(pre[1].matcher, None);
        assert_eq!(pre[1].hooks[0].command(), "git");
        assert_eq!(config.enabled, Some(true));
    }

    #[test]
    fn unknown_root_or_matcher_keys_are_config_errors_not_silent_defaults() {
        let unknown_root = json!({"enabled": true, "futureRoot": 1});
        assert!(HooksConfig::parse(&unknown_root).is_err());

        let unknown_matcher = json!({
            "events": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "x"}], "future": 1}]}
        });
        assert!(HooksConfig::parse(&unknown_matcher).is_err());

        let unknown_event = json!({
            "events": {"PreToolUsez": [{"hooks": [{"type": "command", "command": "x"}]}]}
        });
        assert!(HooksConfig::parse(&unknown_event).is_err());
    }

    #[test]
    fn invalid_hook_definitions_are_rejected() {
        for hooks in [
            json!([{"type": "command", "command": "   "}]),
            json!([{"type": "command", "command": "x", "timeoutMs": 0}]),
            json!([{"type": "command", "command": "x", "timeoutMs": -1}]),
            json!([{"type": "command", "command": "x", "shell": false}]),
            json!([{"type": "command", "command": "x", "shell": "  "}]),
            json!([{"type": "process", "command": "x", "args": "not-an-array"}]),
            json!([{"type": "unknown", "command": "x"}]),
        ] {
            let value = json!({"events": {"Stop": [{"hooks": hooks}]}});
            assert!(HooksConfig::parse(&value).is_err(), "must reject {value}");
        }
    }

    #[test]
    fn runtime_root_merge_and_gates_match_the_ts_rules() {
        let root = resolve_runtime_root(&[
            RootGates {
                enabled: Some(true),
                timeout_ms: Some(1500.4),
                ..RootGates::default()
            },
            RootGates {
                enabled: Some(false),
                timeout_ms: Some(2000.6),
                max_output_bytes: Some(4096.0),
            },
        ]);
        assert!(root.enabled, "enabled 一旦为 true 就保持");
        assert_eq!(root.timeout_ms, 2001, "后者覆盖 + Math.round");
        assert_eq!(root.max_output_bytes, 4096);

        let disabled = resolve_runtime_root(&[RootGates::default()]);
        assert!(!disabled.enabled);
        assert_eq!(disabled.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert_eq!(disabled.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);

        let gates = configured_gates(Some(false), None, true);
        assert!(!gates.configured_enabled);
        assert!(gates.declaration_enabled);
        let gates = configured_gates(None, None, true);
        assert!(gates.configured_enabled);
        let gates = configured_gates(None, None, false);
        assert!(
            !gates.configured_enabled,
            "runtime root 关闭 → 配置也跑不了"
        );
    }

    #[test]
    fn timeout_resolution_prefers_ms_then_seconds_then_default() {
        let ms = HookDefinition::Command {
            command: "x".into(),
            enabled: None,
            asynchronous: None,
            shell: None,
            timeout: Some(30.0),
            timeout_ms: Some(250.4),
            status_message: None,
        };
        assert_eq!(resolve_timeout_ms(&ms, 60_000), 250);

        let seconds = HookDefinition::Command {
            command: "x".into(),
            enabled: None,
            asynchronous: None,
            shell: None,
            timeout: Some(1.5),
            timeout_ms: None,
            status_message: None,
        };
        assert_eq!(resolve_timeout_ms(&seconds, 60_000), 1500);

        let defaulted = HookDefinition::Process {
            command: "git".into(),
            enabled: None,
            args: None,
            timeout_ms: None,
            status_message: None,
        };
        assert_eq!(resolve_timeout_ms(&defaulted, 60_000), 60_000);
        assert_eq!(resolve_timeout_ms(&defaulted, 0), 1, "至少 1ms");
    }

    #[test]
    fn discovery_walks_up_to_the_git_root_and_dedupes_explicit_paths() {
        let base =
            std::env::temp_dir().join(format!("zcode-hook-discovery-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let workspace = base.join("repo");
        let nested = workspace.join("packages").join("app");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(workspace.join(".git")).unwrap();
        fs::write(workspace.join("zcode.json"), "{}").unwrap();
        fs::create_dir_all(workspace.join(".zcode")).unwrap();
        fs::write(
            workspace.join(".zcode").join("config.json"),
            r#"{"hooks":{"enabled":true}}"#,
        )
        .unwrap();

        let refs =
            discover_config_paths(&nested, Some(&workspace.join(".zcode").join("config.json")));
        let rendered: Vec<_> = refs
            .iter()
            .map(|reference| {
                (
                    reference
                        .path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .to_string(),
                    reference.explicit_project_config,
                )
            })
            .collect();
        assert_eq!(
            rendered,
            vec![
                ("zcode.json".to_string(), false),
                ("config.json".to_string(), false)
            ],
            "根目录的 zcode.json 在前，显式重复路径被丢弃：{rendered:?}"
        );

        let (sources, errors) = read_sources(&nested, None);
        assert!(errors.is_empty());
        assert_eq!(
            sources.len(),
            1,
            "没有 hooks 段的 zcode.json 不贡献 source（TS 的 skip）"
        );
        assert_eq!(sources[0].discovery_order, 1, "它仍占用 discoveryOrder 0");
        assert_eq!(sources[0].config_file_kind, ConfigFileKind::ZcodeConfigJson);
        assert_eq!(sources[0].base_dir, workspace);
        assert!(
            !sources[0].editable,
            "editable 只对工作区的 .zcode/config.json 成立"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn unreadable_config_is_reported_but_other_sources_still_load() {
        let base = std::env::temp_dir().join(format!("zcode-hook-bad-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("zcode.json"), "{ not json").unwrap();
        fs::create_dir_all(base.join(".zcode")).unwrap();
        fs::write(
            base.join(".zcode").join("config.json"),
            r#"{"hooks":{"events":{"Stop":[{"hooks":[{"type":"command","command":"echo ok"}]}]}}}"#,
        )
        .unwrap();
        let (sources, errors) = read_sources(&base, None);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].reason_code, "workspace_hooks_config_unreadable");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].discovery_order, 1, "坏文件仍占用序号");
        let _ = fs::remove_dir_all(&base);
    }
}
