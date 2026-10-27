//! 声明的 canonical 化与摘要（`workspace-hook-digest.ts` 的 Rust 版）。
//!
//! 摘要 = `sha256(JSON.stringify(payload))`，payload 是**定序数组**（不是对象）：
//! 字段顺序天然稳定，也不会因为 JSON 对象键顺序在不同实现里不同而分叉。
//!
//! 与 JS 的两个细节必须对齐：
//! 1. `JSON.stringify` 的整数不带小数点——Rust 侧用 [`js_number`] 把整数值的
//!    `f64` 归一成整数，否则 `1500.0` 会序列化成 `1500.0` 而 JS 是 `1500`。
//! 2. 路径分隔符统一成 `/`（Windows 的 `\` 会让同一份配置在两个平台算出不同摘要）。

use std::path::Path;

use serde_json::{json, Value};

use crate::content_hash::sha256_hex;

use super::config::{
    configured_gates, relative_source_path, resolve_timeout_ms, ConfigFileKind, RootGates,
    RuntimeRoot, ShellConfig, SourceInput, WORKSPACE_HOOK_DIGEST_SCHEMA_VERSION,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryType {
    Command,
    Process,
}

impl EntryType {
    pub fn as_str(self) -> &'static str {
        match self {
            EntryType::Command => "command",
            EntryType::Process => "process",
        }
    }
}

/// 一条 canonical hook 声明（`canonicalWorkspaceHookEntrySchema`）。
#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalEntry {
    pub review_item_id: String,
    pub event: super::config::HookEventName,
    pub matcher_index: usize,
    pub hook_index: usize,
    pub source_file_index: usize,
    /// 源文件的 discoveryOrder（信任"槽位"判定的输入；不在协议投影里）。
    pub source_discovery_order: usize,
    pub source_relative_path: String,
    pub matcher: Option<String>,
    pub entry_type: EntryType,
    pub command: String,
    pub args: Option<Vec<String>>,
    pub asynchronous: Option<bool>,
    pub shell: Option<ShellConfig>,
    pub resolved_timeout_ms: u64,
    pub resolved_max_output_bytes: usize,
    pub status_message: Option<String>,
    pub source_root_enabled: bool,
    pub declaration_enabled: bool,
    pub runtime_hooks_enabled: bool,
    pub configured_enabled: bool,
    pub editable: bool,
    pub hook_declaration_digest: String,
}

impl CanonicalEntry {
    /// `canonicalWorkspaceHookEntrySchema` 的 JSON 投影（进快照/评审交互）。
    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "reviewItemId": self.review_item_id,
            "event": self.event.as_str(),
            "matcherIndex": self.matcher_index,
            "hookIndex": self.hook_index,
            "sourceFileIndex": self.source_file_index,
            "sourceRelativePath": self.source_relative_path,
            "matcher": self.matcher,
            "type": self.entry_type.as_str(),
            "command": self.command,
            "resolvedTimeoutMs": self.resolved_timeout_ms,
            "resolvedMaxOutputBytes": self.resolved_max_output_bytes,
            "sourceRootEnabled": self.source_root_enabled,
            "declarationEnabled": self.declaration_enabled,
            "runtimeHooksEnabled": self.runtime_hooks_enabled,
            "configuredEnabled": self.configured_enabled,
            "editable": self.editable,
            "declarationDigestAlgorithm": "sha256",
            "hookDeclarationDigest": self.hook_declaration_digest,
        });
        let object = value
            .as_object_mut()
            .expect("entry projection is an object");
        if let Some(status_message) = &self.status_message {
            object.insert("statusMessage".into(), json!(status_message));
        }
        match self.entry_type {
            EntryType::Process => {
                if let Some(args) = &self.args {
                    if !args.is_empty() {
                        object.insert("args".into(), json!(args));
                    }
                }
            }
            EntryType::Command => {
                if self.asynchronous == Some(true) {
                    object.insert("async".into(), json!(true));
                }
                match &self.shell {
                    Some(ShellConfig::Enabled(true)) => {
                        object.insert("shell".into(), json!(true));
                    }
                    Some(ShellConfig::Script(script)) => {
                        object.insert("shell".into(), json!(script));
                    }
                    _ => {}
                }
            }
        }
        value
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceFileSnapshot {
    pub canonical_path: String,
    pub base_dir: String,
    pub discovery_order: usize,
    pub config_file_kind: ConfigFileKind,
    pub explicit_project_config: bool,
    pub editable: bool,
    pub gates: RootGates,
}

