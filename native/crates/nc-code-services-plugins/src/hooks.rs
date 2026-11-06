//! Plugin hook source discovery and detail projection — port of
//! `adapters/src/plugins/hook-sources.ts` and the hook inspection part of
//! `adapters/src/plugins/index.ts`.
//!
//! `plugins/describe` and `plugins/list` only need event names / hook details; the
//! executable hook path lives in the host's workspace hooks. This module never starts
//! a hook.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::diagnostics::Diagnostic;
use crate::fsutil::{is_file, resolve_inside};

pub const SUPPORTED_HOOK_EVENTS: [&str; 7] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PostToolUseFailure",
    "Stop",
];

pub struct HookPluginContext<'a> {
    pub root_path: &'a Path,
    pub manifest: &'a Map<String, Value>,
    pub manifest_path: &'a Path,
    pub plugin_id: &'a str,
}

pub struct HookSource {
    pub raw_hooks: Value,
    pub source_path: PathBuf,
    pub wrapper: bool,
}

/// `listPluginHookSources`.
pub fn list_plugin_hook_sources(
    ctx: &HookPluginContext<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<HookSource> {
    let mut sources = Vec::new();
    let mut loaded_paths: Vec<String> = Vec::new();
    let standard = ctx.root_path.join("hooks").join("hooks.json");
    if is_file(&standard) {
        if let Some(source) = load_hook_source(ctx, &standard, diagnostics) {
            loaded_paths.push(realpath_or_self(&standard));
            sources.push(source);
        }
    }
    let Some(manifest_hooks) = ctx.manifest.get("hooks") else {
        return sources;
    };
    let specs: Vec<&Value> = match manifest_hooks {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    for spec in specs {
        if let Value::String(path) = spec {
            let Some(path) = resolve_inside(ctx.root_path, path.trim_start_matches("./")) else {
                diagnostics.push(
                    Diagnostic::error(
                        "plugin_component_path_invalid",
                        format!("Plugin hooks path escapes plugin root: {path}"),
                    )
                    .with_path(ctx.manifest_path.to_string_lossy())
                    .with_plugin_id(ctx.plugin_id),
                );
                continue;
            };
            if !is_file(&path) {
                diagnostics.push(
                    Diagnostic::error(
                        "plugin_hook_read_failed",
                        format!("Plugin hooks file not found: {}", path.display()),
                    )
                    .with_path(path.to_string_lossy())
                    .with_plugin_id(ctx.plugin_id),
                );
                continue;
            }
            let real = realpath_or_self(&path);
            if loaded_paths.contains(&real) {
                diagnostics.push(
                    Diagnostic::warning(
                        "plugin_hook_invalid",
                        format!("Duplicate plugin hooks file ignored: {}", path.display()),
                    )
                    .with_path(path.to_string_lossy())
                    .with_plugin_id(ctx.plugin_id),
                );
                continue;
            }
            if let Some(source) = load_hook_source(ctx, &path, diagnostics) {
                loaded_paths.push(real);
                sources.push(source);
            }
            continue;
        }
        sources.push(HookSource {
            raw_hooks: spec.clone(),
            source_path: ctx.manifest_path.to_path_buf(),
            wrapper: false,
        });
    }
    sources
}

fn load_hook_source(
    ctx: &HookPluginContext<'_>,
    path: &Path,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<HookSource> {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(value) => Some(HookSource {
                raw_hooks: value,
                source_path: path.to_path_buf(),
                wrapper: true,
            }),
            Err(error) => {
                diagnostics.push(
                    Diagnostic::error(
                        "plugin_hook_read_failed",
                        format!("Failed to read plugin hooks: {error}"),
                    )
                    .with_path(path.to_string_lossy())
                    .with_plugin_id(ctx.plugin_id),
                );
                None
            }
        },
        Err(error) => {
            diagnostics.push(
                Diagnostic::error(
                    "plugin_hook_read_failed",
                    format!("Failed to read plugin hooks: {error}"),
                )
                .with_path(path.to_string_lossy())
                .with_plugin_id(ctx.plugin_id),
            );
            None
        }
    }
}

fn realpath_or_self(path: &Path) -> String {
    std::fs::canonicalize(path)
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

pub fn is_supported_hook_event(name: &str) -> bool {
    SUPPORTED_HOOK_EVENTS.contains(&name)
}

/// `listPluginHookEventNames`.
pub fn list_plugin_hook_event_names(
    ctx: &HookPluginContext<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for source in list_plugin_hook_sources(ctx, diagnostics) {
        let hooks_root = if source.wrapper {
            source.raw_hooks.get("hooks")
        } else {
            Some(&source.raw_hooks)
        };
        let Some(Value::Object(hooks)) = hooks_root else {
            diagnostics.push(
                Diagnostic::error(
                    "plugin_hook_invalid",
                    if source.wrapper {
                        "Plugin hooks file must contain a hooks object"
                    } else {
                        "Plugin manifest hooks entry must be an object, a path, or an array"
                    },
                )
                .with_path(source.source_path.to_string_lossy())
                .with_plugin_id(ctx.plugin_id),
            );
            continue;
        };
        for event_name in hooks.keys() {
            if !is_supported_hook_event(event_name) {
                diagnostics.push(
                    Diagnostic::warning(
                        "plugin_hook_unsupported_event",
                        format!(
                            "Plugin hook event is not supported by this ZCode runtime: {event_name}"
                        ),
                    )
                    .with_path(source.source_path.to_string_lossy())
                    .with_plugin_id(ctx.plugin_id),
                );
                continue;
            }
            if !names.contains(event_name) {
                names.push(event_name.clone());
            }
        }
    }
    names
}

/// `inspectPluginHooks`: event matcher configs plus the `hookDetails` projection.
pub fn inspect_plugin_hooks(
    ctx: &HookPluginContext<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> (Vec<Value>, Map<String, Value>) {
    let mut details = Vec::new();
    let mut events = Map::new();
    for source in list_plugin_hook_sources(ctx, diagnostics) {
        let hooks_root = if source.wrapper {
            source.raw_hooks.get("hooks")
        } else {
            Some(&source.raw_hooks)
        };
        let Some(Value::Object(hooks_root)) = hooks_root else {
            diagnostics.push(
                Diagnostic::error(
                    "plugin_hook_invalid",
                    if source.wrapper {
                        "Plugin hooks file must contain a hooks object"
                    } else {
                        "Plugin manifest hooks entry must be an object, a path, or an array"
                    },
                )
                .with_path(source.source_path.to_string_lossy())
                .with_plugin_id(ctx.plugin_id),
            );
            continue;
        };
        for (event_name, matcher_configs) in hooks_root {
            if !is_supported_hook_event(event_name) {
                diagnostics.push(
                    Diagnostic::warning(
                        "plugin_hook_unsupported_event",
                        format!(
                            "Plugin hook event is not supported by this ZCode runtime: {event_name}"
                        ),
                    )
                    .with_path(source.source_path.to_string_lossy())
                    .with_plugin_id(ctx.plugin_id),
                );
                continue;
            }
            let Some(configs) = matcher_configs.as_array() else {
                diagnostics.push(
                    Diagnostic::error(
                        "plugin_hook_invalid",
                        format!("Plugin hook event must be an array: {event_name}"),
                    )
                    .with_path(source.source_path.to_string_lossy())
                    .with_plugin_id(ctx.plugin_id),
                );
                continue;
            };
            if configs.is_empty() {
                diagnostics.push(
                    Diagnostic::error(
                        "plugin_hook_invalid",
                        format!(
                            "Invalid plugin hook matcher for {event_name}: hooks must not be empty"
                        ),
                    )
                    .with_path(source.source_path.to_string_lossy())
                    .with_plugin_id(ctx.plugin_id),
                );
                continue;
            }
            let mut matchers = Vec::new();
            for config in configs {
                match validate_matcher(config) {
                    Ok((matcher, hooks)) => {
                        let mut with_plugin = Map::new();
                        if let Some(matcher) = &matcher {
                            with_plugin.insert("matcher".into(), Value::String(matcher.clone()));
                        }
                        with_plugin.insert(
                            "hooks".into(),
                            Value::Array(
                                hooks
                                    .iter()
                                    .map(|hook| Value::Object(hook.clone()))
                                    .collect(),
                            ),
                        );
                        matchers.push(Value::Object(with_plugin));
                        for hook in &hooks {
                            details.push(to_hook_detail(
                                event_name,
                                hook,
                                matcher.as_deref(),
                                &source.source_path,
                            ));
                        }
                    }
                    Err(message) => diagnostics.push(
                        Diagnostic::error(
                            "plugin_hook_invalid",
                            format!("Invalid plugin hook matcher for {event_name}: {message}"),
                        )
                        .with_path(source.source_path.to_string_lossy())
                        .with_plugin_id(ctx.plugin_id),
                    ),
                }
            }
            if !matchers.is_empty() {
                let entry = events
                    .entry(event_name.clone())
                    .or_insert_with(|| Value::Array(Vec::new()));
                if let Value::Array(items) = entry {
                    items.extend(matchers);
                }
            }
        }
    }
    (details, events)
}

/// One validated matcher group: optional matcher plus its hook definitions.
type MatcherConfig = (Option<String>, Vec<Map<String, Value>>);

fn validate_matcher(config: &Value) -> Result<MatcherConfig, String> {
    let Some(record) = config.as_object() else {
        return Err("matcher config must be an object".to_string());
    };
    let matcher = match record.get("matcher") {
        Some(Value::String(value)) => Some(value.clone()),
        Some(Value::Null) | None => None,
        Some(_) => return Err("matcher must be a string".to_string()),
    };
    let Some(hooks) = record.get("hooks").and_then(Value::as_array) else {
        return Err("hooks must be an array".to_string());
    };
    if hooks.is_empty() {
        return Err("hooks must contain at least one hook".to_string());
    }
    let mut valid = Vec::new();
    for hook in hooks {
        valid.push(validate_hook(hook)?);
    }
    Ok((matcher, valid))
}

fn validate_hook(value: &Value) -> Result<Map<String, Value>, String> {
    let Some(hook) = value.as_object() else {
        return Err("hook must be an object".to_string());
    };
    let hook_type = hook.get("type").and_then(Value::as_str);
    match hook_type {
        Some("process") | Some("command") => {}
        _ => return Err("hook type must be process or command".to_string()),
    }
    match hook.get("command").and_then(Value::as_str) {
        Some(command) if !command.is_empty() => {}
        _ => return Err("hook command must be a non-empty string".to_string()),
    }
    if hook.contains_key("enabled") && !hook.get("enabled").is_some_and(Value::is_boolean) {
        return Err("hook enabled must be a boolean".to_string());
    }
    if let Some(timeout_ms) = hook.get("timeoutMs") {
        if !timeout_ms.as_i64().is_some_and(|value| value > 0) {
            return Err("hook timeoutMs must be a positive integer".to_string());
        }
    }
    if let Some(status) = hook.get("statusMessage") {
        if !status.is_string() {
            return Err("hook statusMessage must be a string".to_string());
        }
    }
    if hook_type == Some("process") {
        if let Some(args) = hook.get("args") {
            if !args
                .as_array()
                .is_some_and(|items| items.iter().all(Value::is_string))
            {
                return Err("hook args must be an array of strings".to_string());
            }
        }
    } else {
        if let Some(async_value) = hook.get("async") {
            if !async_value.is_boolean() {
                return Err("hook async must be a boolean".to_string());
            }
        }
        if let Some(shell) = hook.get("shell") {
            let valid =
                shell == &Value::Bool(true) || shell.as_str().is_some_and(|text| !text.is_empty());
            if !valid {
                return Err("hook shell must be true or a non-empty string".to_string());
            }
        }
        if let Some(timeout) = hook.get("timeout") {
            if !timeout.as_f64().is_some_and(|value| value > 0.0) {
                return Err("hook timeout must be positive".to_string());
            }
        }
    }
    Ok(hook.clone())
}

fn to_hook_detail(
    event: &str,
    hook: &Map<String, Value>,
    matcher: Option<&str>,
    source_path: &Path,
) -> Value {
    let mut detail = Map::new();
    detail.insert(
        "command".into(),
        hook.get("command").cloned().unwrap_or(Value::Null),
    );
    detail.insert("event".into(), Value::String(event.to_string()));
    detail.insert("runnable".into(), Value::Bool(true));
    detail.insert(
        "sourcePath".into(),
        Value::String(source_path.to_string_lossy().into_owned()),
    );
    detail.insert(
        "type".into(),
        hook.get("type").cloned().unwrap_or(Value::Null),
    );
    if let Some(matcher) = matcher {
        detail.insert("matcher".into(), Value::String(matcher.to_string()));
    }
    if let Some(status) = hook.get("statusMessage") {
        detail.insert("statusMessage".into(), status.clone());
    }
    if let Some(timeout_ms) = hook.get("timeoutMs") {
        detail.insert("timeoutMs".into(), timeout_ms.clone());
    }
    if hook.get("type").and_then(Value::as_str) == Some("process") {
        if let Some(args) = hook.get("args") {
            detail.insert("args".into(), args.clone());
        }
        return Value::Object(detail);
    }
    for key in ["async", "shell", "timeout"] {
        if let Some(value) = hook.get(key) {
            detail.insert(key.into(), value.clone());
        }
    }
    Value::Object(detail)
}

/// `collectInlineHookEvents` (manifest-only fallback used when no loaded plugin exists).
pub fn inline_hook_event_names(value: &Value) -> Vec<String> {
    let hooks_field = value
        .get("hooks")
        .filter(|hooks| hooks.is_object())
        .unwrap_or(value);
    let Some(record) = hooks_field.as_object() else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for key in record.keys() {
        if !names.contains(key) {
            names.push(key.clone());
        }
    }
    names
}
