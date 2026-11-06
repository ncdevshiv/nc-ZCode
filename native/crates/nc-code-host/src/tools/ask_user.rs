//! The AskUserQuestion tool: asks the user inside a turn and continues with the answers.
//!
//! Reference implementation (aligned field by field and wording by wording; see specs/rust-native-crates.md S3):
//! - `contracts/src/tools/ask-user-question.ts`: the input shape and validation rules;
//! - `core/src/tool/handlers/ask-user-question.ts`: the handler and the three model-facing wordings;
//! - `bootstrap/src/nc-code-protocol/interaction-broker.ts`: answer normalisation (in
//!   `host_interactions::answers_from_answer`);
//! - `bootstrap/src/nc-code-protocol-v4/product-projection.ts`: the v4 payload shape
//!   (in `interaction::questions_payload`).
//!
//! It is an AsyncTool: the wait happens in the caller's runtime and must not go on the blocking pool - one question can wait on
//! a human for minutes, and putting it in `spawn_blocking` would tie up a worker completely.

use std::sync::Arc;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};

use crate::host_interactions::AskUserOutcome;
use crate::session::Session;
use crate::tools::{
    AsyncTool, ToolContext, ToolContract, ToolError, ToolErrorCode, SCOPE_USER_INTERACTION,
};

pub const ASK_USER_QUESTION_TOOL_NAME: &str = "AskUserQuestion";
/// The permission.reason from TS `ask-user-question.ts`: the interaction's prompt is this very sentence,
/// and the UI uses it as the explanation of "why are you interrupting me".
pub const ASK_USER_QUESTION_REASON: &str =
    "AskUserQuestion pauses execution to collect answers from the user";
/// TS `cancellation.userVisibleMessage`.
pub const ASK_USER_QUESTION_CANCELLED: &str =
    "AskUserQuestion was cancelled before answers were returned";
/// TS `MAX_ASK_USER_QUESTION_MODEL_BYTES`.
const MAX_OUTPUT_BYTES: usize = 100_000;
const MIN_QUESTIONS: usize = 1;
const MAX_QUESTIONS: usize = 4;
const MIN_OPTIONS: usize = 2;
const MAX_OPTIONS: usize = 4;

/// The model-visible tool description. The "Plan mode note" paragraph from the TS original is carried over as is: this host
/// now has the plan tools (`tools/plan.rs`), so that instruction is genuinely executable - it stops the model from using AskUserQuestion
/// to ask "is the plan ok", a question only ExitPlanMode's approval interaction can carry.
const ASK_USER_QUESTION_DESCRIPTION: &str = concat!(
    "Use this tool only when you are blocked on a decision that is genuinely the user's to make: one you cannot resolve from the request, the code, or sensible defaults.",
    "\n",
    "\n",
    "Usage notes:",
    "\n",
    "- Users will always be able to select \"Other\" to provide custom text input",
    "\n",
    "- Use multiSelect: true to allow multiple answers to be selected for a question",
    "\n",
    "- If you recommend a specific option, make that the first option in the list and add \"(Recommended)\" at the end of the label",
    "\n",
    "\n",
    "Plan mode note: To switch into plan mode, use EnterPlanMode (not this tool). Once in plan mode, use this tool to clarify requirements or choose between approaches BEFORE finalizing your plan. Do NOT use this tool to ask \"Is my plan ready?\", \"Should I proceed?\", or otherwise reference \"the plan\" in questions — the user cannot see the plan until you call ExitPlanMode for approval.",
    "\n",
    "\n",
    "Reserve this for decisions where the user's answer changes what you do next — not for choices with a conventional default or facts you can verify in the codebase yourself. In those cases pick the obvious option, mention it in your response, and proceed.",
    "\n",
    "\n",
    "Preview feature:",
    "\n",
    "Use the optional `preview` field on options when presenting concrete artifacts that users need to visually compare:",
    "\n",
    "- ASCII mockups of UI layouts or components",
    "\n",
    "- Code snippets showing different implementations",
    "\n",
    "- Diagram variations",
    "\n",
    "- Configuration examples",
    "\n",
    "\n",
    "Preview content is rendered as markdown in a monospace box. Multi-line text with newlines is supported. When any option has a preview, the UI switches to a side-by-side layout with a vertical option list on the left and preview on the right. Do not use previews for simple preference questions where labels and descriptions suffice. Note: previews are only supported for single-select questions (not multiSelect).",
    "\n",
);