impl SourceFileSnapshot {
    fn to_json(&self) -> Value {
        let mut hooks_root = serde_json::Map::new();
        if let Some(enabled) = self.gates.enabled {
            hooks_root.insert("enabled".into(), json!(enabled));
        }
        if let Some(timeout_ms) = self.gates.timeout_ms {
            hooks_root.insert("timeoutMs".into(), js_number(timeout_ms));
        }
        if let Some(max_output_bytes) = self.gates.max_output_bytes {
            hooks_root.insert("maxOutputBytes".into(), js_number(max_output_bytes));
        }
        json!({
            "canonicalPath": self.canonical_path,
            "baseDir": self.base_dir,
            "discoveryOrder": self.discovery_order,
            "configFileKind": self.config_file_kind.as_str(),
            "explicitProjectConfig": self.explicit_project_config,
            "editable": self.editable,
            "hooksRoot": Value::Object(hooks_root),
        })
    }
}

/// `workspaceHookBundleSnapshotSchema`。
#[derive(Debug, Clone, PartialEq)]
pub struct BundleSnapshot {
    pub workspace_identity: String,
    pub discovered_at: String,
    pub source_files: Vec<SourceFileSnapshot>,
    pub hooks: Vec<CanonicalEntry>,
    pub bundle_digest: String,
}

impl BundleSnapshot {
    pub fn to_json(&self) -> Value {
        json!({
            "schemaVersion": WORKSPACE_HOOK_DIGEST_SCHEMA_VERSION,
            "workspaceIdentity": self.workspace_identity,
            "discoveredAt": self.discovered_at,
            "sourceFiles": self.source_files.iter().map(SourceFileSnapshot::to_json).collect::<Vec<_>>(),
            "hooks": self.hooks.iter().map(CanonicalEntry::to_json).collect::<Vec<_>>(),
            "digestAlgorithm": "sha256",
            "bundleDigest": self.bundle_digest,
        })
    }
}

/// 一次声明的 digest 输入（每条 hook 一条）。
pub struct DeclarationDigestInput<'a> {
    pub source_relative_path: &'a str,
    pub source_discovery_order: usize,
    pub event: super::config::HookEventName,
    pub matcher: Option<&'a str>,
    pub matcher_index: usize,
    pub hook_index: usize,
    pub hook: &'a super::config::HookDefinition,
    pub resolved_timeout_ms: u64,
    pub resolved_max_output_bytes: usize,
}

pub fn declaration_digest(input: &DeclarationDigestInput<'_>) -> String {
    let execution = match input.hook {
        super::config::HookDefinition::Process { command, args, .. } => json!([
            "process",
            command,
            args.as_ref().cloned().unwrap_or_default()
        ]),
        super::config::HookDefinition::Command {
            command,
            asynchronous,
            shell,
            ..
        } => {
            let shell = match shell {
                None => json!(["unset"]),
                Some(ShellConfig::Enabled(true)) => json!(["true"]),
                Some(ShellConfig::Enabled(false)) => json!(["false"]),
                Some(ShellConfig::Script(script)) => json!(["string", script]),
            };
            json!(["command", command, asynchronous.unwrap_or(false), shell])
        }
    };
    let payload = json!([
        "workspace-hook-declaration",
        WORKSPACE_HOOK_DIGEST_SCHEMA_VERSION,
        input.source_relative_path,
        input.source_discovery_order,
        input.event.as_str(),
        input.matcher,
        input.matcher_index,
        input.hook_index,
        execution,
        input.resolved_timeout_ms,
        input.resolved_max_output_bytes,
    ]);
    sha256_hex(payload.to_string().as_bytes())
}

