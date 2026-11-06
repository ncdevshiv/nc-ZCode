//! Schema-driven settings normalization.
//!
//! The JSON Schema and defaults are generated from the REAL Zod schema by
//! `native/tools/generate-shared-contracts.mjs`; this module never retypes fields.
//! It reproduces the two behaviors the TS service relies on: unknown keys are
//! stripped (Zod object default) and missing fields take their schema defaults.

use serde_json::{Map, Value};
use std::sync::OnceLock;

pub const SETTINGS_SCHEMA_JSON: &str =
    include_str!("../../../../fixtures/app-settings.schema.json");
pub const SETTINGS_PATCH_SCHEMA_JSON: &str =
    include_str!("../../../../fixtures/app-settings-patch.schema.json");
pub const SETTINGS_DEFAULTS_JSON: &str =
    include_str!("../../../../fixtures/app-settings-defaults.json");

pub struct SettingsSchema {
    full: Value,
    patch: Value,
    defaults: Value,
}

pub fn schema() -> &'static SettingsSchema {
    static SCHEMA: OnceLock<SettingsSchema> = OnceLock::new();
    SCHEMA.get_or_init(|| SettingsSchema {
        full: serde_json::from_str(SETTINGS_SCHEMA_JSON).expect("app settings schema fixture"),
        patch: serde_json::from_str(SETTINGS_PATCH_SCHEMA_JSON)
            .expect("app settings patch schema fixture"),
        defaults: serde_json::from_str(SETTINGS_DEFAULTS_JSON)
            .expect("app settings defaults fixture"),
    })
}

impl SettingsSchema {
    pub fn defaults(&self) -> Value {
        self.defaults.clone()
    }

    pub fn validate_full(&self, value: &Value) -> Result<(), String> {
        validate(value, &self.full, "$")
    }

    pub fn validate_patch(&self, value: &Value) -> Result<(), String> {
        validate(value, &self.patch, "$")
    }

    pub fn normalize_full(&self, value: &Value) -> Value {
        normalize(value, &self.full, Some(&self.defaults))
    }

    pub fn normalize_patch(&self, value: &Value) -> Value {
        normalize(value, &self.patch, None)
    }
}

