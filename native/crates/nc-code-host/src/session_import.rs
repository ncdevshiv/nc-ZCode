//! Session import (`session/import`): create a real session from another client's
//! transcript — currently Claude Code's native JSONL history, already parsed by the task
//! channel into visible user/assistant messages.
//!
//! Ownership: the **host** owns row shapes and transcript durability (the same
//! `open_session_with_id` path every other creation uses), so an imported session is
//! indistinguishable from a native one afterwards: the model reads its history, the UI
//! renders its rows, edit/retry/fork work on it.
//!
//! NO-INVENTION: only messages the caller actually extracted are persisted. The import
//! does not fabricate tool calls, file changes or usage — Claude Code's tool traffic is
//! not part of the visible transcript the user asked to migrate.

use serde_json::{json, Value};

use crate::rows::{self, RowBase, TurnHeaderFields};
use crate::rpc::{RpcError, CODE_INVALID_PARAMS};
use crate::sessions::SessionSeed;
use crate::store::is_safe_session_id;
use crate::HostServer;

const IMPORT_CLIENT_ID: &str = "claude-code-import";

fn text_of(message: Option<&nc_code_provider::request::Message>) -> String {
    message
        .map(|message| {
            message
                .parts
                .iter()
                .filter_map(|part| match part {
                    nc_code_provider::request::ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>()
        })
        .unwrap_or_default()
}

impl HostServer {
    /// `session/import` params:
    /// `{sessionId?, title?, createdAt?, updatedAt?, messages:[{role, content, timestamp?, model?}]}`.
    ///
    /// The caller resolves the source and its visible text (that is product policy in the
    /// task channel); this method turns it into the host's own facts: turnHeader /
    /// userInput / assistantText rows, the model history, and the
    /// "turn → history start" table branch editing needs.
    pub(crate) fn import_session(&self, params: &Value) -> Result<Value, RpcError> {
        let messages = params
            .get("messages")
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())
            .ok_or_else(|| {
                RpcError::new(
                    CODE_INVALID_PARAMS,
                    "params.messages: at least one message is required",
                )
            })?;
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.next_id("session"));
        if !is_safe_session_id(&session_id) {
            return Err(RpcError::new(
                CODE_INVALID_PARAMS,
                "params.sessionId: unsafe session id",
            ));
        }
        let title = params
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("Imported session {session_id}"));
        let created_at = params.get("createdAt").and_then(Value::as_i64);
        let updated_at = params.get("updatedAt").and_then(Value::as_i64);

        let mut rows: Vec<Value> = Vec::new();
        let mut history: Vec<nc_code_provider::request::Message> = Vec::new();
        let mut turn_starts: Vec<(String, usize)> = Vec::new();
        let mut row_id: u64 = 0;
        let mut seq: u64 = 0;
        // An assistant message before the first user turn has no turn to belong to; the
        // source parser can produce that shape (it skips the same case), so skip it here
        // too rather than inventing a turn header for it.
        // One open turn: (turnId, start ms, last-assistant ms, model, history index of
        // its user message). The history start is captured when the user message is
        // pushed — that is the anchor `turn_history_starts` and branch editing need.
        struct OpenTurn {
            turn_id: String,
            started_at: i64,
            ended_at: i64,
            model: Option<String>,
            history_start: usize,
        }
        let mut current: Option<OpenTurn> = None;
        let mut turn_index: usize = 0;

        fn flush_turn(
            current: &mut Option<OpenTurn>,
            history: &[nc_code_provider::request::Message],
            rows: &mut Vec<Value>,
            turn_starts: &mut Vec<(String, usize)>,
            row_id: &mut u64,
            seq: &mut u64,
            turn_index: usize,
        ) {
            let Some(turn) = current.take() else {
                return;
            };
            turn_starts.push((turn.turn_id.clone(), turn.history_start));
            *row_id += 1;
            *seq += 1;
            let header_base = RowBase {
                row_id: *row_id,
                turn_id: turn.turn_id.clone(),
                entity_id: format!("turn:{}", turn.turn_id),
                created_at: turn.started_at,
                created_at_seq: *seq,
            };
            rows.push(rows::turn_header_row(
                &header_base,
                TurnHeaderFields {
                    origin: "userInput",
                    state: "completedSuccess",
                    started_at: turn.started_at,
                    source_command_id: &turn.turn_id,
                    history_round_count: turn_index,
                    file_changes: None,
                    can_rewind_files: false,
                    execution_kind: Some("agent"),
                    workflow_launch: None,
                },
            ));
            *row_id += 1;
            *seq += 1;
            let user_text = text_of(history.get(turn.history_start));
            let user_base = RowBase {
                row_id: *row_id,
                turn_id: turn.turn_id.clone(),
                entity_id: format!("ui:{}", turn.turn_id),
                created_at: turn.started_at,
                created_at_seq: *seq,
            };
            rows.push(rows::user_input_row_with_origin(
                &user_base,
                &user_text,
                "realUser",
                &turn.turn_id,
                Some(IMPORT_CLIENT_ID),
                None,
                &[],
            ));
            let assistant_index = turn.history_start + 1;
            if let Some(assistant) = history
                .get(assistant_index)
                .filter(|message| message.role == nc_code_provider::request::MessageRole::Assistant)
            {
                let text = text_of(Some(assistant));
                *row_id += 1;
                *seq += 1;
                let assistant_base = RowBase {
                    row_id: *row_id,
                    turn_id: turn.turn_id.clone(),
                    entity_id: format!("resp-{}", turn.turn_id),
                    created_at: turn.ended_at,
                    created_at_seq: *seq,
                };
                rows.push(rows::assistant_text_row(
                    &assistant_base,
                    &format!("resp-{}", turn.turn_id),
                    &text,
                    "complete",
                    turn.model.as_deref(),
                    None,
                ));
            }
        }

        for message in messages {
            let role = message.get("role").and_then(Value::as_str).unwrap_or("");
            let content = message.get("content").and_then(Value::as_str).unwrap_or("");
            if content.trim().is_empty() {
                continue;
            }
            let timestamp = message
                .get("timestamp")
                .and_then(Value::as_i64)
                .or(created_at)
                .unwrap_or_else(crate::store::now_ms);
            match role {
                "user" => {
                    flush_turn(
                        &mut current,
                        &history,
                        &mut rows,
                        &mut turn_starts,
                        &mut row_id,
                        &mut seq,
                        turn_index,
                    );
                    let history_start = history.len();
                    history.push(nc_code_provider::request::Message::text(
                        nc_code_provider::request::MessageRole::User,
                        content,
                    ));
                    current = Some(OpenTurn {
                        turn_id: format!("import-{turn_index}"),
                        started_at: timestamp,
                        ended_at: timestamp,
                        model: None,
                        history_start,
                    });
                    turn_index += 1;
                }
                "assistant" => {
                    let Some(turn) = current.as_mut() else {
                        continue;
                    };
                    history.push(nc_code_provider::request::Message::text(
                        nc_code_provider::request::MessageRole::Assistant,
                        content,
                    ));
                    if turn.model.is_none() {
                        turn.model = message
                            .get("model")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    turn.ended_at = turn.ended_at.max(timestamp);
                }
                other => {
                    return Err(RpcError::new(
                        CODE_INVALID_PARAMS,
                        format!("params.messages: unsupported role {other}"),
                    ))
                }
            }
        }
        flush_turn(
            &mut current,
            &history,
            &mut rows,
            &mut turn_starts,
            &mut row_id,
            &mut seq,
            turn_index,
        );
        if history.is_empty() {
            return Err(RpcError::new(
                CODE_INVALID_PARAMS,
                "params.messages: no visible user/assistant content",
            ));
        }

        // 导入的是历史对话，不是在途轮次。phase 留 draft 会被任务列表映射成 running，
        // 所以按源 transcript 的真实收尾落定：末条是 assistant 回复 = 成功收口，
        // 末条是用户输入（Claude 那边被打断）= interrupted。
        let ended_with_assistant = history
            .last()
            .map(|message| message.role == nc_code_provider::request::MessageRole::Assistant)
            .unwrap_or(false);
        let session = self.open_session_with_id(
            session_id.clone(),
            SessionSeed {
                created_at,
                updated_at,
                title: title.clone(),
                title_source: "custom".to_string(),
                rows,
                history,
                turn_history_starts: turn_starts,
                ..SessionSeed::blank()
            },
        )?;
        session.set_phase(if ended_with_assistant {
            "completedSuccess"
        } else {
            "completedInterrupted"
        });
        Ok(json!({
            "sessionId": session.id,
            "title": title,
            "createdAt": session.created_at(),
            "updatedAt": session.updated_at(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::is_safe_session_id;

    fn server() -> std::sync::Arc<HostServer> {
        let base = std::env::temp_dir().join(format!(
            "zcode-host-import-{}-{}",
            std::process::id(),
            crate::store::now_ms()
        ));
        std::fs::create_dir_all(base.join("workspace")).expect("workspace");
        std::fs::create_dir_all(base.join("data")).expect("data");
        HostServer::new(
            base.join("workspace"),
            base.join("data"),
            None,
            true,
            Vec::new(),
        )
        .expect("host")
        .0
    }

    /// The imported session must be a real session: rows the UI can render, model history
    /// the next turn actually receives, and the turn→history table branch editing needs.
    #[tokio::test]
    async fn import_builds_real_rows_and_history() {
        let server = server();
        let result = server
            .import_session(&json!({
                "sessionId": "claude-import-abc123def456ghi789jkl0",
                "title": "Fix the build",
                "createdAt": 1_700_000_000_000i64,
                "messages": [
                    { "role": "user", "content": "why does the build fail?", "timestamp": 1_700_000_000_000i64 },
                    { "role": "assistant", "content": "a missing import", "timestamp": 1_700_000_005_000i64, "model": "claude-3" },
                    { "role": "user", "content": "fix it", "timestamp": 1_700_000_010_000i64 },
                ]
            }))
            .expect("import");
        assert_eq!(
            result["sessionId"],
            json!("claude-import-abc123def456ghi789jkl0")
        );
        let session = server
            .session("claude-import-abc123def456ghi789jkl0")
            .expect("session registered");
        let rows = session.rows_snapshot();
        assert_eq!(rows.len(), 5, "{rows:?}");
        assert_eq!(rows[0]["kind"], json!("turnHeader"));
        assert_eq!(rows[1]["kind"], json!("userInput"));
        assert_eq!(rows[2]["kind"], json!("assistantText"));
        assert_eq!(rows[2]["model"], json!("claude-3"));
        assert_eq!(rows[3]["kind"], json!("turnHeader"));
        assert_eq!(rows[4]["kind"], json!("userInput"));
        // Row ids are contiguous and never reused (branch editing depends on it).
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row["rowId"], json!(index as u64 + 1), "{row}");
        }
        // The model sees the imported history.
        let history = session.history_snapshot().expect("history");
        assert_eq!(history.len(), 3);
        assert_eq!(session.created_at(), 1_700_000_000_000);
        assert_eq!(session.title().0, "Fix the build");
        // Turn→history starts: turn 0 starts at message 0, turn 1 at message 2.
        assert_eq!(
            session.turn_history_snapshot(),
            vec![("import-0".to_string(), 0), ("import-1".to_string(), 2)]
        );
    }

    /// Re-importing into an existing id is refused instead of overwriting the transcript.
    #[tokio::test]
    async fn import_refuses_a_colliding_session_id() {
        let server = server();
        let payload = json!({
            "sessionId": "claude-import-collide",
            "messages": [{ "role": "user", "content": "hello" }],
        });
        server.import_session(&payload).expect("first import");
        let error = server.import_session(&payload).expect_err("second import");
        assert_eq!(error.code, crate::rpc::CODE_INVALID_PARAMS);
        assert!(
            error.message.contains("already exists"),
            "{}",
            error.message
        );
    }

    /// Assistant text before any user turn has no turn to live in; it is skipped rather
    /// than attached to an invented header.
    #[tokio::test]
    async fn a_leading_assistant_message_is_skipped() {
        let server = server();
        server
            .import_session(&json!({
                "sessionId": "claude-import-leading",
                "messages": [
                    { "role": "assistant", "content": "orphan" },
                    { "role": "user", "content": "real question" },
                    { "role": "assistant", "content": "real answer" },
                ]
            }))
            .expect("import");
        let session = server.session("claude-import-leading").expect("session");
        assert_eq!(session.rows_snapshot().len(), 3, "one turn only");
        assert_eq!(session.history_len(), 2, "the orphan is not in history");
    }

    /// The generated ids the task channel uses must stay safe for the store layout.
    #[test]
    fn imported_ids_are_store_safe() {
        let id = format!(
            "claude-import-{}",
            &crate::content_hash::sha256_hex(b"claude:path:sid")[..24]
        );
        assert!(is_safe_session_id(&id), "{id}");
    }
}
