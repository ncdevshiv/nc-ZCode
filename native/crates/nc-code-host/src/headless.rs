//! The renderer for the headless entry point (`nc-code-host --prompt`): turns the **very same** conversation frames into human-readable
//! output while owning no session fact itself.
//!
//! Why a separate module instead of parsing in place inside `main.rs`: this logic is a pure function (one in, one out), so
//! one can assert item by item "what goes to stdout, what goes to stderr, when is it over". Once the CLI starts guessing
//! "it has probably finished talking", behavior appears that only the terminal sees and the protocol does not.

use serde_json::Value;

/// One piece of visible output produced by the renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rendered {
    /// Assistant text going to stdout (a streaming increment or the part completed at the terminal state).
    Text(String),
    /// Diagnostics/notifications going to stderr.
    Notice(String),
    /// The turnHeader reached a terminal state: carries the terminal state and the full text seen so far, which the caller uses to decide the exit code.
    Done { status: String, text: String },
}

const TERMINAL_TURN_STATES: [&str; 3] = ["completedSuccess", "completedError", "cancelled"];

/// The rendering state of assistantText / toolCall rows. The key is rowId (`row.delta` carries only rowId).
#[derive(Default)]
pub struct HeadlessRenderer {
    assistant_rows: Vec<(u64, String)>,
    seen_tools: Vec<u64>,
    done: Option<(String, String)>,
}