/// 展开所有源的声明（顺序 = source 顺序 × 事件闭集顺序 × matcher × hook）。
pub fn resolve_entries(
    workspace_path: &Path,
    sources: &[SourceInput],
    runtime: &RuntimeRoot,
) -> Vec<CanonicalEntry> {
    let mut entries = Vec::new();
    for (source_file_index, source) in sources.iter().enumerate() {
        let source_relative_path = relative_source_path(workspace_path, &source.canonical_path);
        for event in super::config::EVENT_NAMES {
            let matchers: &[super::config::MatcherConfig] = source
                .hooks
                .events
                .as_ref()
                .map(|events| events.for_event(event))
                .unwrap_or(&[]);
            for (matcher_index, matcher) in matchers.iter().enumerate() {
                for (hook_index, hook) in matcher.hooks.iter().enumerate() {
                    let gates =
                        configured_gates(source.hooks.enabled, hook.enabled(), runtime.enabled);
                    let resolved_timeout_ms = resolve_timeout_ms(hook, runtime.timeout_ms);
                    let resolved_max_output_bytes = runtime.max_output_bytes;
                    let hook_declaration_digest = declaration_digest(&DeclarationDigestInput {
                        source_relative_path: &source_relative_path,
                        source_discovery_order: source.discovery_order,
                        event,
                        matcher: matcher.matcher.as_deref(),
                        matcher_index,
                        hook_index,
                        hook,
                        resolved_timeout_ms,
                        resolved_max_output_bytes,
                    });
                    let mut entry = CanonicalEntry {
                        review_item_id: format!(
                            "workspace-hook-{source_file_index}-{}-{matcher_index}-{hook_index}",
                            event.as_str()
                        ),
                        event,
                        matcher_index,
                        hook_index,
                        source_file_index,
                        source_discovery_order: source.discovery_order,
                        source_relative_path: source_relative_path.clone(),
                        matcher: matcher.matcher.clone(),
                        entry_type: EntryType::Command,
                        command: hook.command().to_string(),
                        args: None,
                        asynchronous: None,
                        shell: None,
                        resolved_timeout_ms,
                        resolved_max_output_bytes,
                        status_message: hook.status_message().map(str::to_string),
                        source_root_enabled: gates.source_root_enabled,
                        declaration_enabled: gates.declaration_enabled,
                        runtime_hooks_enabled: gates.runtime_hooks_enabled,
                        configured_enabled: gates.configured_enabled,
                        editable: source.editable,
                        hook_declaration_digest,
                    };
                    match hook {
                        super::config::HookDefinition::Process { args, .. } => {
                            entry.entry_type = EntryType::Process;
                            entry.args = args.clone();
                        }
                        super::config::HookDefinition::Command {
                            asynchronous,
                            shell,
                            ..
                        } => {
                            entry.entry_type = EntryType::Command;
                            entry.asynchronous = *asynchronous;
                            entry.shell = shell.clone();
                        }
                    }
                    entries.push(entry);
                }
            }
        }
    }
    entries
}

/// 构造 bundle 快照。没有声明时返回 `None`（TS 同：空 bundle 不存在，也就没有评审）。
pub fn build_bundle(
    workspace_identity: &str,
    workspace_path: &Path,
    sources: &[SourceInput],
    runtime: &RuntimeRoot,
    discovered_at: &str,
) -> Option<BundleSnapshot> {
    let hooks = resolve_entries(workspace_path, sources, runtime);
    if hooks.is_empty() {
        return None;
    }
    let source_files: Vec<SourceFileSnapshot> = sources
        .iter()
        .map(|source| SourceFileSnapshot {
            canonical_path: source.canonical_path.to_string_lossy().to_string(),
            base_dir: source.base_dir.to_string_lossy().to_string(),
            discovery_order: source.discovery_order,
            config_file_kind: source.config_file_kind,
            explicit_project_config: source.explicit_project_config,
            editable: source.editable,
            gates: RootGates {
                enabled: source.hooks.enabled,
                timeout_ms: source.hooks.timeout_ms,
                max_output_bytes: source.hooks.max_output_bytes,
            },
        })
        .collect();
    let bundle_digest = sha256_hex(
        bundle_payload(workspace_path, sources, &hooks)
            .to_string()
            .as_bytes(),
    );
    Some(BundleSnapshot {
        workspace_identity: workspace_identity.to_string(),
        discovered_at: discovered_at.to_string(),
        source_files,
        hooks,
        bundle_digest,
    })
}

