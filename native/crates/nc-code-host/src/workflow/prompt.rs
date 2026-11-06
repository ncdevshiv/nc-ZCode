//! The fixed text of ask prompts and the submit rejection copy (port of `workflow-driver-helpers.ts` +
//! `workflow-ask-epilogue.ts`).
//!
//! These strings **are the contract**: the model's self-repair depends on the shape of the rejection copy, the GUI folds the epilogue
//! on `schemaEpilogue`'s divider, and the escalation budget copy decides whether the model hits the wall again next time. Copied verbatim from TS, pinned by tests.

use nc_code_workflow::types::{
    AskLastTool, Violation, LAST_TOOL_NAME_MAX_CHARS, LAST_TOOL_TARGET_MAX_CHARS,
};
use serde_json::Value;

/// The cap on escalations within one ask (same family as the nudge budget).
pub const MAX_ESCALATIONS_PER_ASK: u32 = 3;

/// The tool-result copy returned to the model when the budget is exhausted (**an ordinary result, not an error**).
pub const ESCALATION_BUDGET_EXHAUSTED: &str =
    "Escalation budget exhausted: at most 3 escalations per task. \
Do not call escalate again. Proceed on your best judgement with the information you have, \
and state in your final result the assumptions you relied on and the doubts that remain.";

/// The rejection copy for escalate when no ask is in flight (the same argument as submit's identically named guard: do not park, do not hang).
pub const ESCALATION_NO_ACTIVE_ASK: &str =
    "No ask is in flight, so there is nowhere to park this question and nobody \
     would answer it. Escalate only while working on an ask.";

/// A synthetic rejection when submit arrives with no routable ask (already accepted / cancelled).
pub const SUBMIT_NO_ACTIVE_ASK: &str = "no active ask is awaiting a submitted result";

/// The synthetic rejection when an untyped ask receives submit: make the model switch to plain text, never report it to the engine.
pub const SUBMIT_UNTYPED_ASK: &str =
    "this ask does not accept submit_result; provide your answer as your final message";

/// The nudge prompt that urges the model to submit when a turn ends without submitting.
pub const NUDGE_PROMPT: &str = "You ended your turn without submitting a result. \
Call the submit_result tool now with a payload conforming to the required schema.";

/// The user-visible copy when an escalation / submission is cancelled by the ask (TS `cancellation.userVisibleMessage`).
pub const ESCALATE_CANCELLED: &str = "escalate was cancelled before the main agent answered";
pub const SUBMIT_CANCELLED: &str =
    "submit_result was cancelled before the engine returned a verdict";

/// The first line when submit is rejected by the engine (character-for-character identical to the core handler's `formatSubmitViolations`).
const SUBMIT_REJECTION_HEADER: &str = "The submitted result does not match the required schema:";

/// The quality epilogue for every ask (added to both typed and untyped). When the schema's top level has `evidence` / `confidence`,
/// those two fields are called out by name.
pub fn quality_epilogue(schema: Option<&Value>) -> String {
    let fields = top_level_schema_properties(schema);
    let mut lines = vec![
        "",
        "",
        "---",
        "Standard for this result:",
        "- Every finding cites what you read or ran: path and line for code; the exact command and its output for a check; the part of the ask for material the ask itself gave you.",
        "- A check counts as passed only if you ran it during this ask. Otherwise report it as not run.",
        "- Run the check the ask names, at the scale it names. A narrower or faster substitute — one test file for the suite, a build for the tests — is reported as what it is, never as the ask's check; say the exact command you ran.",
        "- Anything you could not do, verify, or find is stated as such — never filled with a plausible guess.",
    ];
    if fields.iter().any(|field| field == "evidence") {
        lines.push("- Put each finding's citation in its `evidence` field.");
    }
    if fields.iter().any(|field| field == "confidence") {
        lines.push(
            "- Rate `confidence` honestly; a low value with a reason beats a confident guess.",
        );
    }
    lines.push("- If you are blocked by something outside your reach, call `escalate` instead of inventing a value.");
    lines.join("\n")
}

/// The schema epilogue of a typed ask (the generic submit profile): divider + one sentence + the JSON Schema indented by 2
/// spaces + the result constraints.
pub fn schema_epilogue(schema: Option<&Value>) -> String {
    let rendered = match schema {
        None => "(any JSON value)".to_string(),
        // TS `JSON.stringify(schema, null, 2)`; serde_json's pretty format is also 2 spaces.
        Some(schema) => serde_json::to_string_pretty(schema).unwrap_or_else(|_| "null".to_string()),
    };
    [
        "",
        "",
        "---",
        "When you have finished, call the `submit_result` tool to submit your final result. Its `result` argument must be a JSON value conforming to this JSON Schema:",
        "",
        rendered.as_str(),
        "",
        "Pass the conforming JSON as the `result` argument — do not wrap it or add commentary.",
    ]
    .join("\n")
}

/// The complete prompt of one ask: instruction body + quality epilogue + (when typed) the schema epilogue.
pub fn ask_prompt(instructions: &str, typed: bool, schema: Option<&Value>) -> String {
    let mut prompt = String::with_capacity(instructions.len() + 2_048);
    prompt.push_str(instructions);
    prompt.push_str(&quality_epilogue(schema));
    if typed {
        prompt.push_str(&schema_epilogue(schema));
    }
    prompt
}

