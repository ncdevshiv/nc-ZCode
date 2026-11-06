//! **Structural bounding** of v4 run-event payloads (port of `boundDynamicWorkflowRunEventPayload` /
//! `toProtocolEvent` from `contracts/src/interfaces/dynamic-workflow-run.port.ts` +
//! `bootstrap/.../dynamic-workflow-run-launch.ts`).
//!
//! The bounds are structural (string length / array item count / key count / depth), not a total byte
//! budget: a structural bound applies per field in place, so one enormous value cannot take the other
//! fields down with it. `actor-created.persona.system` and the node-level error's `finalText` are the
//! two naturally unbounded fields — they are not waved through on an assumption that they are
//! probably short.

use serde_json::{json, Map, Value};

use crate::workflow_service::service::EventPage;

/// Value-for-value identical to TS `DYNAMIC_WORKFLOW_RUN_EVENT_PAYLOAD_LIMITS`.
pub const MAX_STRING_LENGTH: usize = 2_048;
pub const MAX_ARRAY_ITEMS: usize = 32;
pub const MAX_KEYS: usize = 32;
pub const MAX_DEPTH: usize = 6;

/// Clamp one payload inside the bounds. Returns (payload, wasTruncated).
///
/// Normalization is not incidental: a non-finite number is folded to null by `JSON.stringify` on the
/// TS side, and it is folded the same way here (`serde_json::Value` cannot hold NaN/Infinity at all,
/// so readers agree by construction).
pub fn bound_event_payload(payload: &Value) -> (Value, bool) {
    let mut truncated = false;
    let bounded = bound_value(payload, 0, &mut truncated);
    (bounded.unwrap_or(Value::Null), truncated)
}

fn bound_value(value: &Value, depth: usize, truncated: &mut bool) -> Option<Value> {
    match value {
        Value::Null | Value::Bool(_) => Some(value.clone()),
        Value::Number(number) => Some(Value::Number(number.clone())),
        Value::String(text) => {
            if text.encode_utf16().count() <= MAX_STRING_LENGTH {
                Some(value.clone())
            } else {
                *truncated = true;
                Some(Value::String(truncate_surrogate_safe(
                    text,
                    MAX_STRING_LENGTH,
                )))
            }
        }
        Value::Array(items) => {
            if depth >= MAX_DEPTH {
                *truncated = true;
                return None;
            }
            let kept = items.len().min(MAX_ARRAY_ITEMS);
            if kept < items.len() {
                *truncated = true;
            }
            let mut out = Vec::with_capacity(kept);
            for item in items.iter().take(kept) {
                // A hole in an array would change index semantics, so an unloadable element becomes
                // null rather than being skipped.
                out.push(bound_value(item, depth + 1, truncated).unwrap_or(Value::Null));
            }
            Some(Value::Array(out))
        }
        Value::Object(map) => {
            if depth >= MAX_DEPTH {
                *truncated = true;
                return None;
            }
            if map.len() > MAX_KEYS {
                *truncated = true;
            }
            let mut out = Map::new();
            for (key, item) in map.iter().take(MAX_KEYS) {
                // An omitted object member simply disappears (the same result JSON serialization
                // produces when TS returns undefined).
                if let Some(bounded) = bound_value(item, depth + 1, truncated) {
                    out.insert(key.clone(), bounded);
                }
            }
            Some(Value::Object(out))
        }
    }
}

/// Truncate by UTF-16 code units and never leave a lone surrogate (one code unit is dropped rather
/// than split apart).
fn truncate_surrogate_safe(value: &str, max_units: usize) -> String {
    let mut units = 0usize;
    let mut out = String::new();
    for character in value.chars() {
        let width = character.len_utf16();
        if units + width > max_units {
            break;
        }
        units += width;
        out.push(character);
    }
    out
}

/// One stored event → the v4 protocol event `{sequence, type, payload, truncated?}`.
pub fn to_protocol_event(sequence: u64, event: &nc_code_workflow::types::RunEvent) -> Value {
    let mut serialized = serde_json::to_value(event).unwrap_or(Value::Null);
    let Some(map) = serialized.as_object_mut() else {
        // Serializing a RunEvent always yields an object; reaching here means the contract is broken,
        // so an empty payload carrying only the sequence is returned.
        return json!({"sequence": sequence, "payload": {}});
    };
    let type_name = map.remove("type").unwrap_or(Value::Null);
    let payload = Value::Object(std::mem::take(map));
    let (bounded, truncated) = bound_event_payload(&payload);
    let mut out = Map::new();
    out.insert("sequence".to_string(), Value::from(sequence));
    out.insert("type".to_string(), type_name);
    out.insert("payload".to_string(), bounded);
    if truncated {
        out.insert("truncated".to_string(), Value::Bool(true));
    }
    Value::Object(out)
}