fn bundle_payload(
    workspace_path: &Path,
    sources: &[SourceInput],
    hooks: &[CanonicalEntry],
) -> Value {
    let source_payloads: Vec<Value> = sources
        .iter()
        .map(|source| {
            json!([
                relative_source_path(workspace_path, &source.canonical_path),
                source.discovery_order,
                source.config_file_kind.as_str(),
                source.explicit_project_config,
                canonical_optional(source.hooks.enabled.map(Value::Bool)),
                source
                    .hooks
                    .timeout_ms
                    .map(js_number)
                    .map(|value| json!(["set", value]))
                    .unwrap_or_else(|| json!(["unset"])),
                source
                    .hooks
                    .max_output_bytes
                    .map(js_number)
                    .map(|value| json!(["set", value]))
                    .unwrap_or_else(|| json!(["unset"])),
            ])
        })
        .collect();
    let hook_payloads: Vec<Value> = hooks
        .iter()
        .map(|hook| {
            json!([
                hook.hook_declaration_digest,
                hook.source_root_enabled,
                hook.declaration_enabled,
                hook.runtime_hooks_enabled,
                hook.configured_enabled,
            ])
        })
        .collect();
    json!([
        "workspace-hook-bundle",
        WORKSPACE_HOOK_DIGEST_SCHEMA_VERSION,
        source_payloads,
        hook_payloads,
    ])
}

fn canonical_optional(value: Option<Value>) -> Value {
    match value {
        Some(value) => json!(["set", value]),
        None => json!(["unset"]),
    }
}