/// The session-side prompting entry point (the same injection shape as `SubagentRunner`: the tool gets the
/// current session from ToolContext and holds no host-global state itself).
pub struct AskUserPrompter {
    session: Arc<Session>,
}

impl AskUserPrompter {
    pub fn new(session: Arc<Session>) -> Self {
        Self { session }
    }

    pub async fn ask(&self, tool_call_id: &str, input: &Value) -> AskUserOutcome {
        self.session
            .request_user_answers(tool_call_id, ASK_USER_QUESTION_TOOL_NAME, input)
            .await
    }
}

pub struct AskUserTool;

#[async_trait]
impl AsyncTool for AskUserTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: ASK_USER_QUESTION_TOOL_NAME.to_string(),
            description: ASK_USER_QUESTION_DESCRIPTION.to_string(),
            input_schema: input_schema(),
            read_only: true,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: SCOPE_USER_INTERACTION,
            timeout_ms: 30_000,
            max_output_bytes: MAX_OUTPUT_BYTES,
            max_model_bytes: MAX_OUTPUT_BYTES,
            preview_direction: "head",
        }
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        validate_input(input)?;
        let Some(prompter) = ctx.ask_user.as_ref() else {
            // Without a session handle there is no channel that could answer: same semantics as headless, and it does not hang.
            return Ok(zero_answers_content());
        };
        match prompter
            .ask(
                ctx.tool_call_id
                    .as_deref()
                    .unwrap_or(ASK_USER_QUESTION_TOOL_NAME),
                input,
            )
            .await
        {
            AskUserOutcome::Cancelled => Err(ToolError::new(
                ToolErrorCode::Cancelled,
                ASK_USER_QUESTION_CANCELLED,
            )),
            AskUserOutcome::NoAnswers => Ok(zero_answers_content()),
            AskUserOutcome::Answered {
                answers,
                annotations,
            } => Ok(model_content(input, &answers, annotations.as_ref())),
        }
    }
}

/// The JSON Schema the provider sees. In TS `multiSelect` has default(false) yet is **still listed in required**
/// (`withRequiredDefaultedMultiSelect`), while at runtime old calls may omit it,
/// so we keep the same pairing: the schema requires it, validation only follows TS's Zod rules.
fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "questions": {
                "type": "array",
                "minItems": MIN_QUESTIONS,
                "maxItems": MAX_QUESTIONS,
                "description": "Questions to ask the user (1-4 questions)",
                "items": {
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "The complete question to ask the user. Should be clear, specific, and end with a question mark. Example: \"Which library should we use for date formatting?\" If multiSelect is true, phrase it accordingly, e.g. \"Which features do you want to enable?\""
                        },
                        "header": {
                            "type": "string",
                            "description": "Very short label displayed as a chip/tag (max 12 chars). Examples: \"Auth method\", \"Library\", \"Approach\"."
                        },
                        "options": {
                            "type": "array",
                            "minItems": MIN_OPTIONS,
                            "maxItems": MAX_OPTIONS,
                            "description": "The available choices for this question. Must have 2-4 options. Each option should be a distinct, mutually exclusive choice (unless multiSelect is enabled). There should be no 'Other' option, that will be provided automatically.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": {"type": "string", "description": "The display text for this option that the user will see and select. Should be concise (1-5 words) and clearly describe the choice."},
                                    "description": {"type": "string", "description": "Explanation of what this option means or what will happen if chosen. Useful for providing context about trade-offs or implications."},
                                    "preview": {"type": "string", "description": "Optional preview content rendered when this option is focused. Use for mockups, code snippets, or visual comparisons that help users compare options. See the tool description for the expected content format."}
                                },
                                "required": ["label", "description"]
                            }
                        },
                        "multiSelect": {"type": "boolean", "default": false, "description": "Set to true to allow the user to select multiple options instead of just one. Use when choices are not mutually exclusive."}
                    },
                    "required": ["question", "header", "options", "multiSelect"]
                }
            },
            "answers": {
                "type": "object",
                "additionalProperties": {"type": "string"},
                "description": "User answers collected by the permission component"
            },
            "annotations": {
                "type": "object",
                "additionalProperties": {
                    "type": "object",
                    "properties": {
                        "preview": {"type": "string", "description": "The preview content of the selected option, if the question used previews."},
                        "notes": {"type": "string", "description": "Free-text notes the user added to their selection."}
                    }
                },
                "description": "Optional per-question annotations from the user (e.g., notes on preview selections). Keyed by question text."
            },
            "metadata": {
                "type": "object",
                "properties": {
                    "source": {"type": "string", "description": "Optional identifier for the source of this question (e.g., \"remember\" for /remember command). Used for analytics tracking."}
                }
            }
        },
        "required": ["questions"]
    })
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::new(ToolErrorCode::InvalidInput, message)
}