pub fn validate(value: &Value, schema: &Value, path: &str) -> Result<(), String> {
    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        if branches
            .iter()
            .any(|branch| validate(value, branch, path).is_ok())
        {
            return Ok(());
        }
        return Err(format!("{path}: value matches none of the allowed shapes"));
    }
    if let Some(branches) = schema.get("anyOf").and_then(Value::as_array) {
        if branches
            .iter()
            .any(|branch| validate(value, branch, path).is_ok())
        {
            return Ok(());
        }
        return Err(format!("{path}: value matches none of the allowed shapes"));
    }
    if let Some(branches) = schema.get("allOf").and_then(Value::as_array) {
        for branch in branches {
            validate(value, branch, path)?;
        }
    }
    if let Some(expected) = schema.get("const") {
        if value != expected {
            return Err(format!("{path}: expected {expected}"));
        }
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        if !values.contains(value) {
            return Err(format!("{path}: value is not one of the allowed literals"));
        }
    }
    if let Some(expected_type) = schema.get("type").and_then(Value::as_str) {
        if !type_matches(value, expected_type) {
            return Err(format!("{path}: expected {expected_type}"));
        }
    }
    match value {
        Value::Object(map) => {
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for key in required.iter().filter_map(Value::as_str) {
                    if !map.contains_key(key) {
                        return Err(format!("{path}.{key}: required"));
                    }
                }
            }
            let properties = schema.get("properties").and_then(Value::as_object);
            let additional = schema.get("additionalProperties");
            for (key, item) in map {
                if let Some(property_schema) = properties.and_then(|properties| properties.get(key))
                {
                    validate(item, property_schema, &format!("{path}.{key}"))?;
                    continue;
                }
                if let Some(property_names) = schema.get("propertyNames") {
                    let key_value = Value::String(key.clone());
                    validate(&key_value, property_names, &format!("{path}.<key>"))?;
                }
                match additional {
                    Some(Value::Bool(false)) => {
                        return Err(format!("{path}.{key}: unknown key is not allowed"));
                    }
                    Some(additional_schema) if additional_schema.is_object() => {
                        validate(item, additional_schema, &format!("{path}.{key}"))?;
                    }
                    _ => {}
                }
            }
            Ok(())
        }
        Value::Array(items) => {
            if let Some(min_items) = schema.get("minItems").and_then(Value::as_u64) {
                if (items.len() as u64) < min_items {
                    return Err(format!("{path}: needs at least {min_items} items"));
                }
            }
            if let Some(item_schema) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    validate(item, item_schema, &format!("{path}[{index}]"))?;
                }
            }
            Ok(())
        }
        Value::String(text) => {
            if let Some(min_length) = schema.get("minLength").and_then(Value::as_u64) {
                if (text.chars().count() as u64) < min_length {
                    return Err(format!("{path}: shorter than {min_length} characters"));
                }
            }
            Ok(())
        }
        Value::Number(number) => {
            if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64) {
                if number.as_f64().is_some_and(|value| value < minimum) {
                    return Err(format!("{path}: below minimum {minimum}"));
                }
            }
            if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64) {
                if number.as_f64().is_some_and(|value| value > maximum) {
                    return Err(format!("{path}: above maximum {maximum}"));
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn type_matches(value: &Value, expected: &str) -> bool {
    match expected {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn normalize(value: &Value, schema: &Value, defaults_source: Option<&Value>) -> Value {
    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        for branch in branches {
            if validate(value, branch, "$").is_ok() {
                return normalize(value, branch, None);
            }
        }
        return value.clone();
    }
    if let Some(branches) = schema.get("anyOf").and_then(Value::as_array) {
        for branch in branches {
            if validate(value, branch, "$").is_ok() {
                return normalize(value, branch, None);
            }
        }
        return value.clone();
    }
    if let Some(branches) = schema.get("allOf").and_then(Value::as_array) {
        let mut current = value.clone();
        for branch in branches {
            current = normalize(&current, branch, None);
        }
        return current;
    }
    match value {
        Value::Object(map) => {
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                let mut out = Map::new();
                for (key, property_schema) in properties {
                    if let Some(item) = map.get(key) {
                        out.insert(key.clone(), normalize(item, property_schema, None));
                        continue;
                    }
                    if let Some(default) = property_schema.get("default") {
                        out.insert(key.clone(), normalize(default, property_schema, None));
                        continue;
                    }
                    if let Some(default) = defaults_source
                        .and_then(Value::as_object)
                        .and_then(|defaults| defaults.get(key))
                    {
                        out.insert(key.clone(), normalize(default, property_schema, None));
                    }
                }
                return Value::Object(out);
            }
            if let Some(additional_schema) = schema
                .get("additionalProperties")
                .filter(|value| value.is_object())
            {
                let mut out = Map::new();
                for (key, item) in map {
                    out.insert(key.clone(), normalize(item, additional_schema, None));
                }
                return Value::Object(out);
            }
            value.clone()
        }
        Value::Array(items) => {
            let Some(item_schema) = schema.get("items") else {
                return value.clone();
            };
            Value::Array(
                items
                    .iter()
                    .map(|item| normalize(item, item_schema, None))
                    .collect(),
            )
        }
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_validate_and_round_trip() {
        let schema = schema();
        let defaults = schema.defaults();
        schema
            .validate_full(&defaults)
            .expect("defaults must validate");
        assert_eq!(schema.normalize_full(&defaults), defaults);
        assert_eq!(defaults["locale"], json!("zh-CN"));
        assert_eq!(defaults["localePreference"], json!("system"));
    }

    #[test]
    fn unknown_keys_are_stripped_and_missing_defaults_are_filled() {
        let schema = schema();
        let raw = json!({"locale": "en-US", "unknownKey": 1});
        schema.validate_full(&raw).expect("valid");
        let normalized = schema.normalize_full(&raw);
        assert_eq!(normalized["locale"], json!("en-US"));
        assert!(normalized.get("unknownKey").is_none());
        assert_eq!(normalized["localePreference"], json!("system"));
        assert_eq!(normalized["recentProjects"], json!([]));
    }

    #[test]
    fn type_mismatches_are_reported() {
        let schema = schema();
        let raw = json!({"locale": 1});
        let error = schema.validate_full(&raw).expect_err("must reject");
        assert!(error.contains("locale"), "{error}");
        let patched = json!({"embeddedBrowserViewportPreference": {"mode": "nope"}});
        assert!(schema.validate_patch(&patched).is_err());
    }

    #[test]
    fn patch_normalization_keeps_only_known_keys_and_nested_defaults() {
        let schema = schema();
        let patch = json!({
            "locale": "en-US",
            "unknown": true,
            "embeddedBrowserViewportPreference": {"mode": "responsive"},
        });
        let normalized = schema.normalize_patch(&patch);
        assert!(normalized.get("unknown").is_none());
        assert_eq!(normalized["locale"], json!("en-US"));
        assert_eq!(
            normalized["embeddedBrowserViewportPreference"]["mode"],
            json!("responsive")
        );
    }

    #[test]
    fn union_settings_keep_valid_shapes() {
        let schema = schema();
        let raw = json!({
            "lastWorkspaceSession": [
                {"kind": "local", "workspacePath": "G:/work"},
                {
                    "kind": "remote",
                    "workspacePath": "G:/remote",
                    "workspaceIdentity": "id",
                    "target": {"kind": "wsl"},
                    "lastOpenedAt": 1,
                    "lastConnectionStatus": "connected"
                }
            ]
        });
        schema.validate_full(&raw).expect("union shapes valid");
        let normalized = schema.normalize_full(&raw);
        assert_eq!(
            normalized["lastWorkspaceSession"][0]["workspacePurpose"],
            json!("project")
        );
    }
}
