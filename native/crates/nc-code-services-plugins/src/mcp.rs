//! Plugin MCP server definitions and resolution — subset of
//! `adapters/src/plugins/mcp.ts` that the plugin-management channel needs:
//! component names for `list/describe`, and the validation diagnostics for
//! `validate`.
//!
//! The resolver keeps the TypeScript contract (definition discovery from `.mcp.json`
//! and `manifest.mcpServers`, `${…}` template expansion, official-auth strictness) but
//! does not build a live MCP transport — the plugin-management channel never starts
//! servers.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::diagnostics::Diagnostic;
use crate::fsutil::resolve_inside;
use crate::types::OFFICIAL_MARKETPLACE_ID;

pub const ZCODE_PLUGIN_ID_ENV_KEY: &str = "ZCODE_PLUGIN_ID";

const SUPPORTED_TYPES: [&str; 3] = ["stdio", "http", "sse"];
const RESERVED_OFFICIAL_HEADERS: [&str; 7] = [
    "authorization",
    "x-bigmodel-authorization",
    "bigmodel-target-type",
    "bigmodel-organization",
    "bigmodel-project",
    "x-coding-plan-api-key",
    "mcp-session-id",
];

pub struct McpPluginContext<'a> {
    pub root_path: &'a Path,
    pub manifest: Option<&'a Map<String, Value>>,
    pub manifest_path: &'a Path,
    pub plugin_id: &'a str,
    pub marketplace: &'a str,
}

