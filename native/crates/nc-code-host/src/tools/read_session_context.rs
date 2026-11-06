//! `ReadSessionContext`: reads the context of **another persisted session** (TS's tool of the same name).
//!
//! Reference implementation: `core/src/tool/handlers/read-session-context.ts` +
//! `contracts/src/tools/read-session-context.ts`.
//!
//! **Honest boundary**: TS's `source` has four values, `lite|local|fallback|none`. In the `relevant`
//! strategy TS calls a lite model to extract from a long history; this host has no lite model channel, so only
//! **local selection** is implemented here (`source: "local"`) and `"lite"` is never claimed falsely:
//! - `handoff`: take the most recent messages in reverse time order up to the budget;
//! - `relevant`: select ordered by how many query terms hit each message, then reassemble in the original time order;
//!   no hit at all -> `NO_RELEVANT_CONTEXT` (same wording as TS).
//!
//! The budget is approximated as `maxTokens × 4` characters (TS uses a real tokenizer; the approximation is stated explicitly here).
//!
//! The data comes from the host's own `TranscriptStore` (the transcript is the durable carrier of session history),
//! injected by `HostServer` implementing [`SessionContextPort`] - the tool layer never reads the disk directly and never depends across layers.
use super::{
    optional_string, optional_u64, require_string, Tool, ToolContext, ToolContract, ToolError,
    ToolErrorCode,
};
use serde_json::{json, Value};
/// TS `READ_SESSION_CONTEXT_DEFAULT_MAX_TOKENS` / `…_MAX_TOKENS`.
const DEFAULT_MAX_TOKENS: u64 = 6_000;
const MAX_TOKENS: u64 = 12_000;
/// TS `MAX_READ_SESSION_CONTEXT_MODEL_BYTES` (the result budget).
const MAX_MODEL_BYTES: usize = 80_000;
/// TS `DEFAULT_TIMEOUT_MS` (budget for reading a large history + lite extraction; this host only does local selection).
const TIMEOUT_MS: u64 = 300_000;
pub const NO_RELEVANT_CONTEXT: &str = "NO_RELEVANT_CONTEXT";
#[derive(Debug, Clone)]
pub struct ContextMessage {
    pub role: String,
    pub text: String,
}
/// A readable snapshot of one session (projected by the host from the transcript).
#[derive(Debug, Clone)]
pub struct SessionContextSource {
    pub title: String,
    pub directory: String,
    pub path: String,
    pub messages: Vec<ContextMessage>,
}
/// The minimal read port from host to the tool layer (no store types exposed, and the tool never reads the disk itself).
pub trait SessionContextPort: Send + Sync {
    fn read(&self, session_id: &str) -> Option<SessionContextSource>;
}
/// Host-backed implementation: reads the session's authoritative row ledger and
/// projects user/assistant text into the tool's minimal message shape. It never reads
/// the disk directly and never exposes attachment bytes.
pub struct HostSessionContext {
    host: std::sync::Arc<crate::HostServer>,
}
impl HostSessionContext {
    pub fn new(host: std::sync::Arc<crate::HostServer>) -> Self {
        Self { host }
    }
}
impl SessionContextPort for HostSessionContext {
    fn read(&self, session_id: &str) -> Option<SessionContextSource> {
        let session = self.host.load_session(session_id)?;
        let (title, _) = session.title();
        let mut messages = Vec::new();
        for row in session.rows_snapshot() {
            let kind = row.get("kind").and_then(Value::as_str).unwrap_or_default();
            let role = match kind {
                "userInput" => "user",
                "assistantText" => "assistant", // Tool calls/results are not conversation context for this tool; the                // durable model history is already folded into the assistant text.
                _ => continue,
            };
            let Some(text) = row.get("text").and_then(Value::as_str) else {
                continue;
            };
            if text.trim().is_empty() {
                continue;
            }
            messages.push(ContextMessage {
                role: role.to_string(),
                text: text.to_string(),
            });
        }
        Some(SessionContextSource {
            title,
            directory: session.workspace_root.to_string_lossy().to_string(),
            path: self
                .host
                .session_dir(session_id)
                .to_string_lossy()
                .to_string(),
            messages,
        })
    }
}
pub struct ReadSessionContextTool;
fn approximate_budget_chars(max_tokens: u64) -> usize {
    (max_tokens.saturating_mul(4)) as usize
}
fn query_terms(query: &str) -> Vec<String> {
    query
        .split(|character: char| !character.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|term| term.len() >= 2)
        .collect()
}
fn score_message(message: &ContextMessage, terms: &[String]) -> usize {
    let haystack = message.text.to_lowercase();
    terms
        .iter()
        .filter(|term| haystack.contains(term.as_str()))
        .count()
}
fn format_message(message: &ContextMessage) -> String {
    format!("{}: {}", message.role, message.text)
}
/// Concatenates the selected messages in their original time order; returns (content, truncated, selectedCount).
fn assemble(
    source: &SessionContextSource,
    selected_indices: &[usize],
    budget_chars: usize,
) -> (String, bool, usize) {
    let mut content = String::new();
    let mut selected = 0usize;
    let mut truncated = false;
    for index in selected_indices {
        let Some(message) = source.messages.get(*index) else {
            continue;
        };
        let rendered = format_message(message);
        let extra = rendered.len() + if content.is_empty() { 0 } else { 2 };
        if content.len() + extra > budget_chars {
            truncated = true;
            break;
        }
        if !content.is_empty() {
            content.push_str("\n\n");
        }
        content.push_str(&rendered);
        selected += 1;
    }
    (content, truncated, selected)
}
fn select_handoff(source: &SessionContextSource, budget_chars: usize) -> (String, bool, usize) {
    let mut indices: Vec<usize> = (0..source.messages.len()).collect();
    // Accumulate backwards from the end until the budget would be exceeded; then concatenate in the original order.
    let mut used = 0usize;
    let mut start = source.messages.len();
    for (offset, index) in indices.iter().rev().enumerate() {
        let cost = format_message(&source.messages[*index]).len() + 2;
        if used + cost > budget_chars {
            break;
        }
        used += cost;
        start = source.messages.len() - 1 - offset;
    }
    indices = indices.split_off(start.min(indices.len()));
    let truncated = start > 0;
    let (content, overflowed, selected) = assemble(source, &indices, budget_chars);
    (content, truncated || overflowed, selected)
}
fn select_relevant(
    source: &SessionContextSource,
    query: &str,
    budget_chars: usize,
) -> (String, bool, usize) {
    let terms = query_terms(query);
    let mut scored: Vec<(usize, usize)> = source
        .messages
        .iter()
        .enumerate()
        .map(|(index, message)| (index, score_message(message, &terms)))
        .filter(|(_, score)| *score > 0)
        .collect();
    if scored.is_empty() {
        return (NO_RELEVANT_CONTEXT.to_string(), false, 0);
    }
    // More term hits first; on a tie the later (more recent) message wins.
    scored.sort_by(|left, right| right.1.cmp(&left.1).then(right.0.cmp(&left.0)));
    let budget = budget_chars;
    let mut chosen = Vec::new();
    let mut used = 0usize;
    let mut truncated = false;
    for (index, _) in scored {
        let cost = format_message(&source.messages[index]).len() + 2;
        if used + cost > budget {
            truncated = true;
            continue;
        }
        used += cost;
        chosen.push(index);
    }
    chosen.sort_unstable();
    let (content, overflowed, selected) = assemble(source, &chosen, budget_chars);
    (content, truncated || overflowed, selected)
}
impl Tool for ReadSessionContextTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "ReadSessionContext".to_string(),
            description: "Read persisted context from another ZCode session in this workspace. Use strategy=relevant for focused retrieval against a query, or strategy=handoff for a bounded summary of the most recent history.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sessionId": {"type": "string", "description": "Target session id to read from persisted history"},
                    "query": {"type": "string", "description": "Focused description of the context needed"},
                    "strategy": {"type": "string", "enum": ["relevant", "handoff"], "description": "relevant (default) or handoff"},
                    "maxTokens": {"type": "integer", "description": "Approximate maximum tokens to return (max 12000)"},
                },
                "required": ["sessionId", "query"],
            }),
            read_only: true,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: super::SCOPE_SESSION,
            timeout_ms: TIMEOUT_MS,
            max_output_bytes: MAX_MODEL_BYTES,
            max_model_bytes: MAX_MODEL_BYTES,
            preview_direction: "head",
        }
    }
    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let session_id = require_string(input, "sessionId")?.trim().to_string();
        let query = require_string(input, "query")?.to_string();
        if query.trim().is_empty() {
            return Err(ToolError::new(
                ToolErrorCode::InvalidInput,
                "query: must be a non-empty string",
            ));
        }
        let strategy = optional_string(input, "strategy").unwrap_or("relevant");
        if !matches!(strategy, "relevant" | "handoff") {
            return Err(ToolError::new(
                ToolErrorCode::InvalidInput,
                format!("strategy: expected relevant or handoff, got {strategy}"),
            ));
        }
        let max_tokens = optional_u64(input, "maxTokens")
            .unwrap_or(DEFAULT_MAX_TOKENS)
            .min(MAX_TOKENS);
        let budget_chars = approximate_budget_chars(max_tokens);
        let port = ctx.sessions.clone().ok_or_else(|| {
            ToolError::new(
                ToolErrorCode::ExecutionFailed,
                "SessionStorePort is not configured for ReadSessionContext",
            )
        })?;
        let Some(source) = port.read(&session_id) else {
            return Ok(json!({
                "status": "not_found",
                "sessionId": session_id,
                "strategy": strategy,
                "query": query,
                "source": "none",
                "content": format!("No local session found with id {session_id}."),
                "messageCount": 0,
                "truncated": false,
            })
            .to_string());
        };
        let (content, truncated, selected) = if strategy == "handoff" {
            select_handoff(&source, budget_chars)
        } else {
            select_relevant(&source, &query, budget_chars)
        };
        let mut output = json!({
            "status": "success",
            "sessionId": session_id,
            "title": source.title,
            "directory": source.directory,
            "path": source.path,
            "strategy": strategy,
            "query": query,
            "source": "local",
            "content": content,
            "messageCount": source.messages.len(),
            "selectedMessageCount": selected,
            "truncated": truncated,
        });
        if strategy == "relevant" && output["content"] == json!(NO_RELEVANT_CONTEXT) {
            output["selectedMessageCount"] = json!(0);
        }
        Ok(output.to_string())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::sync::Mutex;
    struct FakeSessions {
        messages: Vec<ContextMessage>,
    }
    impl SessionContextPort for FakeSessions {
        fn read(&self, session_id: &str) -> Option<SessionContextSource> {
            if session_id != "session-1" {
                return None;
            }

            Some(SessionContextSource {
                title: "previous work".to_string(),
                directory: "/workspace".to_string(),
                path: "/data/sessions/session-1".to_string(),
                messages: self.messages.clone(),
            })
        }
    }
    fn ctx(port: Option<Arc<dyn SessionContextPort>>) -> ToolContext {
        ToolContext {
            workspace_root: std::env::temp_dir(),
            full_access: true,
            interactive: false,
            trace_id: "t".to_string(),
            turn_id: "turn".to_string(),
            session_id: "session-2".to_string(),
            model_ref: None,
            read_state: Arc::new(Mutex::new(std::collections::HashSet::new())),
            cancel: Arc::new(AtomicBool::new(false)),
            todos: crate::tools::todo::new_todo_store(),
            provider_config: None,
            tool_call_id: None,
            subagent: None,
            ask_user: None,
            plan_mode: false,
            plan: None,
            background: None,
            automation: None,
            browser: None,
            computer: None,
            offpeak: None,
            sessions: port,
            workflow: None,
        }
    }
    fn messages() -> Vec<ContextMessage> {
        vec![
            ContextMessage {
                role: "user".to_string(),
                text: "please fix the auth bug".to_string(),
            },
            ContextMessage {
                role: "assistant".to_string(),
                text: "I will inspect the auth middleware".to_string(),
            },
            ContextMessage {
                role: "user".to_string(),
                text: "also update the readme".to_string(),
            },
        ]
    }
    #[test]
    fn missing_port_fails_closed_with_the_ts_message() {
        let error = ReadSessionContextTool
            .execute(
                &json!({"sessionId": "session-1", "query": "auth"}),
                &ctx(None),
            )
            .expect_err("no port");
        assert!(error.message.contains("SessionStorePort is not configured"));
    }
    #[test]
    fn missing_session_reports_not_found_without_faking_content() {
        let port = Arc::new(FakeSessions {
            messages: messages(),
        });
        let out = ReadSessionContextTool
            .execute(
                &json!({"sessionId": "does-not-exist", "query": "auth"}),
                &ctx(Some(port)),
            )
            .expect("read");
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["status"], json!("not_found"));
        assert_eq!(parsed["source"], json!("none"));
        assert_eq!(parsed["messageCount"], json!(0));
    }
    #[test]
    fn relevant_strategy_selects_only_matching_messages() {
        let port = Arc::new(FakeSessions {
            messages: messages(),
        });
        let out = ReadSessionContextTool
            .execute(
                &json!({"sessionId": "session-1", "query": "auth middleware"}),
                &ctx(Some(port)),
            )
            .expect("read");
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["source"], json!("local"));
        let content = parsed["content"].as_str().unwrap();
        assert!(content.contains("auth bug"), "{content}");
        assert!(
            !content.contains("readme"),
            "an unrelated message must not be selected: {content}"
        );
        assert_eq!(parsed["selectedMessageCount"], json!(2));
    }
    #[test]
    fn relevant_strategy_reports_no_relevant_context_when_nothing_matches() {
        let port = Arc::new(FakeSessions {
            messages: messages(),
        });
        let out = ReadSessionContextTool
            .execute(
                &json!({"sessionId": "session-1", "query": "kubernetes ingress"}),
                &ctx(Some(port)),
            )
            .expect("read");
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["content"], json!(NO_RELEVANT_CONTEXT));
        assert_eq!(parsed["selectedMessageCount"], json!(0));
    }
    #[test]
    fn handoff_strategy_keeps_the_most_recent_messages_within_budget() {
        let port = Arc::new(FakeSessions {
            messages: messages(),
        });
        let out = ReadSessionContextTool
            .execute(
                &json!({"sessionId": "session-1", "query": "anything", "strategy": "handoff", "maxTokens": 10}),
                &ctx(Some(port)),
            )
            .expect("read");
        let parsed: Value = serde_json::from_str(&out).unwrap();
        let content = parsed["content"].as_str().unwrap();
        assert!(
            content.contains("readme"),
            "the newest message must be kept: {content}"
        );
        assert_eq!(parsed["truncated"], json!(true));
    }
}