impl HeadlessRenderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume one protocol line. Not JSON, not a conversation frame → only make noise (never drop it silently: that would make the CLI show a
    /// "success" that did nothing at all).
    pub fn consume_line(&mut self, line: &str) -> Vec<Rendered> {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        self.consume_value(&value)
    }

    /// The read-model rendering: feed in the rows returned by `v4/conversation/rowsRange` and render only rows after the cursor
    /// (`after_row_id`). Every row is handled as `row.upserted`, so **even if the same row is polled many
    /// times it only prints the text that has not appeared yet** — that is why headless can stay simple and still not repeat output.
    pub fn consume_rows(&mut self, rows: &[Value], after_row_id: u64) -> Vec<Rendered> {
        let mut out = Vec::new();
        for row in rows {
            let row_id = row.get("rowId").and_then(Value::as_u64).unwrap_or(0);
            if row_id <= after_row_id {
                continue;
            }
            let delta = serde_json::json!({"op": "row.upserted", "row": row});
            self.consume_delta(&delta, &mut out);
        }
        out
    }

    /// Whether this turn has entered a terminal state (the caller uses it to stop polling and decide the exit code).
    pub fn terminal(&self) -> Option<&(String, String)> {
        self.done.as_ref()
    }

    pub fn consume_value(&mut self, value: &Value) -> Vec<Rendered> {
        let mut out = Vec::new();
        // An error response (a JSON-RPC error member) must make noise: under headless this is the only clue.
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            out.push(Rendered::Notice(format!("error: {message}")));
            return out;
        }
        let payload = &value["params"]["frame"]["payload"];
        match payload.get("kind").and_then(Value::as_str) {
            Some("deltas") => {
                if let Some(deltas) = payload["deltas"].as_array() {
                    for delta in deltas {
                        self.consume_delta(delta, &mut out);
                    }
                }
            }
            // Rows in the initial snapshot (a reconnect / already existing history) are fed in by the same rule; no second read path is opened.
            Some("snapshot") => {
                if let Some(rows) = payload["snapshot"]["rows"]["window"].as_array() {
                    for row in rows {
                        let delta = serde_json::json!({"op": "row.appended", "row": row});
                        self.consume_delta(&delta, &mut out);
                    }
                }
            }
            _ => {}
        }
        out
    }

    fn consume_delta(&mut self, delta: &Value, out: &mut Vec<Rendered>) {
        match delta.get("op").and_then(Value::as_str) {
            Some("row.delta") => {
                if delta.get("path").and_then(Value::as_str) != Some("text") {
                    return;
                }
                let Some(row_id) = delta.get("rowId").and_then(Value::as_u64) else {
                    return;
                };
                let Some(append) = delta.get("append").and_then(Value::as_str) else {
                    return;
                };
                if let Some((_, text)) =
                    self.assistant_rows.iter_mut().find(|(id, _)| *id == row_id)
                {
                    text.push_str(append);
                    out.push(Rendered::Text(append.to_string()));
                }
            }
            Some("row.appended") | Some("row.upserted") => {
                let Some(row) = delta.get("row") else { return };
                match row.get("kind").and_then(Value::as_str) {
                    Some("assistantText") => self.assistant_row(row, delta["op"].as_str(), out),
                    Some("toolCall") => self.tool_row(row, out),
                    Some("turnHeader") => {
                        let status = row.get("state").and_then(Value::as_str).unwrap_or("");
                        if !TERMINAL_TURN_STATES.contains(&status) || self.done.is_some() {
                            return;
                        }
                        // The terminal text follows the projection (streaming increments may be fewer than the terminal state: mid-way cancellation is exactly that).
                        let text = self
                            .assistant_rows
                            .iter()
                            .map(|(_, text)| text.as_str())
                            .collect::<Vec<&str>>()
                            .join("\n");
                        self.done = Some((status.to_string(), text.clone()));
                        out.push(Rendered::Done {
                            status: status.to_string(),
                            text,
                        });
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn assistant_row(&mut self, row: &Value, op: Option<&str>, out: &mut Vec<Rendered>) {
        let Some(row_id) = row.get("rowId").and_then(Value::as_u64) else {
            return;
        };
        let text = row.get("text").and_then(Value::as_str).unwrap_or("");
        if op == Some("row.appended") {
            // appended is **opening a row**: in the live stream the text is still empty, while in a cold snapshot it already carries the historical body.
            // Neither case replays anything to stdout (the CLI must not print history it has already shown), but the body must
            // be recorded into the watermark, otherwise the following upsert would emit the whole history again as "the part not printed yet".
            if !self.assistant_rows.iter().any(|(id, _)| *id == row_id) {
                self.assistant_rows.push((row_id, text.to_string()));
            }
            return;
        }
        // The terminal upsert: completes the part that has not streamed out yet (never reprinting the prefix already seen).
        let entry = {
            self.assistant_rows
                .iter_mut()
                .find(|(id, _)| *id == row_id)
                .map(|(_, seen)| seen)
        };
        let Some(seen) = entry else {
            // This row was never seen (an upsert outside the cold snapshot): output the full text, it must not be dropped.
            if !text.is_empty() {
                self.assistant_rows.push((row_id, text.to_string()));
                out.push(Rendered::Text(text.to_string()));
            }
            return;
        };
        if text.len() > seen.len() && text.starts_with(seen.as_str()) {
            let rest = text[seen.len()..].to_string();
            *seen = text.to_string();
            out.push(Rendered::Text(rest));
        } else if text.len() >= seen.len() {
            *seen = text.to_string();
        }
    }

    fn tool_row(&mut self, row: &Value, out: &mut Vec<Rendered>) {
        let Some(row_id) = row.get("rowId").and_then(Value::as_u64) else {
            return;
        };
        let status = row.get("status").and_then(Value::as_str).unwrap_or("");
        let tool_name = row.get("toolName").and_then(Value::as_str).unwrap_or("?");
        // Only "needs approval" and "failed" make noise on stderr; successful execution does not disturb the pipes other than stdout.
        let noteworthy = matches!(status, "pendingApproval" | "error" | "cancelled");
        if !noteworthy {
            return;
        }
        if self.seen_tools.contains(&row_id) {
            return;
        }
        self.seen_tools.push(row_id);
        match status {
            "pendingApproval" => out.push(Rendered::Notice(format!(
                "note: {tool_name} needs approval; headless has no one to answer, so it will be declined"
            ))),
            "error" => {
                let message = row["error"]["message"]
                    .as_str()
                    .unwrap_or("tool failed");
                out.push(Rendered::Notice(format!("tool {tool_name} failed: {message}")));
            }
            _ => out.push(Rendered::Notice(format!("tool {tool_name} was cancelled"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame(payload: Value) -> Value {
        json!({"jsonrpc":"2.0","method":"v4/conversation/frame","params":{
            "frame": {"payload": payload}}})
    }

    fn deltas(list: Value) -> Value {
        frame(json!({"kind":"deltas","deltas": list}))
    }

    #[test]
    fn streamed_text_and_terminal_upsert_never_double_print() {
        let mut renderer = HeadlessRenderer::new();
        let opened = deltas(json!([{"op":"row.appended","row":{
            "rowId":3,"kind":"assistantText","text":"","state":"streaming"}}]));
        assert!(
            renderer.consume_value(&opened).is_empty(),
            "an empty open prints nothing"
        );

        let streamed = deltas(json!([
            {"op":"row.delta","rowId":3,"path":"text","append":"hel"},
            {"op":"row.delta","rowId":3,"path":"text","append":"lo"},
        ]));
        let rendered = renderer.consume_value(&streamed);
        assert_eq!(
            rendered,
            vec![Rendered::Text("hel".into()), Rendered::Text("lo".into())]
        );

        // The terminal upsert only completes what has not streamed out; not a single already printed character is repeated.
        let final_row = deltas(json!([{"op":"row.upserted","row":{
            "rowId":3,"kind":"assistantText","text":"hello world","state":"complete"}}]));
        assert_eq!(
            renderer.consume_value(&final_row),
            vec![Rendered::Text(" world".into())]
        );
    }

    #[test]
    fn turn_header_terminal_wins_and_reports_the_status() {
        let mut renderer = HeadlessRenderer::new();
        let running = deltas(json!([{"op":"row.appended","row":{
            "rowId":1,"kind":"turnHeader","state":"running"}}]));
        assert!(renderer.consume_value(&running).is_empty());

        let done = deltas(json!([{"op":"row.upserted","row":{
            "rowId":1,"kind":"turnHeader","state":"completedSuccess"}}]));
        let rendered = renderer.consume_value(&done);
        assert!(matches!(
            &rendered[..],
            [Rendered::Done { status, .. }] if status == "completedSuccess"
        ));
        // A second terminal state does not close out again (the CLI should exit exactly once).
        assert!(renderer.consume_value(&done).is_empty());
    }

    #[test]
    fn tool_notices_only_appear_for_approval_and_failure_states() {
        let mut renderer = HeadlessRenderer::new();
        let running = deltas(json!([{"op":"row.appended","row":{
            "rowId":4,"kind":"toolCall","toolName":"Write","status":"running"}}]));
        assert!(
            renderer.consume_value(&running).is_empty(),
            "a normal tool call must not pollute the CLI output"
        );

        let approval = deltas(json!([{"op":"row.upserted","row":{
            "rowId":4,"kind":"toolCall","toolName":"Write","status":"pendingApproval"}}]));
        let notices = renderer.consume_value(&approval);
        assert_eq!(notices.len(), 1);
        assert!(
            matches!(&notices[0], Rendered::Notice(text) if text.contains("headless has no one to answer"))
        );

        let failed = deltas(json!([{"op":"row.upserted","row":{
            "rowId":5,"kind":"toolCall","toolName":"Bash","status":"error",
            "error":{"code":"tool.permissionDenied","message":"denied"}}}]));
        let notices = renderer.consume_value(&failed);
        assert!(
            matches!(&notices[0], Rendered::Notice(text) if text.contains("Bash failed: denied"))
        );
    }

    #[test]
    fn jsonrpc_errors_are_reported_instead_of_a_silent_empty_run() {
        let mut renderer = HeadlessRenderer::new();
        let error = json!({"jsonrpc":"2.0","id":7,"error":{"code":-32602,"message":"params.sessionId: required"}});
        assert!(matches!(
            &renderer.consume_value(&error)[..],
            [Rendered::Notice(text)] if text.contains("params.sessionId: required")
        ));
    }

    #[test]
    fn snapshot_history_is_not_replayed_to_stdout_but_still_counts() {
        let mut renderer = HeadlessRenderer::new();
        let snapshot = frame(json!({"kind":"snapshot","snapshot":{
        "sessionId":"s","rows":{"window":[
            {"rowId":2,"kind":"assistantText","text":"already there","state":"complete"},
            {"rowId":1,"kind":"turnHeader","state":"completedSuccess"}
        ]}}}));
        let outputs = renderer.consume_value(&snapshot);
        assert!(
            !outputs.iter().any(|item| matches!(item, Rendered::Text(_))),
            "cold history must not be reprinted: {outputs:?}"
        );
        assert!(matches!(
            &outputs[..],
            [Rendered::Done { text, .. }] if text == "already there"
        ));
    }
}
