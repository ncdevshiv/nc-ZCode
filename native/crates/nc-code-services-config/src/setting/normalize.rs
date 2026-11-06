//! Settings patch normalization (`normalizeSettingsPatch.ts`).
//!
//! RPC drops `undefined`, so the UI expresses "clear this override" as an empty
//! string; these rules turn that into key removal before the patch is merged.

use serde_json::{Map, Value};

/// A normalized patch plus the keys the user explicitly cleared. A cleared key must
/// remove the existing value during the merge (RPC cannot transport `undefined`).
pub struct NormalizedPatch {
    pub values: Map<String, Value>,
    pub cleared: Vec<String>,
}

pub fn normalize_patch(patch: &Map<String, Value>) -> NormalizedPatch {
    let mut normalized = patch.clone();
    let mut cleared = Vec::new();

    if normalized.contains_key("locale") && !normalized.contains_key("localePreference") {
        if let Some(locale) = normalized.get("locale").and_then(Value::as_str) {
            normalized.insert(
                "localePreference".to_string(),
                Value::String(locale.to_string()),
            );
        }
    }

    for key in [
        "terminalFontFamily",
        "httpProxy",
        "httpProxyNoProxy",
        "httpProxyCaCertPath",
        "zcodeEndpointOrigin",
        "providerFamilyDomain",
    ] {
        clear_when_blank(&mut normalized, key, &mut cleared);
    }

    if let Some(selection) = normalized
        .get_mut("integratedTerminalShell")
        .and_then(Value::as_object_mut)
    {
        let mode = selection.get("mode").and_then(Value::as_str);
        if mode == Some("auto") {
            normalized.remove("integratedTerminalShell");
            cleared.push("integratedTerminalShell".to_string());
        } else if mode == Some("shell") {
            for key in ["id", "label", "path"] {
                if let Some(Value::String(text)) = selection.get_mut(key) {
                    *text = text.trim().to_string();
                }
            }
        }
    }

    NormalizedPatch {
        values: normalized,
        cleared,
    }
}

fn clear_when_blank(patch: &mut Map<String, Value>, key: &str, cleared: &mut Vec<String>) {
    let Some(Value::String(text)) = patch.get_mut(key) else {
        return;
    };
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        patch.remove(key);
        cleared.push(key.to_string());
    } else {
        *text = trimmed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn normalize(value: Value) -> Value {
        Value::Object(normalize_patch(value.as_object().expect("object")).values)
    }

    #[test]
    fn locale_without_preference_becomes_an_explicit_preference() {
        assert_eq!(
            normalize(json!({"locale": "en-US"})),
            json!({"locale": "en-US", "localePreference": "en-US"})
        );
        assert_eq!(
            normalize(json!({"locale": "en-US", "localePreference": "system"})),
            json!({"locale": "en-US", "localePreference": "system"})
        );
    }

    #[test]
    fn blank_overrides_are_cleared() {
        let normalized = normalize(json!({
            "terminalFontFamily": "  ",
            "httpProxy": "",
            "httpProxyNoProxy": "  ",
            "httpProxyCaCertPath": "",
            "zcodeEndpointOrigin": "",
            "providerFamilyDomain": " ",
        }));
        assert_eq!(normalized, json!({}));
    }

    #[test]
    fn terminal_shell_auto_is_removed_and_shell_fields_are_trimmed() {
        assert_eq!(
            normalize(json!({"integratedTerminalShell": {"mode": "auto"}})),
            json!({})
        );
        assert_eq!(
            normalize(json!({
                "integratedTerminalShell": {
                    "mode": "shell",
                    "id": " git-bash ",
                    "label": " Git Bash ",
                    "path": " C:/git/bash.exe "
                }
            })),
            json!({
                "integratedTerminalShell": {
                    "mode": "shell",
                    "id": "git-bash",
                    "label": "Git Bash",
                    "path": "C:/git/bash.exe"
                }
            })
        );
    }

    #[test]
    fn non_blank_values_are_trimmed_not_removed() {
        assert_eq!(
            normalize(json!({"httpProxy": " http://p:1 ", "terminalFontFamily": " Fira "})),
            json!({"httpProxy": "http://p:1", "terminalFontFamily": "Fira"})
        );
    }
}
