//! `model-selection` channel — provider/model candidates derived from the personal
//! provider store. Any provider configured through `provider-settings` appears here.

use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{
    CancelToken, Channel, ChannelContext, ChannelFailure, EventBus, EventSubscription,
};
use nc_code_rpc::WireValue;
use nc_code_service_core::{ServiceArgs, ServiceError};
use serde_json::{json, Value};

use crate::store::{
    model_effective_config, provider_effective_config, ProviderStore, StoreSnapshot,
};

pub struct ModelSelectionService {
    store: Arc<ProviderStore>,
    events: EventBus,
}

impl ModelSelectionService {
    pub fn new(store: Arc<ProviderStore>, events: EventBus) -> Arc<Self> {
        Arc::new(Self { store, events })
    }

    pub async fn view(&self, requested: Option<&Value>) -> Result<Value, ServiceError> {
        let snapshot = self.store.snapshot().await?;
        Ok(build_view(&snapshot, requested))
    }
}

#[async_trait]
impl Channel for ModelSelectionService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        match command {
            "getView" => {
                let input = args.json(0)?;
                let requested = input
                    .as_ref()
                    .and_then(|value| value.get("selection"))
                    .filter(|value| !value.is_null());
                Ok(WireValue::Json(self.view(requested).await?))
            }
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }

    async fn listen(
        &self,
        _ctx: &ChannelContext,
        event: &str,
        _arg: WireValue,
    ) -> Result<EventSubscription, ChannelFailure> {
        match event {
            "onDidChange" => Ok(EventSubscription::from_broadcast(self.events.subscribe())),
            other => Err(ChannelFailure::event_not_found(other)),
        }
    }
}

pub fn build_view(snapshot: &StoreSnapshot, requested: Option<&Value>) -> Value {
    let providers: Vec<Value> = snapshot
        .providers
        .iter()
        .filter(|provider| provider.enabled)
        .filter_map(|provider| {
            let config = provider_effective_config(provider, snapshot);
            if !crate::store::provider_issues(&config).is_empty() {
                return None;
            }
            let models: Vec<Value> = provider
                .models
                .iter()
                .filter(|model| model.enabled)
                .map(|model| {
                    json!({
                        "modelId": model.model_id,
                        "config": model_effective_config(model),
                    })
                })
                .collect();
            if models.is_empty() {
                return None;
            }
            let mut item = json!({
                "providerId": provider.provider_id,
                "providerName": provider.provider_name,
                "config": config,
                "models": models,
            });
            if let Some(template_id) = &provider.template_id {
                item["templateId"] = Value::String(template_id.clone());
            }
            Some(item)
        })
        .collect();

    let preferred = requested
        .filter(|selection| selection_is_valid(selection, &providers))
        .map(|selection| complete_selection(selection, &providers))
        .or_else(|| {
            snapshot
                .default_model_selection
                .as_ref()
                .filter(|selection| selection_is_valid(selection, &providers))
                .map(|selection| complete_selection(selection, &providers))
        })
        .or_else(|| {
            providers.first().and_then(|provider| {
                let provider_id = provider["providerId"].as_str()?;
                let model = provider["models"].as_array()?.first()?;
                let model_id = model.get("modelId")?.as_str()?;
                let mut selection = json!({"providerId": provider_id, "modelId": model_id});
                if let Some(level) = default_reasoning_level(model) {
                    selection["options"] = json!({"reasoningLevel": level});
                }
                Some(selection)
            })
        });

    let mut view = json!({
        "revision": snapshot.revision,
        "providers": providers,
    });
    if let Some(preferred) = preferred {
        view["preferredSelection"] = preferred;
    }
    // The response of `getView({selection})` must come with effectiveSelection: the draft submission gate reads it,
    // Instead of inferring preferredSelection (the two have different semantics: one is "the execution intention of the current request",
    // One is "the default choice when there is no intention").
    if let Some(requested) = requested {
        match resolve_effective(requested, &providers) {
            Ok(effective) => view["effectiveSelection"] = effective,
            Err(issue) => {
                view["effectiveSelection"] = Value::Null;
                view["selectionIssue"] = json!(issue);
            }
        }
    }
    view
}