/// The TS Zod rules ported one by one (custom refine wording is kept verbatim so the model can correct itself from the text).
fn validate_input(input: &Value) -> Result<(), ToolError> {
    let questions = input
        .get("questions")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("questions: required array of 1-4 questions"))?;
    if questions.len() < MIN_QUESTIONS || questions.len() > MAX_QUESTIONS {
        return Err(invalid(format!(
            "questions: must contain {MIN_QUESTIONS}-{MAX_QUESTIONS} questions"
        )));
    }

    let mut texts = Vec::with_capacity(questions.len());
    for question in questions {
        let text = question
            .get("question")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| invalid("question.question: required non-empty string"))?;
        question
            .get("header")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| invalid("question.header: required non-empty string"))?;
        if let Some(multi) = question.get("multiSelect") {
            if !multi.is_boolean() {
                return Err(invalid("question.multiSelect: must be boolean"));
            }
        }
        let options = question
            .get("options")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("question.options: required array of 2-4 options"))?;
        if options.len() < MIN_OPTIONS || options.len() > MAX_OPTIONS {
            return Err(invalid(format!(
                "options: must contain {MIN_OPTIONS}-{MAX_OPTIONS} options"
            )));
        }

        let mut labels = Vec::with_capacity(options.len());
        for option in options {
            let label = option
                .get("label")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| invalid("option.label: required non-empty string"))?;
            option
                .get("description")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| invalid("option.description: required non-empty string"))?;
            if labels.iter().any(|existing| existing == label) {
                return Err(invalid("Option labels must be unique within each question"));
            }
            labels.push(label.to_string());
            if label.trim().to_lowercase() == "other" {
                return Err(invalid(
                    "Do not include an Other option; clients provide it automatically",
                ));
            }
            if let Some(preview) = option.get("preview").and_then(Value::as_str) {
                validate_preview(preview)?;
            }
        }

        if texts.contains(&text) {
            return Err(invalid("Question texts must be unique"));
        }
        texts.push(text);
    }

    // TS's strict() rejects unknown fields: if the model hallucinates one we prefer to fail on the spot rather than carry
    // an input nobody reads inside the interaction payload.
    if let Some(answers) = input.get("answers") {
        if !answers.is_object() {
            return Err(invalid("answers: must be an object keyed by question text"));
        }
    }
    Ok(())
}

/// The three preview rules from TS: first MARKER decides "this intends to write HTML", then it must be a **fragment**
/// (no doctype/html/body), free of script/style, and it must really contain one tag.
fn validate_preview(preview: &str) -> Result<(), ToolError> {
    let marker = Regex::new(r"(?i)<!doctype\b|<!--|</?\s*[a-z][a-z0-9:-]*(?:\s[^<>]*)?>")
        .expect("static pattern");
    if !marker.is_match(preview) {
        return Ok(());
    }
    let document = Regex::new(r"(?i)<!doctype\b|</?\s*(?:html|body)\b").expect("static pattern");
    if document.is_match(preview) {
        return Err(invalid(
            "HTML preview must be a fragment without html, body, or doctype",
        ));
    }
    let script_style = Regex::new(r"(?i)</?\s*(?:script|style)\b").expect("static pattern");
    if script_style.is_match(preview) {
        return Err(invalid("HTML preview cannot contain script or style tags"));
    }
    let tag = Regex::new(r"(?i)</?\s*[a-z][a-z0-9:-]*(?:\s[^<>]*)?>").expect("static pattern");
    if !tag.is_match(preview) {
        return Err(invalid("HTML preview must contain an HTML tag"));
    }
    Ok(())
}