/// An event page → the v4 result `{events, hasMore}` (`hasMore` comes from the service's limit+1 probe).
pub fn event_page_to_v4(page: &EventPage) -> Value {
    let events: Vec<Value> = page
        .events
        .iter()
        .map(|stored| to_protocol_event(stored.sequence, &stored.event))
        .collect();
    let mut out = Map::new();
    out.insert("events".to_string(), Value::Array(events));
    out.insert("hasMore".to_string(), Value::Bool(page.has_more));
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_workflow::types::RunEvent;
    use serde_json::json;

    #[test]
    fn long_strings_are_truncated_by_utf16_units_surrogate_safe() {
        let long = "a".repeat(MAX_STRING_LENGTH + 10);
        let (bounded, truncated) = bound_event_payload(&json!({"text": long}));
        assert!(truncated);
        let text = bounded["text"].as_str().unwrap();
        assert_eq!(text.encode_utf16().count(), MAX_STRING_LENGTH);

        // A cut landing inside a surrogate pair: the whole emoji (2 code units) is dropped, so no lone
        // surrogate is produced.
        let mut near = "a".repeat(MAX_STRING_LENGTH - 1);
        near.push('😀');
        let (bounded, truncated) = bound_event_payload(&json!({"text": near}));
        assert!(truncated);
        let text = bounded["text"].as_str().unwrap();
        assert_eq!(text.encode_utf16().count(), MAX_STRING_LENGTH - 1);
        assert!(!text.contains('\u{FFFD}'));
    }

    #[test]
    fn arrays_and_objects_are_capped_structurally() {
        let items: Vec<Value> = (0..40).map(|n| json!(n)).collect();
        let (bounded, truncated) = bound_event_payload(&json!({"items": items}));
        assert!(truncated);
        assert_eq!(bounded["items"].as_array().unwrap().len(), MAX_ARRAY_ITEMS);

        let mut map = Map::new();
        for n in 0..40 {
            map.insert(format!("k{n}"), json!(n));
        }
        let (bounded, truncated) = bound_event_payload(&Value::Object(map));
        assert!(truncated);
        assert_eq!(bounded.as_object().unwrap().len(), MAX_KEYS);
    }

    #[test]
    fn depth_is_capped() {
        let mut value = json!(1);
        for _ in 0..10 {
            value = json!({"n": value});
        }
        let (_bounded, truncated) = bound_event_payload(&value);
        assert!(truncated);
    }

    #[test]
    fn deep_object_members_are_omitted_not_nulled() {
        let mut value = json!({"leaf": 1});
        for _ in 0..(MAX_DEPTH - 1) {
            value = json!({"n": value});
        }
        // One more object level: the inner object is omitted at the depth bound.
        let value = json!({"n": value});
        let (bounded, truncated) = bound_event_payload(&value);
        assert!(truncated);
        // Omitted member: truncation happened, yet the whole tree stays a minimal JSON value (null is
        // not used to impersonate a member that was cut away).
        assert!(bounded.is_object());
    }

    #[test]
    fn protocol_event_splits_type_from_payload() {
        let event = RunEvent::Log {
            message: "hello".into(),
        };
        let value = to_protocol_event(7, &event);
        assert_eq!(value["sequence"], json!(7));
        assert_eq!(value["type"], json!("log"));
        assert_eq!(value["payload"]["message"], json!("hello"));
        assert!(value.get("truncated").is_none());
    }

    #[test]
    fn protocol_event_marks_truncation() {
        let event = RunEvent::Log {
            message: "x".repeat(MAX_STRING_LENGTH + 5),
        };
        let value = to_protocol_event(1, &event);
        assert_eq!(value["truncated"], json!(true));
        assert_eq!(
            value["payload"]["message"]
                .as_str()
                .unwrap()
                .encode_utf16()
                .count(),
            MAX_STRING_LENGTH
        );
    }
}
