# -*- coding: utf-8 -*-
# Compilation repairs: snapshot.rs (7 in the first compilation) + 3 in this round of service/session.
import io

failures = 0


def patch(path, pairs):
    global failures
    text = io.open(path, encoding="utf-8").read()
    for old, new, label in pairs:
        hits = text.count(old)
        if hits != 1:
            print("MISS(%d) [%s] in %s" % (hits, label, path))
            failures += 1
            continue
        text = text.replace(old, new)
        print("ok [%s]" % label)
    io.open(path, "w", encoding="utf-8", newline="").write(text)


snap = "native/crates/zcode-services-agent/src/snapshot.rs"

patch(
    snap,
    [
        # 1) Missing brackets (E0615)
        (
            " (!trimmed.is_empty()).then(|| trimmed.to_string)\n}",
            " (!trimmed.is_empty()).then(|| trimmed.to_string())\n}",
            "non_empty parens",
        ),
        # 2) Borrowed input cannot be returned as 'static (E lifetime × 2): changed to literal branch.
        (
            """pub fn sanitize_mode(mode: &str) -> &'static str {
    match mode {
        "plan" | "build" | "edit" | "yolo" | "auto" => mode,
        _ => DEFAULT_MODE,
    }
}""",
            """pub fn sanitize_mode(mode: &str) -> &'static str {
    // Literal return: directly inputting &str as 'static will not satisfy the borrowing requirement (E lifetime).
    match mode {
        "plan" => "plan",
        "build" => "build",
        "edit" => "edit",
        "yolo" => "yolo",
        "auto" => "auto",
        _ => DEFAULT_MODE,
    }
}""",
            "sanitize_mode 'static arms",
        ),
        (
            """ match value {
        "default" | "first_input" | "generated" | "custom" => Some(value),
        _ => None,
    }""",
            """ match value {
        //Same as sanitize_mode: closed set returns literal, no input parameters are borrowed.
        "default" => Some("default"),
        "first_input" => Some("first_input"),"generated" => Some("generated"),
        "custom" => Some("custom"),
        _ => None,
    }""",
            "title_source 'static arms",
        ),
        # 3) The predicate of Option<&Value>::filter is &&Value (E0631 ×2)
        (
            ''' if let Some(full_access) = payload.get("fullAccessOption").filter(Value::is_object) {''',
            ''' if let Some(full_access) = payload.get("fullAccessOption").filter(|value| value.is_object()) {''',
            "filter is_object",
        ),
        # 4) turn_has_later_tool was never constructed (dangling reference left by the previous round of interrupts):
        # The semantics of "Tool row following within the same turn" have been overridden by turn_max_tool_row > builder.row_id.
        (
            """ let later_tools = turn_has_later_tool
            .iter()
            .any(|(turn, _)| turn == &builder.turn_id);
        let max_tool_row = turn_max_tool_row
            .get(&builder.turn_id)
            .copied()
            .unwrap_or(0);
        if later_tools || max_tool_row > builder.row_id {""",
            """ // The original implementation refers here to turn_has_later_tool (a map that was never constructed). Semantics of the criterion
        // "Assistant followed by turn and tool row" has been overridden by turn_max_tool_row:
        // It records the maximum rowId of the tool row in each turn. If it is greater than the assistant's rowId, it means "follow".
        let max_tool_row = turn_max_tool_row
            .get(&builder.turn_id)
            .copied()
            .unwrap_or(0);
        if max_tool_row > builder.row_id {""",
            "dangling turn_has_later_tool removed",
        ),
        # 5) Both arms are actually to_string (&str's clone or &str) → directly converted to String.
        (
            """ let message_id = if response_id.starts_with("tool:") || response_id.starts_with("reasoning:") {
                    response_id.to_string()
                } else {
                    response_id.clone()
                };""",""" // The original if/else arms are .to_string() and .clone() (&str's clone is still
                // &str, the type does not match), the semantics are "as is", direct conversion.
                let message_id = response_id.to_string();""",
            "message_id collapse",
        ),
        # 6) or_else requires 0 argument closure (E0593)
        (
            """ let active_turn_id = if phase == "running" {
        running_turn_id(input.rows).or_else(current_turn_id)
    } else {
        None
    };""",
            """ let active_turn_id = if phase == "running" {
        running_turn_id(input.rows).or_else(|| current_turn_id(input.rows))
    } else {
        None
    };""",
            "or_else closure",
        ),
    ],
)

patch(
    "native/crates/zcode-services-agent/src/service.rs",
    [
        # Value does not contain_key (E0599)
        (
            ' _ if params.contains_key("thoughtLevel") => {',
            ' _ if params.get("thoughtLevel").is_some() => {',
            "thoughtLevel presence check",
        ),
        # The session id takes out the String first, and then gives the meta to fact (E0505 borrowing conflict)
        (
            """ let Some(session_id) = meta.get("sessionId").and_then(Value::as_str) else {
                    continue;
                };
                let fact = runtime
                    .as_ref()
                    .and_then(|runtime| runtime.session_view(session_id))
                    .unwrap_or(meta);
                let info = snapshot::session_info(&fact, &workspace_path, &key);
                if info.is_null() {
                    continue;
                }""",
            """ let Some(session_id) = meta
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                else {continue;
                };
                let fact = runtime
                    .as_ref()
                    .and_then(|runtime| runtime.session_view(&session_id))
                    .unwrap_or(meta);
                let info = snapshot::session_info(&fact, &workspace_path, &key);
                if info.is_null() {
                    continue;
                }""",
            "meta move fixed",
        ),
        (
            ''' rows.push((updated_at, session_id.to_string(), info));''',
            ''' rows.push((updated_at, session_id, info));''',
            "session_id already String",
        ),
    ],
)

patch(
    "native/crates/zcode-services-agent/src/session.rs",
    [
        # Vec<Value> without Display: Assert the message to use Debug.
        (
            ' assert_eq!(available.len(), 1, "{available:#}");',
            ' assert_eq!(available.len(), 1, "{available:#?}");',
            "vec debug format",
        ),
    ],
)

if failures:
    raise SystemExit("FAILED: %d" % failures)
print("all compile fixes landed")