//! Desktop provider handoff.
//!
//! The desktop composition root already passes the provider store files to every agent
//! spawn (`nc_code_BUILTIN_PROVIDER_CONFIG_FILE` / `nc_code_PERSONAL_PROVIDER_CONFIG_FILE`,
//! injected by `packages/provider-node/src/runtime-paths.ts`). The native host reads the
//! same two files and projects the active api-key provider into its static
//! [`ProviderConfig`], so a default flip can reach a model without the TypeScript layer
//! marshalling secrets through argv or a second config format.
//!
//! Supported subset (fail-closed for everything else):
//! - `access.type == "api-key"` or `"zhipu-coding-plan-api-key"` with a non-empty key
//!   (the desktop resolves coding-plan keys before writing the store);
//! - `api.type` in `anthropic-messages` / `openai-chat-completions` / `openai-responses`;
//! - no custom `api.headers` (the host request builder has no header hook yet).
//!
//! Account providers (OAuth/coding-plan) need a token-refresh bridge and are reported as
//! unavailable rather than silently spawning a model-less host.

use std::path::Path;

use serde_json::Value;

use crate::provider_config::ProviderConfig;

/// Env var carrying the built-in provider config file path (written by the desktop).
pub const BUILTIN_PROVIDER_CONFIG_ENV: &str = "ZCODE_BUILTIN_PROVIDER_CONFIG_FILE";
/// Env var carrying the personal provider config file path (written by the desktop).
pub const PERSONAL_PROVIDER_CONFIG_ENV: &str = "ZCODE_PERSONAL_PROVIDER_CONFIG_FILE";

const DEFAULT_MAX_TOKENS: u32 = 4_096;

/// Outcome of a desktop handoff attempt. `Unavailable` carries a human-readable reason so
/// the host can log why it started without a provider instead of failing silently.
#[derive(Debug)]
pub enum ProviderHandoff {
    Resolved(ProviderConfig),
    Unavailable(String),
}

/// Resolve the handoff provider from the spawn environment.
///
/// Returns `None` when neither env var is present (the caller did not ask for a desktop
/// handoff); returns `Unavailable` when files exist but no usable api-key provider can be
/// projected.
pub fn resolve_from_env() -> Option<ProviderHandoff> {
    let builtin = std::env::var_os(BUILTIN_PROVIDER_CONFIG_ENV).map(std::path::PathBuf::from);
    let personal = std::env::var_os(PERSONAL_PROVIDER_CONFIG_ENV).map(std::path::PathBuf::from);
    if builtin.is_none() && personal.is_none() {
        return None;
    }
    Some(resolve_from_files(personal.as_deref(), builtin.as_deref()))
}

/// Pure resolver over the two provider store files. Missing/unreadable files are treated
/// as empty layers; a malformed file is reported as unavailable (never guessed around).
pub fn resolve_from_files(personal: Option<&Path>, builtin: Option<&Path>) -> ProviderHandoff {
    let mut providers: Vec<ProviderLayer> = Vec::new();
    let mut models: Vec<ModelEntry> = Vec::new();
    let mut order: Vec<String> = Vec::new();
    let mut default_selection: Option<Selection> = None;

    for path in [builtin, personal].into_iter().flatten() {
        let layer = match read_layer(path) {
            Ok(layer) => layer,
            Err(reason) => return ProviderHandoff::Unavailable(reason),
        };
        providers.extend(layer.providers);
        models.extend(layer.models);
        order.extend(layer.order);
        if layer.default_selection.is_some() {
            default_selection = layer.default_selection;
        }
    }
    if providers.is_empty() {
        return ProviderHandoff::Unavailable(
            "provider store has no provider entries yet".to_string(),
        );
    }
    order.extend(providers.iter().map(|provider| provider.id.clone()));
    let mut ordered: Vec<&ProviderLayer> = Vec::new();
    for id in &order {
        if let Some(provider) = providers
            .iter()
            .find(|provider| &provider.id == id && !ordered.iter().any(|known| &known.id == id))
        {
            ordered.push(provider);
        }
    }
    for provider in &providers {
        if !ordered.iter().any(|known| known.id == provider.id) {
            ordered.push(provider);
        }
    }
    let candidates = selection_candidates(default_selection.as_ref(), &ordered, &models);
    let mut failures: Vec<String> = Vec::new();
    for selection in candidates {
        match project_provider(selection, &providers, &models) {
            Ok(config) => return ProviderHandoff::Resolved(config),
            Err(reason) => failures.push(reason),
        }
    }
    ProviderHandoff::Unavailable(if failures.is_empty() {
        "provider store has no enabled model to hand to the native host".to_string()
    } else {
        failures.join("; ")
    })
}