/// An engine Violation -> a rejection body the model can repair item by item (the same shape as TS's `formatSubmitViolations`).
pub fn format_submit_violations(violations: &[Violation]) -> String {
    if violations.is_empty() {
        return format!("{SUBMIT_REJECTION_HEADER}\n(no details provided)");
    }
    let lines: Vec<String> = violations
        .iter()
        .map(|violation| {
            format!(
                "{}: expected {}, got {}",
                violation.path, violation.expected, violation.got
            )
        })
        .collect();
    format!("{SUBMIT_REJECTION_HEADER}\n{}", lines.join("\n"))
}

/// The key set of the schema's top-level `properties`; empty for a non-object schema or when absent.
fn top_level_schema_properties(schema: Option<&Value>) -> Vec<String> {
    schema
        .and_then(|schema| schema.get("properties"))
        .and_then(Value::as_object)
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default()
}

/// Compress one tool call into `node-progress`'s `lastTool` (port of `workflow-driver-tool-target.ts`).
///
/// The target hint takes only one bounded scalar key, and everything else counts as "unrecognizable" -- the arguments
/// may hold an entire patch or body text, while this event is read into the main agent's context.
pub fn summarize_tool_call(name: &str, input: &Value) -> Option<AskLastTool> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    Some(AskLastTool {
        name: name.chars().take(LAST_TOOL_NAME_MAX_CHARS).collect(),
        target: derive_tool_target(input),
    })
}

/// Probe argument keys by priority; the path family keeps the tail (the file name is the discriminator), the rest keep the head.
const TARGET_KEYS: &[(&str, bool)] = &[
    ("file_path", false),
    ("notebook_path", false),
    ("path", false),
    ("command", true),
    ("pattern", true),
    ("url", true),
    ("query", true),
];

fn derive_tool_target(input: &Value) -> Option<String> {
    let Value::Object(record) = input else {
        return None;
    };
    for (key, keep_head) in TARGET_KEYS {
        let Some(value) = record.get(*key).and_then(Value::as_str) else {
            continue;
        };
        // A multi-line command takes only the first line with runs of whitespace collapsed: the command head is what "is running".
        let flattened = value
            .split('\n')
            .next()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if flattened.is_empty() {
            continue;
        }
        return Some(bound_target(&flattened, *keep_head));
    }
    None
}

fn bound_target(text: &str, keep_head: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= LAST_TOOL_TARGET_MAX_CHARS {
        return text.to_string();
    }
    if keep_head {
        return chars[..LAST_TOOL_TARGET_MAX_CHARS].iter().collect();
    }
    let keep = LAST_TOOL_TARGET_MAX_CHARS.saturating_sub(1);
    format!(
        "…{}",
        chars[chars.len() - keep..].iter().collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_workflow::types::Violation;
    use serde_json::json;

    #[test]
    fn quality_epilogue_mentions_schema_fields_only_when_present() {
        let plain = quality_epilogue(None);
        assert!(plain.starts_with("\n\n---\nStandard for this result:"));
        assert!(!plain.contains("`evidence` field"));
        assert!(!plain.contains("`confidence` honestly"));
        assert!(plain.ends_with("call `escalate` instead of inventing a value."));

        let named = quality_epilogue(Some(&json!({
            "type": "object",
            "properties": {"evidence": {"type": "string"}, "confidence": {"type": "number"}}
        })));
        assert!(named.contains("- Put each finding's citation in its `evidence` field."));
        assert!(named.contains("- Rate `confidence` honestly"));
    }

    #[test]
    fn schema_epilogue_renders_the_schema_verbatim() {
        let schema = json!({"type": "object", "required": ["answer"]});
        let rendered = schema_epilogue(Some(&schema));
        assert!(rendered.contains(
            "When you have finished, call the `submit_result` tool to submit your final result."
        ));
        assert!(
            rendered.contains("  \"type\": \"object\",\n  \"required\": [\n    \"answer\"\n  ]")
        );
        assert!(rendered.ends_with("do not wrap it or add commentary."));
        assert!(schema_epilogue(None).contains("(any JSON value)"));
    }

    #[test]
    fn rejection_text_matches_the_ts_handler() {
        let text = format_submit_violations(&[Violation {
            path: "$.items[0]".to_string(),
            expected: "string".to_string(),
            got: "number".to_string(),
        }]);
        assert_eq!(
            text,
            "The submitted result does not match the required schema:\n\
             $.items[0]: expected string, got number"
        );
        assert_eq!(
            format_submit_violations(&[]),
            "The submitted result does not match the required schema:\n(no details provided)"
        );
    }

    #[test]
    fn last_tool_summary_bounds_name_and_target() {
        let summary = summarize_tool_call(
            "Read",
            &json!({"file_path": "C:/very/long/path/to/a/file/that/keeps/the/tail.ts"}),
        )
        .expect("summary");
        assert_eq!(summary.name, "Read");
        let target = summary.target.expect("target");
        assert!(target.ends_with("tail.ts"), "{target}");

        let head = summarize_tool_call("Bash", &json!({"command": "cargo   test\nsecond line"}))
            .expect("summary");
        assert_eq!(head.target.as_deref(), Some("cargo test"));

        let long = summarize_tool_call("Bash", &json!({"command": "x".repeat(300)})).unwrap();
        assert_eq!(
            long.target.unwrap().chars().count(),
            LAST_TOOL_TARGET_MAX_CHARS
        );
        assert!(summarize_tool_call("  ", &json!({})).is_none());
    }
}
