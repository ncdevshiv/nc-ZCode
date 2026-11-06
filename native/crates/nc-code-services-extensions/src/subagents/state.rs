//! agents-state.json read/write —— the Rust counterpart of readAgentStateFile/
//! writeAgentStateFile in TS `subagents/subagentsService.ts`.
//!
//! The state file carries three things: model-selection overrides for built-in agents, model-selection overrides for plugin agents, and the list of disabled agent ids.
//! On read the structure is normalized (invalid items are dropped rather than invalidating the whole file); on write an atomic write is used.

use std::path::Path;

use serde_json::{json, Map, Value};

use nc_code_service_core::atomic::atomic_write_text;

/// emptyAgentsState: the empty state of the three fields.
pub fn empty_agents_state() -> Value {
    json!({
        "builtInModelSelectionOverrides": {},
        "pluginAgentModelSelectionOverrides": {},
        "disabledAgentIds": [],
    })
}

fn as_record(value: Option<&Value>) -> Map<String, Value> {
    value
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// readAgentStateFile: read plus normalize (an invalid structure falls back to the empty state, so one bad field does not ruin the whole state).
pub fn read_agents_state(path: &Path) -> Value {
    let Ok(text) = std::fs::read_to_string(path) else {
        return empty_agents_state();
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
        return empty_agents_state();
    };
    if !parsed.is_object() {
        return empty_agents_state();
    }
    let built_in = as_record(parsed.get("builtInModelSelectionOverrides"));
    let plugin = as_record(parsed.get("pluginAgentModelSelectionOverrides"));
    let disabled: Vec<Value> = parsed
        .get("disabledAgentIds")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(|id| Value::String(id.trim().to_string()))
                .filter(|id| id.as_str().is_some_and(|s| !s.is_empty()))
                .collect()
        })
        .unwrap_or_default();
    json!({
        "builtInModelSelectionOverrides": Value::Object(built_in),
        "pluginAgentModelSelectionOverrides": Value::Object(plugin),
        "disabledAgentIds": Value::Array(disabled),
    })
}

/// writeAgentStateFile: atomic write (same as TS atomicWriteText).
pub async fn write_agents_state(path: &Path, state: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(state).map_err(|error| error.to_string())?;
    atomic_write_text(path, &text)
        .await
        .map_err(|error| format!("write {}: {error}", path.display()))
}

/// Whether disabledAgentIds contains the given id.
pub fn is_disabled(state: &Value, agent_id: &str) -> bool {
    state
        .get("disabledAgentIds")
        .and_then(Value::as_array)
        .map(|ids| ids.iter().any(|id| id.as_str() == Some(agent_id)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_normalizes_state() {
        let dir = std::env::temp_dir().join(format!("zcode-agents-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("agents-state.json");
        std::fs::write(
            &path,
            r#"{"disabledAgentIds":["a","", "  ","b"],"builtInModelSelectionOverrides":42}"#,
        )
        .expect("write");
        let state = read_agents_state(&path);
        // An invalid builtIn override (a number) normalizes to an empty object; empty ids are filtered out.
        assert_eq!(state["builtInModelSelectionOverrides"], json!({}));
        assert_eq!(state["disabledAgentIds"], json!(["a", "b"]));
        assert!(is_disabled(&state, "a"));
        assert!(!is_disabled(&state, "c"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_or_corrupt_file_is_empty_state() {
        let dir =
            std::env::temp_dir().join(format!("zcode-agents-state-miss-{}", std::process::id()));
        let path = dir.join("agents-state.json");
        assert_eq!(read_agents_state(&path), empty_agents_state());
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(&path, "{ not json").expect("write");
        assert_eq!(read_agents_state(&path), empty_agents_state());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