#[derive(Debug, Clone)]
struct ProviderLayer {
    id: String,
    api_type: Option<String>,
    base_url: Option<String>,
    headers: Vec<(String, String)>,
    access_type: Option<String>,
    api_key: Option<String>,
    model_ids: Vec<String>,
}

#[derive(Debug, Clone)]
struct ModelEntry {
    provider_id: String,
    model_id: String,
    enabled: Option<bool>,
    max_output_tokens: Option<u32>,
    reasoning_levels: Vec<String>,
}

#[derive(Debug, Clone)]
struct Selection {
    provider_id: String,
    model_id: Option<String>,
}

#[derive(Debug, Default)]
struct ParsedLayer {
    providers: Vec<ProviderLayer>,
    models: Vec<ModelEntry>,
    order: Vec<String>,
    default_selection: Option<Selection>,
}

fn read_layer(path: &Path) -> Result<ParsedLayer, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ParsedLayer::default())
        }
        Err(error) => {
            return Err(format!(
                "provider store {} is unreadable: {error}",
                path.display()
            ))
        }
    };
    let root: Value = serde_json::from_str(&text).map_err(|error| {
        format!(
            "provider store {} is not valid JSON: {error}",
            path.display()
        )
    })?;
    let config = root
        .get("config")
        .ok_or_else(|| format!("provider store {} has no config object", path.display()))?;
    let provider_rules = config
        .get("providerConfigRules")
        .and_then(|rules| rules.get("providerRules"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let providers = provider_rules
        .iter()
        .filter_map(parse_provider_rule)
        .collect::<Vec<ProviderLayer>>();
    let model_rules = config.get("modelConfigRules");
    let mut models: Vec<ModelEntry> = Vec::new();
    for key in ["providerModelRules", "manualProviderModelRules"] {
        if let Some(rules) = model_rules
            .and_then(|rules| rules.get(key))
            .and_then(Value::as_array)
        {
            models.extend(rules.iter().filter_map(parse_model_rule));
        }
    }
    let order = config
        .get("providerOrder")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    let default_selection = config
        .get("defaultModelSelection")
        .and_then(parse_selection);
    Ok(ParsedLayer {
        providers,
        models,
        order,
        default_selection,
    })
}

fn parse_provider_rule(rule: &Value) -> Option<ProviderLayer> {
    let id = rule.get("providerId")?.as_str()?.trim().to_string();
    if id.is_empty() {
        return None;
    }
    let config = rule.get("config")?;
    let api = config.get("api");
    let access = config.get("access");
    let model_ids = ["builtinModelIds", "personalModelIds", "modelOrder"]
        .into_iter()
        .filter_map(|key| config.get(key).and_then(Value::as_array))
        .flat_map(|ids| ids.iter().filter_map(Value::as_str).map(str::to_string))
        .collect();
    let headers: Vec<(String, String)> = api
        .and_then(|api| api.get("headers"))
        .and_then(Value::as_object)
        .map(|headers| {
            headers
                .iter()
                .filter_map(|(name, value)| value.as_str().map(|v| (name.clone(), v.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Some(ProviderLayer {
        id,
        api_type: api
            .and_then(|api| api.get("type"))
            .and_then(Value::as_str)
            .map(str::to_string),
        base_url: api
            .and_then(|api| api.get("baseUrl"))
            .and_then(Value::as_str)
            .map(str::to_string),
        headers,
        access_type: access
            .and_then(|access| access.get("type"))
            .and_then(Value::as_str)
            .map(str::to_string),
        api_key: access
            .and_then(|access| access.get("apiKey"))
            .and_then(Value::as_str)
            .map(str::to_string),
        model_ids,
    })
}

fn parse_model_rule(rule: &Value) -> Option<ModelEntry> {
    let provider_id = rule.get("providerId")?.as_str()?.trim().to_string();
    let model_id = rule.get("modelId")?.as_str()?.trim().to_string();
    if provider_id.is_empty() || model_id.is_empty() {
        return None;
    }
    let config = rule.get("config");
    let option_specs = config.and_then(|config| config.get("optionSpecs"));
    let max_output_tokens = option_specs
        .and_then(|specs| specs.get("maxOutputTokens"))
        .and_then(|limit| limit.get("max"))
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0);
    let reasoning_levels = option_specs
        .and_then(|specs| specs.get("reasoningLevel"))
        .and_then(|level| level.get("values"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter_map(Value::as_str)
        .filter(|level| !level.is_empty())
        .map(str::to_string)
        .collect();
    Some(ModelEntry {
        provider_id,
        model_id,
        enabled: config
            .and_then(|config| config.get("enabled"))
            .and_then(Value::as_bool),
        max_output_tokens,
        reasoning_levels,
    })
}

fn parse_selection(value: &Value) -> Option<Selection> {
    let provider_id = value.get("providerId")?.as_str()?.trim().to_string();
    if provider_id.is_empty() {
        return None;
    }
    Some(Selection {
        provider_id,
        model_id: value
            .get("modelId")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// The default selection first, then every ordered provider with its first enabled model.
fn selection_candidates(
    default_selection: Option<&Selection>,
    ordered: &[&ProviderLayer],
    models: &[ModelEntry],
) -> Vec<Selection> {
    let mut candidates = Vec::new();
    if let Some(selection) = default_selection {
        candidates.push(selection.clone());
    }
    for provider in ordered {
        let model_id = provider
            .model_ids
            .iter()
            .chain(
                models
                    .iter()
                    .filter(|model| model.provider_id == provider.id)
                    .filter(|model| model.enabled != Some(false))
                    .map(|model| &model.model_id),
            )
            .next()
            .cloned();
        candidates.push(Selection {
            provider_id: provider.id.clone(),
            model_id,
        });
    }
    candidates
}

fn project_provider(
    selection: Selection,
    providers: &[ProviderLayer],
    models: &[ModelEntry],
) -> Result<ProviderConfig, String> {
    let provider = providers
        .iter()
        .find(|provider| provider.id == selection.provider_id)
        .ok_or_else(|| format!("provider {} is not in the store", selection.provider_id))?;
    let api_type = provider.api_type.as_deref().unwrap_or("<missing>");
    let kind = match api_type {
        "anthropic-messages" => "anthropic",
        "openai-chat-completions" | "openai-responses" => "openai-compatible",
        other => {
            return Err(format!(
                "provider {} uses API type {other}, which the native host cannot speak",
                provider.id
            ))
        }
    };
    let base_url = provider
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| format!("provider {} has no baseUrl", provider.id))?;
    let headers = provider.headers.clone();
    if !matches!(
        provider.access_type.as_deref(),
        Some("api-key" | "zhipu-coding-plan-api-key")
    ) {
        return Err(format!(
            "provider {} uses access type {}, which needs the account-auth bridge (accepted: api-key, zhipu-coding-plan-api-key)",
            provider.id,
            provider.access_type.as_deref().unwrap_or("<missing>")
        ));
    }
    let api_key = provider
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| format!("provider {} has no apiKey", provider.id))?
        .to_string();
    let provider_models: Vec<&ModelEntry> = models
        .iter()
        .filter(|model| model.provider_id == provider.id && model.enabled != Some(false))
        .collect();
    let selected_model = match selection.model_id {
        Some(model_id) => {
            if provider_models
                .iter()
                .any(|model| model.model_id == model_id)
                || provider.model_ids.iter().any(|known| known == &model_id)
            {
                model_id
            } else {
                return Err(format!("provider {} has no model {model_id}", provider.id));
            }
        }
        None => provider_models
            .first()
            .map(|model| model.model_id.clone())
            .or_else(|| provider.model_ids.first().cloned())
            .ok_or_else(|| format!("provider {} has no model", provider.id))?,
    };
    let selected_entry = provider_models
        .iter()
        .find(|model| model.model_id == selected_model);
    let max_tokens = selected_entry
        .and_then(|model| model.max_output_tokens)
        .unwrap_or(DEFAULT_MAX_TOKENS);
    let reasoning_levels = selected_entry
        .map(|model| model.reasoning_levels.clone())
        .unwrap_or_default();
    let extra_models = provider_models
        .iter()
        .map(|model| model.model_id.clone())
        .filter(|model_id| model_id != &selected_model)
        .collect();
    Ok(ProviderConfig {
        provider_id: Some(provider.id.clone()),
        kind: kind.to_string(),
        base_url: base_url.to_string(),
        api_key,
        model: selected_model,
        max_tokens,
        auth_style: Some(if kind == "anthropic" {
            "x-api-key".to_string()
        } else {
            "bearer".to_string()
        }),
        stream: true,
        extra_models,
        reasoning_levels,
        headers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(provider_rules: Value, model_rules: Value) -> serde_json::Value {
        serde_json::json!({
            "schemaVersion": 3,
            "config": {
                "providerConfigRules": { "providerRules": provider_rules },
                "modelConfigRules": {
                    "providerModelRules": model_rules,
                    "manualProviderModelRules": []
                },
                "providerOrder": ["personal:0"]
            }
        })
    }

    fn write_store(name: &str, value: &Value) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "zcode-provider-handoff-{name}-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, value.to_string()).expect("write provider store");
        path
    }

    #[test]
    fn projects_an_api_key_provider_with_models_and_reasoning() {
        let personal = store(
            serde_json::json!([{
                "providerId": "personal:0",
                "config": {
                    "access": {"type": "api-key", "apiKey": "test-key-not-a-real-credential"},
                    "api": {"type": "anthropic-messages", "baseUrl": "https://provider.example/v1"}
                }
            }]),
            serde_json::json!([
                {
                    "providerId": "personal:0",
                    "modelId": "model-a",
                    "config": {
                        "enabled": true,
                        "optionSpecs": {
                            "reasoningLevel": {"values": ["low", "high"]},
                            "maxOutputTokens": {"max": 8192}
                        }
                    }
                },
                {"providerId": "personal:0", "modelId": "model-b", "config": {"enabled": false}}
            ]),
        );
        let path = write_store("project", &personal);
        let handoff = resolve_from_files(Some(&path), None);
        let _ = std::fs::remove_file(path);
        let ProviderHandoff::Resolved(config) = handoff else {
            panic!("expected a resolved provider: {handoff:?}");
        };
        assert_eq!(config.provider_id.as_deref(), Some("personal:0"));
        assert_eq!(config.kind, "anthropic");
        assert_eq!(config.base_url, "https://provider.example/v1");
        assert_eq!(config.api_key, "test-key-not-a-real-credential");
        assert_eq!(config.model, "model-a");
        assert_eq!(config.max_tokens, 8192);
        assert_eq!(config.auth_style.as_deref(), Some("x-api-key"));
        assert_eq!(config.extra_models, Vec::<String>::new());
        assert_eq!(config.reasoning_levels, vec!["low", "high"]);
    }

    #[test]
    fn projects_a_desktop_resolved_coding_plan_api_key() {
        let personal = store(
            serde_json::json!([{
                "providerId": "personal:0",
                "config": {
                    "access": {
                        "type": "zhipu-coding-plan-api-key",
                        "apiKey": "test-coding-plan-key-not-a-real-credential"
                    },
                    "api": {"type": "openai-chat-completions", "baseUrl": "https://z.example/v1"}
                }
            }]),
            serde_json::json!([
                {"providerId": "personal:0", "modelId": "glm-5", "config": {"enabled": true}}
            ]),
        );
        let path = write_store("coding-plan", &personal);
        let handoff = resolve_from_files(Some(&path), None);
        let _ = std::fs::remove_file(path);
        let ProviderHandoff::Resolved(config) = handoff else {
            panic!("expected a resolved coding-plan provider: {handoff:?}");
        };
        assert_eq!(config.provider_id.as_deref(), Some("personal:0"));
        assert_eq!(config.kind, "openai-compatible");
        assert_eq!(config.base_url, "https://z.example/v1");
        assert_eq!(config.api_key, "test-coding-plan-key-not-a-real-credential");
        assert_eq!(config.model, "glm-5");
        assert_eq!(config.auth_style.as_deref(), Some("bearer"));
    }

    #[test]
    fn prefers_the_default_model_selection() {
        let personal = store(
            serde_json::json!([{
                "providerId": "personal:0",
                "config": {
                    "access": {"type": "api-key", "apiKey": "test-key-not-a-real-credential"},
                    "api": {"type": "openai-chat-completions", "baseUrl": "https://provider.example/v1"}
                }
            }]),
            serde_json::json!([
                {"providerId": "personal:0", "modelId": "model-a", "config": {"enabled": true}},
                {"providerId": "personal:0", "modelId": "model-b", "config": {"enabled": true}}
            ]),
        );
        let mut value = personal.clone();
        value["config"]["defaultModelSelection"] =
            serde_json::json!({"providerId": "personal:0", "modelId": "model-b"});
        let path = write_store("default", &value);
        let handoff = resolve_from_files(Some(&path), None);
        let _ = std::fs::remove_file(path);
        let ProviderHandoff::Resolved(config) = handoff else {
            panic!("expected a resolved provider: {handoff:?}");
        };
        assert_eq!(config.model, "model-b");
        assert_eq!(config.auth_style.as_deref(), Some("bearer"));
        assert_eq!(config.extra_models, vec!["model-a"]);
    }

    #[test]
    fn account_access_is_a_named_failure_and_custom_headers_resolve() {
        let account = store(
            serde_json::json!([{
                "providerId": "zai",
                "config": {
                    "access": {"type": "zhipu-account", "accountType": "coding-plan"},
                    "api": {"type": "anthropic-messages", "baseUrl": "https://z.example/v1"}
                }
            }]),
            serde_json::json!([]),
        );
        let path = write_store("account", &account);
        let handoff = resolve_from_files(Some(&path), None);
        let _ = std::fs::remove_file(path);
        let ProviderHandoff::Unavailable(reason) = handoff else {
            panic!("account access must not resolve: {handoff:?}");
        };
        assert!(reason.contains("account-auth bridge"), "{reason}");

        let headers = store(
            serde_json::json!([{
                "providerId": "personal:0",
                "config": {
                    "access": {"type": "api-key", "apiKey": "test-key-not-a-real-credential"},
                    "api": {
                        "type": "anthropic-messages",
                        "baseUrl": "https://provider.example/v1",
                        "headers": {"X-Custom": "1"}
                    }
                }
            }]),
            serde_json::json!([
                {"providerId": "personal:0", "modelId": "model-a", "config": {"enabled": true}}
            ]),
        );
        let path = write_store("headers", &headers);
        let handoff = resolve_from_files(Some(&path), None);
        let _ = std::fs::remove_file(path);
        let ProviderHandoff::Resolved(config) = handoff else {
            panic!("custom headers must resolve: {handoff:?}");
        };
        assert!(
            config
                .headers
                .iter()
                .any(|(name, value)| name == "X-Custom" && value == "1"),
            "custom header projected: {:?}",
            config.headers
        );
    }

    #[test]
    fn missing_files_yield_a_named_unavailable() {
        let missing = std::env::temp_dir().join("zcode-provider-handoff-does-not-exist.json");
        let handoff = resolve_from_files(Some(&missing), None);
        let ProviderHandoff::Unavailable(reason) = handoff else {
            panic!("missing store must not resolve: {handoff:?}");
        };
        assert!(reason.contains("no provider entries"), "{reason}");
    }

    #[test]
    fn malformed_store_is_reported_not_guessed() {
        let path = std::env::temp_dir().join(format!(
            "zcode-provider-handoff-malformed-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, "{ not json").expect("write malformed store");
        let handoff = resolve_from_files(Some(&path), None);
        let _ = std::fs::remove_file(path);
        let ProviderHandoff::Unavailable(reason) = handoff else {
            panic!("malformed store must not resolve: {handoff:?}");
        };
        assert!(reason.contains("not valid JSON"), "{reason}");
    }
}