/// The execution intention of requesting selection: when invalid, the TS `selectionIssue` string with the same name is given and is not replaced silently.
fn resolve_effective(selection: &Value, providers: &[Value]) -> Result<Value, &'static str> {
    let Some(provider_id) = selection.get("providerId").and_then(Value::as_str) else {
        return Err("selection-missing");
    };
    let Some(model_id) = selection.get("modelId").and_then(Value::as_str) else {
        return Err("selection-missing");
    };
    let provider = providers
        .iter()
        .find(|provider| provider["providerId"].as_str() == Some(provider_id))
        .ok_or("provider-not-found")?;
    let model = provider["models"]
        .as_array()
        .and_then(|models| {
            models
                .iter()
                .find(|model| model["modelId"].as_str() == Some(model_id))
        })
        .ok_or("model-not-found")?;
    let has_reasoning = selection
        .get("options")
        .and_then(|options| options.get("reasoningLevel"))
        .and_then(Value::as_str)
        .is_some();
    if !has_reasoning {
        return Err("reasoning-level-missing");
    }
    let completed = selection.clone();
    if let Some(spec) = model
        .get("config")
        .and_then(|config| config.get("optionSpecs"))
        .and_then(|specs| specs.get("reasoningLevel"))
        .and_then(|spec| spec.get("values"))
        .and_then(Value::as_array)
    {
        let level = completed
            .get("options")
            .and_then(|options| options.get("reasoningLevel"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !spec.iter().any(|value| value.as_str() == Some(level)) {
            return Err("reasoning-level-not-supported");
        }
    }
    Ok(completed)
}

/// Default inference strength for model optionSpecs: `default` takes precedence, otherwise the first optional value.
/// UI submission channel requires `options.reasoningLevel` required (registry.validateModelSelectionOptions),
/// The default preferredSelection will prevent the first message from being sent.
fn default_reasoning_level(model: &Value) -> Option<String> {
    let spec = model
        .get("config")?
        .get("optionSpecs")?
        .get("reasoningLevel")?;
    if let Some(default) = spec.get("default").and_then(Value::as_str) {
        if !default.is_empty() {
            return Some(default.to_string());
        }
    }
    spec.get("values")?
        .as_array()?
        .first()?
        .as_str()
        .map(str::to_string)
}

/// Complete selection's default reasoningLevel (retaining the legal value given by the caller).
fn complete_selection(selection: &Value, providers: &[Value]) -> Value {
    let mut completed = selection.clone();
    let provider_id = selection.get("providerId").and_then(Value::as_str);
    let model_id = selection.get("modelId").and_then(Value::as_str);
    let model = providers.iter().find_map(|provider| {
        if provider["providerId"].as_str() != provider_id {
            return None;
        }
        provider["models"]
            .as_array()?
            .iter()
            .find(|model| model["modelId"].as_str() == model_id)
    });
    let has_reasoning = completed
        .get("options")
        .and_then(|options| options.get("reasoningLevel"))
        .and_then(Value::as_str)
        .is_some();
    if !has_reasoning {
        if let Some(level) = model.and_then(default_reasoning_level) {
            completed["options"] = json!({"reasoningLevel": level});
        }
    }
    completed
}

fn selection_is_valid(selection: &Value, providers: &[Value]) -> bool {
    let Some(provider_id) = selection.get("providerId").and_then(Value::as_str) else {
        return false;
    };
    let Some(model_id) = selection.get("modelId").and_then(Value::as_str) else {
        return false;
    };
    providers.iter().any(|provider| {
        provider["providerId"].as_str() == Some(provider_id)
            && provider["models"].as_array().is_some_and(|models| {
                models
                    .iter()
                    .any(|model| model["modelId"].as_str() == Some(model_id))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_view() -> Vec<Value> {
        vec![json!({
            "providerId": "personal:1",
            "providerName": "P",
            "config": {},
            "models": [{
                "modelId": "m1",
                "config": {
                    "optionSpecs": {"reasoningLevel": {"values": ["low", "high"]}}
                }
            }]
        })]
    }

    #[test]
    fn preferred_selection_gets_default_reasoning_level() {
        let providers = provider_view();
        let selection = json!({"providerId": "personal:1", "modelId": "m1"});
        assert!(selection_is_valid(&selection, &providers));
        let completed = complete_selection(&selection, &providers);
        assert_eq!(completed["options"]["reasoningLevel"], json!("low"));
    }

    #[test]
    fn explicit_reasoning_level_is_preserved() {
        let providers = provider_view();
        let selection = json!({
            "providerId": "personal:1",
            "modelId": "m1",
            "options": {"reasoningLevel": "high"}
        });
        let completed = complete_selection(&selection, &providers);
        assert_eq!(completed["options"]["reasoningLevel"], json!("high"));
    }

    #[test]
    fn default_field_wins_over_first_value() {
        let model = json!({
            "config": {"optionSpecs": {"reasoningLevel": {"values": ["low", "high"], "default": "high"}}}
        });
        assert_eq!(default_reasoning_level(&model), Some("high".to_string()));
    }
}