fn js_number(value: f64) -> Value {
    if value.is_finite() && value.fract() == 0.0 && value.abs() <= i64::MAX as f64 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_hooks::config::{
        read_sources, RuntimeRoot, SourceInput, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_TIMEOUT_MS,
    };
    use serde_json::json;

    /// Node 参考值：用 TS 的 payload 形状 + `createHash('sha256')` 在同一条 fixture
    /// 上算出的摘要。任何"实现上差不多"的改动都会让这些常量失配。
    const REFERENCE_DECLARATION_DIGESTS: [&str; 3] = [
        "4a09ae22ee6d79cdf9fdbc11cb5d4a77c3eb411aebfa1b830c7ed41402bc5f4d",
        "46bece04b972250d65386e9b0fff579deb615913e7822cd24ec934d88dcd8860",
        "94e40f41499a9fd80a86eae579ba286314f64245df73efd8c68636b893ff2fc6",
    ];
    const REFERENCE_BUNDLE_DIGEST: &str =
        "17cc6a528bc9ff772063dde0409c293a0e19bed63e542bdb5255c669543debf0";

    fn fixture_workspace(tag: &str) -> std::path::PathBuf {
        let base =
            std::env::temp_dir().join(format!("zcode-hook-digest-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join(".zcode")).unwrap();
        std::fs::write(
            base.join(".zcode").join("config.json"),
            serde_json::to_string_pretty(&json!({
                "hooks": {
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
                }
            }))
            .unwrap(),
        )
        .unwrap();
        base
    }

    fn runtime() -> RuntimeRoot {
        RuntimeRoot {
            enabled: true,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    #[test]
    fn declaration_and_bundle_digests_match_the_typescript_implementation() {
        let workspace = fixture_workspace("parity");
        let (sources, errors) = read_sources(&workspace, None);
        assert!(errors.is_empty());
        let bundle = build_bundle(
            "ws-digest",
            &workspace,
            &sources,
            &runtime(),
            "2026-01-01T00:00:00.000Z",
        )
        .expect("bundle exists");
        assert_eq!(bundle.hooks.len(), 3);
        let digests: Vec<&str> = bundle
            .hooks
            .iter()
            .map(|entry| entry.hook_declaration_digest.as_str())
            .collect();
        assert_eq!(
            digests, REFERENCE_DECLARATION_DIGESTS,
            "declaration digests"
        );
        assert_eq!(bundle.bundle_digest, REFERENCE_BUNDLE_DIGEST);
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn entries_carry_the_protocol_identity_and_shape() {
        let workspace = fixture_workspace("shape");
        let (sources, _) = read_sources(&workspace, None);
        let bundle = build_bundle(
            "ws-shape",
            &workspace,
            &sources,
            &runtime(),
            "2026-01-01T00:00:00.000Z",
        )
        .expect("bundle exists");
        let ids: Vec<&str> = bundle
            .hooks
            .iter()
            .map(|entry| entry.review_item_id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec![
                "workspace-hook-0-PreToolUse-0-0",
                "workspace-hook-0-PreToolUse-1-0",
                "workspace-hook-0-Stop-0-0",
            ]
        );
        let pre_tool = &bundle.hooks[0];
        assert_eq!(pre_tool.entry_type, EntryType::Command);
        assert_eq!(pre_tool.resolved_timeout_ms, DEFAULT_TIMEOUT_MS);
        assert_eq!(pre_tool.matcher.as_deref(), Some("Bash"));
        assert!(pre_tool.configured_enabled);
        let process = bundle.hooks[1].to_json();
        assert_eq!(process["type"], json!("process"));
        assert_eq!(process["args"], json!(["status", "--short"]));
        let stop = bundle.hooks[2].to_json();
        assert_eq!(stop["async"], json!(true));
        assert_eq!(stop["shell"], json!(true));
        assert!(stop.get("args").is_none(), "command 型不带 args");
        assert_eq!(stop["sourceRelativePath"], json!(".zcode/config.json"));

        let snapshot = bundle.to_json();
        assert_eq!(snapshot["schemaVersion"], json!(1));
        assert_eq!(snapshot["digestAlgorithm"], json!("sha256"));
        assert_eq!(
            snapshot["sourceFiles"][0]["configFileKind"],
            json!(".zcode/config.json")
        );
        assert_eq!(
            snapshot["sourceFiles"][0]["hooksRoot"]["enabled"],
            json!(true)
        );
    }

    #[test]
    fn changing_a_declaration_changes_its_digest_and_the_bundle_digest() {
        let workspace = fixture_workspace("mutate");
        let (sources, _) = read_sources(&workspace, None);
        let before = build_bundle("ws", &workspace, &sources, &runtime(), "t").unwrap();

        std::fs::write(
            workspace.join(".zcode").join("config.json"),
            serde_json::to_string(&json!({
                "hooks": {"enabled": true, "events": {"Stop": [
                    {"hooks": [{"type": "command", "command": "echo changed"}]}
                ]}}
            }))
            .unwrap(),
        )
        .unwrap();
        let (sources, _) = read_sources(&workspace, None);
        let after = build_bundle("ws", &workspace, &sources, &runtime(), "t").unwrap();
        assert_ne!(before.bundle_digest, after.bundle_digest);
        assert_ne!(
            before.hooks[2].hook_declaration_digest, after.hooks[0].hook_declaration_digest,
            "改命令必须改声明摘要（否则旧信任会静默套到新命令上）"
        );
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn an_empty_bundle_does_not_exist() {
        let base = std::env::temp_dir().join(format!("zcode-hook-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let (sources, errors) = read_sources(&base, None);
        assert!(errors.is_empty());
        assert!(build_bundle("ws", &base, &sources, &runtime(), "t").is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn source_relative_paths_use_forward_slashes_and_survive_shared_prefixes() {
        let workspace = std::path::Path::new("/repo/app");
        let source = std::path::Path::new("/repo/app/.zcode/config.json");
        assert_eq!(
            super::relative_source_path(workspace, source),
            ".zcode/config.json"
        );
        let outside = std::path::Path::new("/repo/shared/hooks.json");
        assert_eq!(
            super::relative_source_path(workspace, outside),
            "../shared/hooks.json"
        );
    }

    #[test]
    fn hooks_config_and_source_inputs_are_the_only_digest_inputs() {
        // discoveryOrder / explicitProjectConfig / enabled / timeoutMs 都进 bundle 摘要：
        // 改任意一个都必须让摘要变化（TS 注释里点名的兼容性约束）。
        let workspace = fixture_workspace("inputs");
        let (sources, _) = read_sources(&workspace, None);
        let baseline = build_bundle("ws", &workspace, &sources, &runtime(), "t").unwrap();
        let mut shifted: Vec<SourceInput> = sources.clone();
        shifted[0].discovery_order = 1;
        let changed = build_bundle("ws", &workspace, &shifted, &runtime(), "t").unwrap();
        assert_ne!(
            baseline.bundle_digest, changed.bundle_digest,
            "discoveryOrder 是摘要输入"
        );

        let mut explicit: Vec<SourceInput> = sources.clone();
        explicit[0].explicit_project_config = true;
        let changed = build_bundle("ws", &workspace, &explicit, &runtime(), "t").unwrap();
        assert_ne!(baseline.bundle_digest, changed.bundle_digest);

        let runtime_disabled = RuntimeRoot {
            enabled: false,
            ..runtime()
        };
        let disabled = build_bundle("ws", &workspace, &sources, &runtime_disabled, "t").unwrap();
        assert_ne!(
            baseline.bundle_digest, disabled.bundle_digest,
            "runtimeHooksEnabled 进摘要"
        );
        assert!(disabled.hooks.iter().all(|entry| !entry.configured_enabled));
    }
}