fn plugin_name(ctx: &McpPluginContext<'_>) -> String {
    ctx.manifest
        .and_then(|manifest| manifest.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// `loadPluginMcpServerDefinitions`: `.mcp.json` first, then `manifest.mcpServers`.
pub fn load_plugin_mcp_server_definitions(
    ctx: &McpPluginContext<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Map<String, Value> {
    let mut definitions =
        load_mcp_servers_from_file(&ctx.root_path.join(".mcp.json"), ctx, diagnostics);
    if let Some(spec) = ctx.manifest.and_then(|manifest| manifest.get("mcpServers")) {
        let from_manifest = load_mcp_servers_from_spec(spec, ctx, diagnostics);
        for (name, server) in from_manifest {
            definitions.insert(name, server);
        }
    }
    definitions
}

fn load_mcp_servers_from_spec(
    spec: &Value,
    ctx: &McpPluginContext<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Map<String, Value> {
    match spec {
        Value::String(relative) => {
            let Some(path) = resolve_inside(ctx.root_path, relative.trim_start_matches("./"))
            else {
                diagnostics.push(
                    Diagnostic::error(
                        "plugin_component_path_invalid",
                        format!("Plugin mcpServers path escapes plugin root: {relative}"),
                    )
                    .with_path(ctx.manifest_path.to_string_lossy())
                    .with_plugin_id(ctx.plugin_id),
                );
                return Map::new();
            };
            load_mcp_servers_from_file(&path, ctx, diagnostics)
        }
        Value::Array(items) => {
            let mut merged = Map::new();
            for item in items {
                for (name, server) in load_mcp_servers_from_spec(item, ctx, diagnostics) {
                    merged.insert(name, server);
                }
            }
            merged
        }
        _ => normalize_mcp_servers_shape(spec, ctx, diagnostics),
    }
}

fn load_mcp_servers_from_file(
    path: &Path,
    ctx: &McpPluginContext<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Map<String, Value> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Map::new(),
        Err(error) => {
            diagnostics.push(
                Diagnostic::error(
                    "plugin_mcp_read_failed",
                    format!("Failed to read MCP config: {error}"),
                )
                .with_path(path.to_string_lossy())
                .with_plugin_id(ctx.plugin_id),
            );
            return Map::new();
        }
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(value) => normalize_mcp_servers_shape(&value, ctx, diagnostics),
        Err(error) => {
            diagnostics.push(
                Diagnostic::error(
                    "plugin_mcp_read_failed",
                    format!("Failed to read MCP config: {error}"),
                )
                .with_path(path.to_string_lossy())
                .with_plugin_id(ctx.plugin_id),
            );
            Map::new()
        }
    }
}

fn normalize_mcp_servers_shape(
    value: &Value,
    ctx: &McpPluginContext<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Map<String, Value> {
    let Some(record) = value.as_object() else {
        diagnostics.push(
            Diagnostic::error("plugin_mcp_invalid", "Plugin MCP config must be an object")
                .with_path(ctx.manifest_path.to_string_lossy())
                .with_plugin_id(ctx.plugin_id),
        );
        return Map::new();
    };
    let servers = record
        .get("mcpServers")
        .and_then(Value::as_object)
        .unwrap_or(record);
    servers
        .iter()
        .filter(|(_, config)| config.is_object())
        .map(|(name, config)| (name.clone(), config.clone()))
        .collect()
}

enum ResolveError {
    Variable(String),
    Invalid(String),
}

/// `resolvePluginMcpServers`: returns the namespaced server configs
/// (`plugin:<name>:<server>`); invalid servers are reported and dropped.
pub fn resolve_plugin_mcp_servers(
    definitions: &Map<String, Value>,
    data_path: &Path,
    working_directory: &Path,
    ctx: &McpPluginContext<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Map<String, Value> {
    let context = VariableContext {
        data_path: data_path.to_path_buf(),
        root_path: ctx.root_path.to_path_buf(),
        working_directory: working_directory.to_path_buf(),
        manifest: ctx.manifest,
    };
    let mut result = Map::new();
    let name = plugin_name(ctx);
    for (server_name, server) in definitions {
        match resolve_server_config(server, &context, server_name, ctx) {
            Ok(config) => {
                result.insert(format!("plugin:{name}:{server_name}"), config);
            }
            Err(error) => {
                let (code, message) = match error {
                    ResolveError::Variable(message) => ("plugin_variable_missing", message),
                    ResolveError::Invalid(message) => ("plugin_mcp_server_disabled", message),
                };
                diagnostics.push(
                    Diagnostic::error(code, message)
                        .with_path(ctx.manifest_path.to_string_lossy())
                        .with_plugin_id(ctx.plugin_id),
                );
            }
        }
    }
    result
}

struct VariableContext<'a> {
    data_path: PathBuf,
    root_path: PathBuf,
    working_directory: PathBuf,
    manifest: Option<&'a Map<String, Value>>,
}

impl VariableContext<'_> {
    fn user_config_defaults(&self) -> Map<String, Value> {
        let mut defaults = Map::new();
        if let Some(user_config) = self
            .manifest
            .and_then(|manifest| manifest.get("userConfig"))
            .and_then(Value::as_object)
        {
            for (key, option) in user_config {
                if let Some(default) = option.get("default") {
                    if is_option_value(default) {
                        defaults.insert(key.clone(), default.clone());
                    }
                }
            }
        }
        defaults
    }

    fn user_config_sensitive(&self, key: &str) -> bool {
        self.manifest
            .and_then(|manifest| manifest.get("userConfig"))
            .and_then(|config| config.get(key))
            .and_then(|option| option.get("sensitive"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

fn is_option_value(value: &Value) -> bool {
    value.is_string() || value.is_number() || value.is_boolean()
}

fn resolve_server_config(
    server: &Value,
    context: &VariableContext<'_>,
    mcp_key: &str,
    ctx: &McpPluginContext<'_>,
) -> Result<Value, ResolveError> {
    let record = server
        .as_object()
        .ok_or_else(|| ResolveError::Invalid("MCP server config must be an object".to_string()))?;
    let server_type = record
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| infer_type(record));
    if !SUPPORTED_TYPES.contains(&server_type.as_str()) {
        return Err(ResolveError::Invalid(format!(
            "Unsupported MCP transport: {server_type}"
        )));
    }
    let source = if ctx.marketplace == OFFICIAL_MARKETPLACE_ID {
        "builtin"
    } else {
        "plugin"
    };
    let official_auth = parse_official_auth(record.get("auth"), mcp_key)?;

    if server_type == "stdio" {
        let command = require_string(record.get("command"), "stdio MCP server requires command")?;
        if official_auth.is_some() && record.get("oauth").is_some() {
            return Err(ResolveError::Invalid(format!(
                "MCP server {mcp_key}: nc_code_official auth cannot be combined with oauth"
            )));
        }
        let mut env = Map::new();
        env.insert(
            "CLAUDE_PROJECT_DIR".into(),
            Value::String(context.working_directory.to_string_lossy().into_owned()),
        );
        env.insert(
            "ZCODE_PROJECT_DIR".into(),
            Value::String(context.working_directory.to_string_lossy().into_owned()),
        );
        env.insert(
            "CLAUDE_PLUGIN_DATA".into(),
            Value::String(context.data_path.to_string_lossy().into_owned()),
        );
        env.insert(
            "ZCODE_PLUGIN_DATA".into(),
            Value::String(context.data_path.to_string_lossy().into_owned()),
        );
        env.insert(
            "CLAUDE_PLUGIN_ROOT".into(),
            Value::String(context.root_path.to_string_lossy().into_owned()),
        );
        env.insert(
            "ZCODE_PLUGIN_ROOT".into(),
            Value::String(context.root_path.to_string_lossy().into_owned()),
        );
        // Manifest/user env overrides the defaults, then the resolver-owned plugin id wins.
        for (key, value) in
            resolve_string_record(record.get("env").and_then(Value::as_object), context, true)?
        {
            env.insert(key, value);
        }
        env.insert(
            ZCODE_PLUGIN_ID_ENV_KEY.into(),
            Value::String(ctx.plugin_id.to_string()),
        );

        let mut config = Map::new();
        config.insert("type".into(), Value::String("stdio".into()));
        config.insert(
            "command".into(),
            Value::String(resolve_template(&command, context, false)?),
        );
        if let Some(args) = record.get("args").and_then(Value::as_array) {
            let mut resolved = Vec::new();
            for arg in args.iter().filter_map(Value::as_str) {
                resolved.push(Value::String(resolve_template(arg, context, false)?));
            }
            config.insert("args".into(), Value::Array(resolved));
        }
        if let Some(cwd) = record.get("cwd").and_then(Value::as_str) {
            config.insert(
                "cwd".into(),
                Value::String(resolve_template(cwd, context, false)?),
            );
        }
        if let Some(enabled) = record.get("enabled").and_then(Value::as_bool) {
            config.insert("enabled".into(), Value::Bool(enabled));
        }
        config.insert("env".into(), Value::Object(env));
        config.insert("source".into(), serde_json::json!({"kind": source}));
        if let Some(timeout) = record.get("timeoutMs").and_then(Value::as_i64) {
            config.insert("timeoutMs".into(), Value::from(timeout));
        }
        if let Some(auth) = official_auth {
            config.insert("auth".into(), auth);
            config.insert(
                "official".into(),
                serde_json::json!({"mcpKey": mcp_key, "pluginId": ctx.plugin_id, "source": "plugin"}),
            );
        }
        return Ok(Value::Object(config));
    }

    let url = require_string(
        record.get("url"),
        &format!("{server_type} MCP server requires url"),
    )?;
    let headers = match record.get("headers").and_then(Value::as_object) {
        Some(headers) => Some(resolve_string_record(Some(headers), context, true)?),
        None => None,
    };
    let oauth = resolve_oauth(record.get("oauth"), context)?;
    if let Some(auth) = official_auth {
        if oauth.is_some() {
            return Err(ResolveError::Invalid(format!(
                "MCP server {mcp_key}: nc_code_official auth cannot be combined with oauth"
            )));
        }
        let reserved = reserved_headers(headers.as_ref());
        if !reserved.is_empty() {
            return Err(ResolveError::Invalid(format!(
                "MCP server {mcp_key}: static headers must not contain reserved header(s): {}",
                reserved.join(", ")
            )));
        }
        let mut config = Map::new();
        config.insert("type".into(), Value::String("http".into()));
        config.insert(
            "url".into(),
            Value::String(resolve_template(&url, context, false)?),
        );
        if let Some(enabled) = record.get("enabled").and_then(Value::as_bool) {
            config.insert("enabled".into(), Value::Bool(enabled));
        }
        if let Some(headers) = headers {
            config.insert("headers".into(), Value::Object(headers));
        }
        config.insert("auth".into(), auth);
        config.insert("source".into(), serde_json::json!({"kind": source}));
        config.insert(
            "official".into(),
            serde_json::json!({"mcpKey": mcp_key, "pluginId": ctx.plugin_id, "source": "plugin"}),
        );
        if let Some(timeout) = record.get("timeoutMs").and_then(Value::as_i64) {
            config.insert("timeoutMs".into(), Value::from(timeout));
        }
        return Ok(Value::Object(config));
    }

    let mut config = Map::new();
    config.insert("type".into(), Value::String(server_type));
    config.insert(
        "url".into(),
        Value::String(resolve_template(&url, context, false)?),
    );
    if let Some(enabled) = record.get("enabled").and_then(Value::as_bool) {
        config.insert("enabled".into(), Value::Bool(enabled));
    }
    if let Some(headers) = headers {
        config.insert("headers".into(), Value::Object(headers));
    }
    if let Some(oauth) = oauth {
        config.insert("oauth".into(), oauth);
    }
    config.insert("source".into(), serde_json::json!({"kind": source}));
    if let Some(timeout) = record.get("timeoutMs").and_then(Value::as_i64) {
        config.insert("timeoutMs".into(), Value::from(timeout));
    }
    Ok(Value::Object(config))
}

fn reserved_headers(headers: Option<&Map<String, Value>>) -> Vec<String> {
    let Some(headers) = headers else {
        return Vec::new();
    };
    let mut hits: Vec<String> = headers
        .keys()
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| RESERVED_OFFICIAL_HEADERS.contains(&name.as_str()))
        .collect();
    hits.sort();
    hits.dedup();
    hits
}

fn parse_official_auth(
    value: Option<&Value>,
    mcp_key: &str,
) -> Result<Option<Value>, ResolveError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(record) = value.as_object() else {
        return Err(ResolveError::Invalid(format!(
            "MCP server {mcp_key}: auth must be an object"
        )));
    };
    if record.get("type").and_then(Value::as_str) != Some("ZCODE_official") {
        return Err(ResolveError::Invalid(format!(
            "MCP server {mcp_key}: unsupported auth type: {}",
            record
                .get("type")
                .map(value_to_text)
                .unwrap_or_else(|| "undefined".to_string())
        )));
    }
    if record.get("provider").and_then(Value::as_str) != Some("jwt_token") {
        return Err(ResolveError::Invalid(format!(
            "MCP server {mcp_key}: unsupported auth provider: {}",
            record
                .get("provider")
                .map(value_to_text)
                .unwrap_or_else(|| "undefined".to_string())
        )));
    }
    Ok(Some(serde_json::json!({
        "type": "ZCODE_official",
        "provider": "jwt_token",
    })))
}

fn resolve_oauth(
    value: Option<&Value>,
    context: &VariableContext<'_>,
) -> Result<Option<Value>, ResolveError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(record) = value.as_object() else {
        return Ok(None);
    };
    let oauth_type = record.get("type").and_then(Value::as_str).unwrap_or("");
    if oauth_type == "client_credentials" {
        return Ok(Some(serde_json::json!({
            "type": "client_credentials",
            "clientId": resolve_template(
                &require_string(record.get("clientId"), "MCP OAuth client_credentials requires clientId")?,
                context,
                false,
            )?,
            "clientSecret": resolve_template(
                &require_string(record.get("clientSecret"), "MCP OAuth client_credentials requires clientSecret")?,
                context,
                true,
            )?,
        })));
    }
    if oauth_type == "authorization_code" {
        let mut config = Map::new();
        config.insert("type".into(), Value::String("authorization_code".into()));
        for (source_key, target_key, sensitive) in [
            ("clientId", "clientId", false),
            ("clientSecret", "clientSecret", true),
            ("clientName", "clientName", false),
            ("redirectPath", "redirectPath", false),
            ("scope", "scope", false),
        ] {
            if let Some(text) = record.get(source_key).and_then(Value::as_str) {
                config.insert(
                    target_key.into(),
                    Value::String(resolve_template(text, context, sensitive)?),
                );
            }
        }
        return Ok(Some(Value::Object(config)));
    }
    Err(ResolveError::Invalid(format!(
        "Unsupported MCP OAuth type: {}",
        record.get("type").map(value_to_text).unwrap_or_default()
    )))
}

fn infer_type(record: &Map<String, Value>) -> String {
    if record.get("command").and_then(Value::as_str).is_some() {
        "stdio".to_string()
    } else {
        "http".to_string()
    }
}

fn require_string(value: Option<&Value>, message: &str) -> Result<String, ResolveError> {
    match value.and_then(Value::as_str) {
        Some(text) if !text.is_empty() => Ok(text.to_string()),
        _ => Err(ResolveError::Invalid(message.to_string())),
    }
}

fn resolve_string_record(
    record: Option<&Map<String, Value>>,
    context: &VariableContext<'_>,
    allow_sensitive: bool,
) -> Result<Map<String, Value>, ResolveError> {
    let mut result = Map::new();
    for (key, value) in record.into_iter().flatten() {
        if let Some(text) = value.as_str() {
            result.insert(
                key.clone(),
                Value::String(resolve_template(text, context, allow_sensitive)?),
            );
        }
    }
    Ok(result)
}

fn resolve_template(
    value: &str,
    context: &VariableContext<'_>,
    allow_sensitive: bool,
) -> Result<String, ResolveError> {
    let mut output = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            output.push_str(&rest[start..]);
            return Ok(output);
        };
        let name = &after[..end];
        output.push_str(&resolve_variable(name, context, allow_sensitive)?);
        rest = &after[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn resolve_variable(
    name: &str,
    context: &VariableContext<'_>,
    allow_sensitive: bool,
) -> Result<String, ResolveError> {
    match name {
        "CLAUDE_PLUGIN_ROOT" | "ZCODE_PLUGIN_ROOT" => {
            return Ok(context.root_path.to_string_lossy().into_owned())
        }
        "CLAUDE_PLUGIN_DATA" | "ZCODE_PLUGIN_DATA" => {
            return Ok(context.data_path.to_string_lossy().into_owned())
        }
        "CLAUDE_PROJECT_DIR" | "ZCODE_PROJECT_DIR" => {
            return Ok(context.working_directory.to_string_lossy().into_owned())
        }
        "CLAUDE_CODE_SESSION_ID" | "CLAUDE_SESSION_ID" | "ZCODE_SESSION_ID" => {
            return Err(ResolveError::Variable(format!(
                "Plugin variable requires a runtime session context: {name}"
            )))
        }
        "CLAUDE_SKILL_DIR" | "ZCODE_SKILL_DIR" => {
            return Err(ResolveError::Variable(format!(
                "Plugin variable requires a skill context: {name}"
            )))
        }
        _ => {}
    }
    if let Some(key) = name.strip_prefix("user_config.") {
        if context.user_config_sensitive(key) && !allow_sensitive {
            return Err(ResolveError::Variable(format!(
                "Sensitive plugin user_config value cannot be used in this field: {key}"
            )));
        }
        if let Some(value) = context.user_config_defaults().get(key) {
            return Ok(value_to_text(value));
        }
        return Err(ResolveError::Variable(format!(
            "Missing plugin user_config value: {key}"
        )));
    }
    if name.starts_with("ZCODE_") {
        return std::env::var(name)
            .map_err(|_| ResolveError::Variable(format!("Missing environment variable: {name}")));
    }
    if allow_sensitive && is_env_name(name) {
        return std::env::var(name)
            .map_err(|_| ResolveError::Variable(format!("Missing environment variable: {name}")));
    }
    Ok(format!("${{{name}}}"))
}

fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn value_to_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx<'a>(
        root: &'a Path,
        manifest: &'a Map<String, Value>,
        path: &'a Path,
    ) -> McpPluginContext<'a> {
        McpPluginContext {
            root_path: root,
            manifest: Some(manifest),
            manifest_path: path,
            plugin_id: "demo@mp",
            marketplace: "mp",
        }
    }

    #[test]
    fn manifest_definitions_are_namespaced_when_resolved() {
        let root = Path::new("/tmp/plugin");
        let mut manifest = Map::new();
        manifest.insert("name".into(), json!("demo"));
        manifest.insert(
            "mcpServers".into(),
            json!({"docs": {"type": "stdio", "command": "node", "args": ["${ZCODE_PLUGIN_ROOT}/a.js"]}}),
        );
        let path = Path::new("/tmp/plugin/.zcode-plugin/plugin.json");
        let context = ctx(root, &manifest, path);
        let mut diagnostics = Vec::new();
        let definitions = load_plugin_mcp_server_definitions(&context, &mut diagnostics);
        assert!(diagnostics.is_empty());
        let resolved = resolve_plugin_mcp_servers(
            &definitions,
            Path::new("/tmp/data"),
            Path::new("/tmp/work"),
            &context,
            &mut diagnostics,
        );
        let server = resolved.get("plugin:demo:docs").expect("namespaced server");
        assert_eq!(server["args"][0], json!("/tmp/plugin/a.js"));
        assert_eq!(server["env"][ZCODE_PLUGIN_ID_ENV_KEY], json!("demo@mp"));
    }

    #[test]
    fn unsupported_transport_is_a_disabled_diagnostic() {
        let root = Path::new("/tmp/plugin");
        let mut manifest = Map::new();
        manifest.insert("name".into(), json!("demo"));
        manifest.insert(
            "mcpServers".into(),
            json!({"x": {"type": "ws", "url": "wss://x"}}),
        );
        let path = Path::new("/tmp/plugin/plugin.json");
        let context = ctx(root, &manifest, path);
        let mut diagnostics = Vec::new();
        let definitions = load_plugin_mcp_server_definitions(&context, &mut diagnostics);
        let resolved = resolve_plugin_mcp_servers(
            &definitions,
            Path::new("/tmp/data"),
            Path::new("/tmp/work"),
            &context,
            &mut diagnostics,
        );
        assert!(resolved.is_empty());
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "plugin_mcp_server_disabled"));
    }

    #[test]
    fn unsupported_manifest_field_does_not_leak_into_names() {
        let root = Path::new("/tmp/plugin");
        let manifest = Map::new();
        let path = Path::new("/tmp/plugin/plugin.json");
        let context = ctx(root, &manifest, path);
        let mut diagnostics = Vec::new();
        let definitions = load_plugin_mcp_server_definitions(&context, &mut diagnostics);
        assert!(definitions.is_empty());
    }
}