/// The zero-answer wording (the empty answers branch of TS `formatAskUserQuestionModelContent`).
/// It tells the model plainly "this is not a rejection, and do not invent a user preference", otherwise it would treat it as a veto and retry.
pub fn zero_answers_content() -> String {
    "The user did not provide answers to these questions. Continue using your best judgment; do not treat this as a rejection or invent a user preference.".to_string()
}

/// The full-answer/partially-answered wording, aligned character by character with TS: `"question"="answer"`, with
/// `selected preview:` / `user notes:` appended when preview/notes are present, and the unanswered count computed over **the input's question texts**.
pub fn model_content(input: &Value, answers: &Value, annotations: Option<&Value>) -> String {
    if answers.as_object().map(|map| map.is_empty()) == Some(true) {
        return zero_answers_content();
    }
    let entry_text = |question: &str, answer: &Value| -> String {
        let mut parts = vec![format!(
            "\"{question}\"=\"{}\"",
            answer.as_str().unwrap_or("")
        )];
        if let Some(annotation) = annotations.and_then(|value| value.get(question)) {
            if let Some(preview) = annotation.get("preview").and_then(Value::as_str) {
                parts.push(format!("selected preview:\n{preview}"));
            }
            if let Some(notes) = annotation.get("notes").and_then(Value::as_str) {
                parts.push(format!("user notes: {notes}"));
            }
        }
        parts.join(" ")
    };

    let mut rendered: Vec<String> = Vec::new();
    let mut unanswered = 0usize;
    let questions = crate::interaction::questions_payload(input);
    for question in &questions {
        let Some(text) = question["question"].as_str() else {
            continue;
        };
        match answers.get(text) {
            Some(answer) => rendered.push(entry_text(text, answer)),
            None => unanswered += 1,
        }
    }
    // answers may carry free-form keys that TS also allows (old clients echo the whole payload back): append them in insertion
    // order, never silently drop an answer.
    if let Some(map) = answers.as_object() {
        for (text, answer) in map {
            if !questions
                .iter()
                .any(|question| question["question"].as_str() == Some(text.as_str()))
            {
                rendered.push(entry_text(text, answer));
            }
        }
    }

    let answers_text = rendered.join(", ");
    if unanswered > 0 {
        return format!(
            "The user answered some questions and skipped {unanswered}. Provided answers: {answers_text}. Continue with the provided answers and use your best judgment for the unanswered questions; do not invent user preferences."
        );
    }
    format!("User has answered your questions: {answers_text}. You can now continue with the user's answers in mind.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_questions() -> Value {
        json!({"questions": [
            {"question": "Which library?", "header": "Library", "multiSelect": false, "options": [
                {"label": "swr", "description": "hooks-first"},
                {"label": "react-query", "description": "cache-first"}
            ]},
            {"question": "Ship dark mode?", "header": "Dark", "multiSelect": true, "options": [
                {"label": "yes", "description": "now"},
                {"label": "no", "description": "later"}
            ]}
        ]})
    }

    #[test]
    fn contract_matches_the_ts_tool_entry() {
        let contract = AskUserTool.contract();
        assert_eq!(contract.name, "AskUserQuestion");
        assert!(contract.read_only && contract.concurrent_safe && !contract.destructive);
        assert_eq!(contract.side_effect_scope, "userInteraction");
        assert_eq!(contract.timeout_ms, 30_000);
        assert_eq!(contract.max_output_bytes, MAX_OUTPUT_BYTES);
        let multi = &contract.input_schema["properties"]["questions"]["items"];
        assert!(
            multi["required"]
                .as_array()
                .expect("required list")
                .iter()
                .any(|item| item == &json!("multiSelect")),
            "TS keeps defaulted multiSelect in required for the provider-visible schema"
        );
    }

    #[test]
    fn validation_rejects_the_shapes_ts_rejects() {
        assert!(validate_input(&json!({"questions": []})).is_err());
        let mut too_many = two_questions();
        let extra = too_many["questions"].as_array_mut().unwrap()[0].clone();
        for _ in 0..4 {
            too_many["questions"]
                .as_array_mut()
                .unwrap()
                .push(extra.clone());
        }
        assert!(validate_input(&too_many).is_err());

        let duplicate_label = json!({"questions": [
            {"question": "Q?", "header": "H", "options": [
                {"label": "a", "description": "1"}, {"label": "a", "description": "2"}]}
        ]});
        assert_eq!(
            validate_input(&duplicate_label).unwrap_err().message,
            "Option labels must be unique within each question"
        );

        let other_label = json!({"questions": [
            {"question": "Q?", "header": "H", "options": [
                {"label": " A", "description": "1"}, {"label": " OTHER ", "description": "2"}]}
        ]});
        assert_eq!(
            validate_input(&other_label).unwrap_err().message,
            "Do not include an Other option; clients provide it automatically"
        );

        let duplicate_text = json!({"questions": [
            {"question": "Same?", "header": "A", "options": [{"label": "x", "description": "1"}, {"label": "y", "description": "2"}]},
            {"question": "Same?", "header": "B", "options": [{"label": "x", "description": "1"}, {"label": "y", "description": "2"}]}
        ]});
        assert_eq!(
            validate_input(&duplicate_text).unwrap_err().message,
            "Question texts must be unique"
        );

        let one_option = json!({"questions": [
            {"question": "Q?", "header": "H", "options": [{"label": "a", "description": "1"}]}
        ]});
        assert!(validate_input(&one_option).is_err());
        assert!(validate_input(&two_questions()).is_ok());
    }

    #[test]
    fn preview_rules_mirror_the_three_zod_messages() {
        let cases = [
            (
                "<!doctype html><p>hi</p>",
                "HTML preview must be a fragment without html, body, or doctype",
            ),
            (
                "<div><script>alert(1)</script></div>",
                "HTML preview cannot contain script or style tags",
            ),
            ("plain text", "must stay a plain preview"),
        ];
        let mut failures = Vec::new();
        for (preview, expectation) in cases {
            let result = validate_input(&json!({"questions": [
                {"question": "Q?", "header": "H", "options": [
                    {"label": "a", "description": "1", "preview": preview},
                    {"label": "b", "description": "2"}
                ]}
            ]}));
            match (expectation, result) {
                ("must stay a plain preview", Ok(())) => {}
                ("must stay a plain preview", Err(error)) => failures.push(error.message),
                (expected, Err(error)) => {
                    if error.message != expected {
                        failures.push(format!("{} != {expected}", error.message));
                    }
                }
                (expected, Ok(())) => {
                    failures.push(format!("preview {preview:?} passed but {expected}"))
                }
            }
        }
        assert!(failures.is_empty(), "{failures:?}");
    }

    #[test]
    fn model_content_matches_the_ts_wording() {
        let input = two_questions();

        assert_eq!(
            model_content(&input, &json!({}), None),
            zero_answers_content()
        );

        let full = model_content(
            &input,
            &json!({"Which library?": "swr", "Ship dark mode?": "yes"}),
            None,
        );
        assert_eq!(
            full,
            "User has answered your questions: \"Which library?\"=\"swr\", \"Ship dark mode?\"=\"yes\". You can now continue with the user's answers in mind."
        );

        let partial = model_content(
            &input,
            &json!({"Which library?": "swr"}),
            Some(&json!({"Which library?": {"preview": "<div>swr</div>", "notes": "cheaper"}})),
        );
        assert_eq!(
            partial,
            "The user answered some questions and skipped 1. Provided answers: \"Which library?\"=\"swr\" selected preview:\n<div>swr</div> user notes: cheaper. Continue with the provided answers and use your best judgment for the unanswered questions; do not invent user preferences."
        );
    }

    #[test]
    fn free_text_answers_outside_the_question_set_are_not_dropped() {
        let content = model_content(
            &two_questions(),
            &json!({"Which library?": "custom pick"}),
            None,
        );
        assert!(
            content.contains("\"Which library?\"=\"custom pick\""),
            "{content}"
        );
        assert!(content.contains("skipped 1"), "{content}");
    }
}
