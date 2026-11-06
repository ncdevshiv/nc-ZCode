//! Legacy account-connection migration (`legacyAccountConnectionSettings.ts`).
//!
//! Old builds stored provider-family navigation keys (`modelProviderFamilyModes`,
//! `modelProviderFamilySelectedKeys`). They are migrated once into
//! `providerFamilyConnectionSelections` and retained in the file for rollback.

use percent_encoding::percent_decode_str;
use serde_json::{Map, Value};

pub struct LegacyTeamConnection {
    pub family: String,
    pub product_id: String,
    pub project_id: String,
}

pub fn needs_legacy_migration(value: &Value) -> bool {
    let Some(raw) = value.as_object() else {
        return false;
    };
    !raw.contains_key("providerFamilyConnectionSelections")
        && (raw.contains_key("modelProviderFamilySelectedKeys")
            || raw.contains_key("modelProviderFamilyModes"))
}

pub fn incomplete_legacy_team_connections(value: &Value) -> Vec<LegacyTeamConnection> {
    if !needs_legacy_migration(value) {
        return Vec::new();
    }
    let Some(raw) = value.as_object() else {
        return Vec::new();
    };
    let modes = raw
        .get("modelProviderFamilyModes")
        .and_then(Value::as_object);
    let keys = raw
        .get("modelProviderFamilySelectedKeys")
        .and_then(Value::as_object);
    let mut result = Vec::new();
    for family in ["zai", "bigmodel"] {
        let api_key_mode = modes
            .and_then(|modes| modes.get(family))
            .and_then(Value::as_str)
            == Some("apiKey");
        if api_key_mode {
            continue;
        }
        let prefix = format!("team-plan:builtin:{family}-coding-plan:");
        let key = keys
            .and_then(|keys| keys.get(family))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let Some(rest) = key.strip_prefix(&prefix) else {
            continue;
        };
        let parts = split_legacy_parts(rest);
        if parts.len() != 2 || parts.iter().any(String::is_empty) {
            continue;
        }
        result.push(LegacyTeamConnection {
            family: family.to_string(),
            product_id: parts[0].clone(),
            project_id: parts[1].clone(),
        });
    }
    result
}

pub fn migrate_legacy(value: &Value) -> Value {
    if !needs_legacy_migration(value) {
        return value.clone();
    }
    let Some(raw) = value.as_object() else {
        return value.clone();
    };
    let modes = raw
        .get("modelProviderFamilyModes")
        .and_then(Value::as_object);
    let keys = raw
        .get("modelProviderFamilySelectedKeys")
        .and_then(Value::as_object);
    let mut selections = Map::new();
    for family in ["zai", "bigmodel"] {
        let api_key_mode = modes
            .and_then(|modes| modes.get(family))
            .and_then(Value::as_str)
            == Some("apiKey");
        if api_key_mode {
            continue;
        }
        let key = keys
            .and_then(|keys| keys.get(family))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let selection = if key == format!("coding-plan:builtin:{family}-start-plan") {
            Some(serde_json::json!({"kind": "start-plan"}))
        } else if key == format!("coding-plan:builtin:{family}-coding-plan") {
            Some(serde_json::json!({"kind": "individual-coding-plan"}))
        } else {
            let prefix = format!("team-plan:builtin:{family}-coding-plan:");
            key.strip_prefix(&prefix).and_then(|rest| {
                let parts = split_legacy_parts(rest);
                // Project-only legacy keys cannot identify the organization; keep the
                // field out instead of inventing a Team identity.
                if parts.len() != 3 || parts.iter().any(String::is_empty) {
                    return None;
                }
                Some(serde_json::json!({
                    "kind": "team-coding-plan",
                    "productId": parts[0],
                    "organizationId": parts[1],
                    "projectId": parts[2],
                }))
            })
        };
        if let Some(selection) = selection {
            selections.insert(family.to_string(), selection);
        }
    }
    let mut migrated = raw.clone();
    migrated.insert(
        "providerFamilyConnectionSelections".to_string(),
        Value::Object(selections),
    );
    Value::Object(migrated)
}

pub fn retain_legacy_fields(value: &Value) -> Map<String, Value> {
    let mut retained = Map::new();
    let Some(raw) = value.as_object() else {
        return retained;
    };
    for key in [
        "modelProviderFamilyModes",
        "modelProviderFamilySelectedKeys",
    ] {
        if let Some(field) = raw.get(key) {
            retained.insert(key.to_string(), field.clone());
        }
    }
    retained
}

fn split_legacy_parts(value: &str) -> Vec<String> {
    value
        .split(':')
        .map(|part| {
            percent_decode_str(part)
                .decode_utf8_lossy()
                .trim()
                .to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn individual_and_team_keys_migrate() {
        let raw = json!({
            "modelProviderFamilySelectedKeys": {
                "zai": "coding-plan:builtin:zai-coding-plan",
                "bigmodel": "team-plan:builtin:bigmodel-coding-plan:product%201:org:proj"
            }
        });
        assert!(needs_legacy_migration(&raw));
        let migrated = migrate_legacy(&raw);
        assert_eq!(
            migrated["providerFamilyConnectionSelections"]["zai"]["kind"],
            json!("individual-coding-plan")
        );
        assert_eq!(
            migrated["providerFamilyConnectionSelections"]["bigmodel"],
            json!({
                "kind": "team-coding-plan",
                "productId": "product 1",
                "organizationId": "org",
                "projectId": "proj",
            })
        );
        let retained = retain_legacy_fields(&raw);
        assert!(retained.contains_key("modelProviderFamilySelectedKeys"));
    }

    #[test]
    fn api_key_mode_is_not_an_account_selection() {
        let raw = json!({
            "modelProviderFamilyModes": {"zai": "apiKey"},
            "modelProviderFamilySelectedKeys": {"zai": "coding-plan:builtin:zai-coding-plan"}
        });
        let migrated = migrate_legacy(&raw);
        assert!(migrated["providerFamilyConnectionSelections"]
            .as_object()
            .expect("object")
            .is_empty());
    }

    #[test]
    fn incomplete_team_keys_are_detected() {
        let raw = json!({
            "modelProviderFamilySelectedKeys": {
                "bigmodel": "team-plan:builtin:bigmodel-coding-plan:product:project"
            }
        });
        assert_eq!(incomplete_legacy_team_connections(&raw).len(), 1);
    }

    #[test]
    fn empty_selection_kind_migrates() {
        let raw = json!({
            "modelProviderFamilyModes": {"zai": "codingPlan"},
            "modelProviderFamilySelectedKeys": {"zai": "coding-plan:builtin:zai-start-plan"}
        });
        let migrated = migrate_legacy(&raw);
        assert_eq!(
            migrated["providerFamilyConnectionSelections"]["zai"],
            json!({"kind": "start-plan"})
        );
    }
}
