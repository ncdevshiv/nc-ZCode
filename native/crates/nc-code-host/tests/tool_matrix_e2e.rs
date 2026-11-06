//! Tool matrix e2e: run the ported tools that previously had only unit tests **one by one through a real turn**.
//!
//! The same harness as `tool_loop_e2e.rs`: a real host process + real tool execution + real
//! writes to disk, with only the model side being a scripted SSE responder (the provider double lives only in the test process, and
//! the product code has zero doubles). What is asserted is row projections and disk facts, not "the call raised no error".
//!
//! Coverage: Grep / Glob / Edit (the real read-before-edit chain) / TodoWrite+TodoRead /
//! ListModels / Skill / ReadImage / WebFetch (private addresses fail-closed) / js /
//! Bash's **body budget** (200 KB of output must be squeezed down to 30 000 B in the next request body, and said out loud).
//! Not here: the Cron quartet lives in `nc-code-services-agent/tests/cron_tools_e2e.rs` (the automation port is
//! injected by the services composition root, and the stdio host does not register them), and WebSearch needs real network access (see the parity-table exemption).

mod common;

use common::*;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn root(name: &str) -> PathBuf {
    // Distinguished only by test name: a pid+timestamp key collides between tests launched in the same millisecond (a historical pitfall).
    std::env::temp_dir().join(format!("zcode-host-toolmatrix-{name}"))
}

/// The case's workspace: lay the fixtures down here first, then hand the same name to `drive`.
fn workspace(name: &str) -> PathBuf {
    let path = root(name).join("workspace");
    let _ = std::fs::remove_dir_all(root(name));
    std::fs::create_dir_all(&path).expect("workspace");
    path
}

fn write_provider_config(base: &Path, port: u16) -> PathBuf {
    let path = base.join("provider.json");
    std::fs::write(
        &path,
        json!({
            "kind": "anthropic",
            "baseUrl": format!("http://127.0.0.1:{port}"),
            "apiKey": "test-key-not-a-real-credential",
            "model": "test-model",
            "maxTokens": 1024,
        })
        .to_string(),
    )
    .expect("provider config");
    path
}

/// Runs `calls` in order inside one turn (each is a tool_use in a separate model response) and returns the projected rows.
fn drive(name: &str, calls: &[(&str, Value)]) -> Vec<Value> {
    let (rows, provider) = drive_with_provider(name, calls);
    // The responder's listener thread exits with the process; the workspace is kept here so the caller can verify disk facts.
    drop(provider);
    rows
}

/// As above, but the provider is left to the caller: needed when asserting the request body **the model really saw**
/// (the tool body budget takes effect in the next request, not on the row projection).
fn drive_with_provider(name: &str, calls: &[(&str, Value)]) -> (Vec<Value>, ScriptedResponder) {
    let base = root(name);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");

    let mut script: Vec<String> = calls
        .iter()
        .enumerate()
        .map(|(index, (tool, input))| {
            tool_use_sse_with_id(&format!("toolu_mx_{index}"), tool, input)
        })
        .collect();
    script.push(FINAL_TEXT_SSE.to_string());

    let (provider, port) = ScriptedResponder::spawn(script);
    let provider_config = write_provider_config(&base, port);
    let mut process = HostProcess::spawn(&workspace, &data_dir, &provider_config, true);
    let mut client = Client {
        id_counter: 0,
        stdin: process.stdin(),
        stdout: process.stdout(),
        pending: std::collections::VecDeque::new(),
    };

    let id = client.send(
        "v4/command",
        command_envelope("createSession", "cmd-create", None, json!({})),
    );
    let session_id = client.response(id)["result"]["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    let id = client.send(
        "v4/conversation/subscribe",
        json!({"sessionId": session_id, "connectionId": "conn-1", "clientMode": "desktop-continuous"}),
    );
    let _ = client.response(id);
    let _ = client.next_line(); // initial snapshot

    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-send",
            Some(&session_id),
            json!({"text": "run the tool"}),
        ),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));

    let expected = calls.len();
    let mut rows: Vec<Value> = Vec::new();
    let mut converged = false;
    for _ in 0..200 {
        let line = client.next_line();
        apply_to_rows(&mut rows, &deltas_from_frame(&line));
        let tool_rows = rows
            .iter()
            .filter(|row| row["kind"] == json!("toolCall"))
            .count();
        let header_done = rows
            .iter()
            .any(|row| row["kind"] == json!("turnHeader") && row["state"] != json!("running"));
        // The terminal criterion is "every tool has a row and the turn closed", not frame order (batching can merge several frames into one).
        if header_done && tool_rows >= expected {
            converged = true;
            break;
        }
    }
    assert!(
        converged,
        "{name}: turn did not converge: {}",
        serde_json::to_string_pretty(&rows).unwrap()
    );
    (rows, provider)
}

fn tool_row(rows: &[Value], tool: &str) -> Value {
    rows.iter()
        .find(|row| row["kind"] == json!("toolCall") && row["toolName"] == json!(tool))
        .unwrap_or_else(|| {
            panic!(
                "{tool} row must be projected: {}",
                serde_json::to_string_pretty(rows).unwrap()
            )
        })
        .clone()
}

fn tool_rows(rows: &[Value], tool: &str) -> Vec<Value> {
    rows.iter()
        .filter(|row| row["kind"] == json!("toolCall") && row["toolName"] == json!(tool))
        .cloned()
        .collect()
}

fn output_text(row: &Value) -> String {
    row["output"]["text"].as_str().unwrap_or("").to_string()
}

#[test]
fn grep_finds_the_needle_in_real_files() {
    let ws = workspace("grep");
    std::fs::create_dir_all(ws.join("src")).expect("src");
    std::fs::write(
        ws.join("src/a.rs"),
        "const a = 1;
//matrix-needle
const b = 2;
",
    )
    .expect("fixture");
    let rows = drive(
        "grep",
        &[
            // TS's default mode is files_with_matches: without output_mode it returns only paths.
            ("Grep", json!({"pattern": "matrix-needle"})),
            (
                "Grep",
                json!({"pattern": "matrix-needle", "output_mode": "content"}),
            ),
            // TS has long advertised these parameters to the model, so they must really take effect inside the turn.
            (
                "Grep",
                json!({"pattern": "matrix-needle", "output_mode": "content", "-C": 1, "head_limit": 3}),
            ),
        ],
    );
    let calls = tool_rows(&rows, "Grep");
    assert_eq!(calls.len(), 3, "Three Greps must have a final line");
    let default_shape = output_text(&calls[0]);
    assert!(
        default_shape.contains("a.rs") && !default_shape.contains("matrix-needle"),
        "The default mode only returns the file path, not the line content: {default_shape}"
    );
    let plain = output_text(&calls[1]);
    assert!(
        plain.contains("a.rs"),
        "grep must report the real file: {plain}"
    );
    assert!(
        plain.contains("matrix-needle") && !plain.contains("const a = 1"),
        "content mode displays content without adjacent lines: {plain}"
    );
    let context_text = output_text(&calls[2]);
    assert!(
        context_text.contains("const a = 1") && context_text.contains("const b = 2"),
        "-C 1 must bring out one line before and after: {context_text}"
    );
    assert_eq!(
        context_text.lines().count(),
        3,
        "head_limit 3 should be cut into three lines: {context_text}"
    );
}

#[test]
fn glob_lists_real_paths() {
    let ws = workspace("glob");
    std::fs::write(ws.join("only.rs"), "fn main() {}\n").expect("fixture");
    let rows = drive("glob", &[("Glob", json!({"pattern": "**/*.rs"}))]);
    let row = tool_row(&rows, "Glob");
    assert_eq!(row["status"], json!("success"), "{row}");
    assert!(
        output_text(&row).contains("only.rs"),
        "glob must list the file that really exists: {}",
        output_text(&row)
    );
}

#[test]
fn edit_requires_a_real_read_then_writes_the_file() {
    let ws = workspace("edit");
    std::fs::write(
        ws.join("target.txt"),
        (0..8)
            .map(|i| format!("value-v1-{i}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .expect("fixture");
    let rows = drive(
        "edit",
        &[
            ("Read", json!({"file_path": "target.txt"})),
            (
                "Edit",
                json!({"file_path": "target.txt", "old_string": "value-v1-3", "new_string": "value-v2-3"}),
            ),
        ],
    );
    let read = tool_row(&rows, "Read");
    assert_eq!(read["status"], json!("success"), "{read}");
    let edit = tool_row(&rows, "Edit");
    assert_eq!(edit["status"], json!("success"), "{edit}");
    // Beyond the row projection, the disk is checked too: the model saying it changed something does not count, the file really changing does.
    let on_disk = std::fs::read_to_string(ws.join("target.txt")).expect("read back");
    assert!(
        on_disk.contains("value-v2-3") && !on_disk.contains("value-v1-3"),
        "the file must actually change: {on_disk}"
    );
}

#[test]
fn todo_write_then_read_round_trips_through_the_session_store() {
    let _ = workspace("todo");
    let rows = drive(
        "todo",
        &[
            (
                "TodoWrite",
                json!({"todos": [{"content": "ship the native host", "status": "pending", "priority": "high"}]}),
            ),
            // TS dropped the hard refusal of "at most one in_progress" (contracts/tools/todo.ts:60-70),
            // so parallel sub-agents each opening one is legal input — this has to succeed inside a real host turn.
            (
                "TodoWrite",
                json!({"todos": [
                    {"content": "ship the native host", "status": "in_progress", "priority": "high"},
                    {"content": "benchmark every tool", "status": "in_progress", "priority": "medium"},
                ]}),
            ),
            ("TodoRead", json!({})),
        ],
    );
    let first = tool_row(&rows, "TodoWrite");
    assert_eq!(first["status"], json!("success"), "{first}");
    let writes = tool_rows(&rows, "TodoWrite");
    let second = writes.last().expect("second TodoWrite row");
    assert_eq!(second["status"], json!("success"), "{second}");
    let written = output_text(second);
    // What the model sees must be TS's return shape with TS's closed-set spellings: serde by default would write the variant name as
    // "inProgress", and the TS side's closed-set validation would then drop the entire list.
    let parsed: Value = serde_json::from_str(&written)
        .unwrap_or_else(|error| panic!("TodoWrite output must be JSON ({error}): {written}"));
    assert_eq!(
        parsed["summary"],
        json!({"total": 2, "pending": 0, "inProgress": 2, "completed": 0}),
        "The summary count of TS must be returned unchanged: {written}"
    );
    assert_eq!(
        parsed["oldTodos"],
        json!([{"content": "ship the native host", "status": "pending", "priority": "high"}]),
        "oldTodos must be the last one: {written}"
    );
    assert_eq!(
        parsed["todos"][0]["status"],
        json!("in_progress"),
        "The spelling of closed sets must be consistent with TS: {written}"
    );
    let read = tool_row(&rows, "TodoRead");
    assert_eq!(read["status"], json!("success"), "{read}");
    assert!(
        output_text(&read).contains("ship the native host"),
        "TodoRead must return what TodoWrite stored: {}",
        output_text(&read)
    );
}

#[test]
fn list_models_reflects_the_real_provider_config() {
    let _ = workspace("listmodels");
    let rows = drive("listmodels", &[("ListModels", json!({}))]);
    let row = tool_row(&rows, "ListModels");
    assert_eq!(row["status"], json!("success"), "{row}");
    assert!(
        output_text(&row).contains("test-model"),
        "catalog must come from the injected --provider-config: {}",
        output_text(&row)
    );
}

#[test]
fn skill_loads_a_real_skill_file() {
    let ws = workspace("skill"); // Join segment by segment: the host side also joins segment by segment, and shoving in "a/b/c" at once would leave the slashes verbatim, making the comparison falsely fail.
    let dir = ws.join(".agents").join("skills").join("matrix-skill");
    std::fs::create_dir_all(&dir).expect("skill dir");
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: matrix-skill\ndescription: a fixture skill\n---\nSkill body text for e2e.\nScript lives at ${ZCODE_SKILL_DIR}/run.sh; $HOME stays ${HOME}.\n",
    )
    .expect("write skill");
    let rows = drive("skill", &[("Skill", json!({"skill": "matrix-skill"}))]);
    let row = tool_row(&rows, "Skill");
    assert_eq!(row["status"], json!("success"), "{row}");
    let text = output_text(&row);
    assert!(
        text.contains("Skill body text"),
        "Skill must return the file body: {text}"
    );
    // Two behaviours with the same semantics as TS: the skill directory variable expands into a real directory, and the base directory is stated explicitly to the model.
    let expected_base = dir.to_string_lossy().into_owned();
    assert!(
        text.contains(&expected_base) && !text.contains("${ZCODE_SKILL_DIR}"),
        "skill dir variable must expand. expected={expected_base:?} contains={}\ntext={text}",
        text.contains(&expected_base)
    );
    assert!(
        text.contains("${HOME}"),
        "a foreign variable must be left alone: {text}"
    );
    // TS's body is the content after stripFrontmatter: the YAML header should not enter the model context.
    assert!(
        !text.contains("description: a fixture skill"),
        "frontmatter must not reach the model: {text}"
    );
    assert!(
        text.contains("Base directory for this skill:"),
        "the model must be told the base directory: {text}"
    );
}

#[test]
fn read_image_returns_a_real_image_payload() {
    // A 1x1 PNG (the publicly reproducible minimal image), not fabricated bytes.
    let bytes = base64_bytes("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==");
    let ws = workspace("image");
    std::fs::write(ws.join("pixel.png"), &bytes).expect("write png");
    let rows = drive("image", &[("ReadImage", json!({"file_path": "pixel.png"}))]);
    let row = tool_row(&rows, "ReadImage");
    assert_eq!(row["status"], json!("success"), "{row}");
    let text = output_text(&row);
    assert!(
        text.contains("png") || text.contains("image"),
        "ReadImage must report the real media type: {text}"
    );
}

/// WebFetch's private-address gate: the same rules as TS (`EgressBlocked`), and what the e2e pins is **fail-closed**,
/// not "whether the machine can reach the internet".
#[test]
fn webfetch_refuses_loopback_without_touching_the_network() {
    let _ = workspace("webfetch");
    let rows = drive(
        "webfetch",
        &[(
            "WebFetch",
            json!({"url": "http://127.0.0.1:9/x", "prompt": "summarise"}),
        )],
    );
    let row = tool_row(&rows, "WebFetch");
    assert_ne!(row["status"], json!("success"), "{row}");
    assert!(
        output_text(&row).contains("not fetchable"),
        "refusal must name the guard: {}",
        output_text(&row)
    );
}

#[test]
fn js_evaluates_real_javascript_when_node_exists() {
    let _ = workspace("js");
    let node = std::process::Command::new("node")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok();
    let rows = drive("js", &[("js", json!({"code": "1+1"}))]);
    let row = tool_row(&rows, "js");
    if node {
        assert_eq!(row["status"], json!("success"), "{row}");
        assert!(
            output_text(&row).contains('2'),
            "js must return the evaluated value: {}",
            output_text(&row)
        );
    } else {
        // On a machine without node we must not fake green either: the tool must honestly report the missing runtime.
        assert_ne!(row["status"], json!("success"), "{row}");
        assert!(
            output_text(&row).to_lowercase().contains("node"),
            "missing-runtime error must name node: {}",
            output_text(&row)
        );
    }
}

fn base64_bytes(text: &str) -> Vec<u8> {
    // Standard base64 decoding, but no dependency added for it: the character table is short enough to compute by hand.
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bits = 0u32;
    let mut value = 0u32;
    let mut out = Vec::new();
    for byte in text.bytes() {
        if byte == b'=' || byte == b'\n' {
            continue;
        }
        let index = TABLE
            .iter()
            .position(|entry| *entry == byte)
            .unwrap_or_else(|| panic!("not base64: {byte}"));
        value = (value << 6) | index as u32;
        bits += 6;
        if bits >= 8 {
            out.push((value >> (bits - 8)) as u8);
            bits -= 8;
        }
    }
    out
}

/// The tool body budget must **really take effect**: in production TS's `resultBudget` squeezes Bash's body down to 30 000 B
/// and previews it by **tail** (the errors and exit information are at the end of the output). This host originally only declared those numbers
/// without any execution path reading them — an output of tens of MB would be stuffed into the model context verbatim.
///
/// What is asserted is the tool_result inside the **next model request body** (where the budget takes effect), not the row projection:
/// the row side has its own 32 KiB ceiling (TS `buildToolOutput`), and the two are separate budgets.
#[test]
fn bash_output_beyond_the_tool_budget_is_bounded_before_it_reaches_the_model() {
    let _ = workspace("bash-budget");
    let command =
        r#"node -e "process.stdout.write('HEAD-MARKER-' + 'x'.repeat(200000) + '\nTAIL-MARKER-')""#;
    let (rows, provider) =
        drive_with_provider("bash-budget", &[("Bash", json!({"command": command}))]);
    let row = tool_row(&rows, "Bash");
    assert_eq!(row["status"], json!("success"), "{row}");

    let bodies = provider.request_bodies();
    let last = bodies.last().expect("The request body with tool_result");
    // Look only at the tool_result block: the request body as a whole also carries the assistant's tool_use input,
    // and that command itself contains these two marker strings (which is why the first version of the assertion misfired).
    let parsed: Value = serde_json::from_str(last).expect("request body is JSON");
    let tool_result = parsed["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .rev()
        .find_map(|message| {
            message["content"].as_array()?.iter().find_map(|block| {
                (block["type"] == json!("tool_result"))
                    .then(|| block["content"].as_str().unwrap_or_default())
            })
        })
        .expect("tool_result block");
    assert!(
        tool_result.contains("Tool output truncated by resultBudget"),
        "Truncation must make a sound (only the model will know how much is missing and whether it should change to a narrower command): {}",&tool_result[..tool_result.len().min(600)]
    );
    assert!(
        tool_result.contains("maxModelBytes=30000"),
        "The tag must contain a budget value (TS's Bash text budget): {tool_result}"
    );
    assert!(
        tool_result.contains("TAIL-MARKER"),
        "The tail direction must keep the end of the output: {tool_result}"
    );
    assert!(
        !tool_result.contains("HEAD-MARKER"),
        "The tail direction should not leave the beginning (otherwise the direction declaration will be false): {}",
        &tool_result[..tool_result.len().min(300)]
    );
    let x_count = tool_result.matches('x').count();
    assert!(
        x_count < 40_000,
        "The text must fall within the 30 000 B budget: tool_result counts {x_count} x"
    );
}

#[test]
fn apply_patch_adds_and_updates_real_files() {
    let ws = workspace("apply-patch");
    std::fs::write(ws.join("target.txt"), "a\nb\nc\n").expect("fixture");
    let add_patch = "*** Begin Patch\n*** Add File: added.txt\n+hello\n+world\n*** End Patch\n";
    let update_patch = "*** Begin Patch\n*** Update File: target.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n*** End Patch\n";
    let rows = drive(
        "apply-patch",
        &[
            ("ApplyPatch", json!({"patch_text": add_patch})),
            ("ApplyPatch", json!({"patch_text": update_patch})),
        ],
    );
    let add = tool_rows(&rows, "ApplyPatch");
    assert_eq!(add.len(), 2, "both ApplyPatch calls must project");
    assert_eq!(add[0]["status"], json!("success"), "{}", add[0]);
    assert_eq!(add[1]["status"], json!("success"), "{}", add[1]);
    let added = std::fs::read_to_string(ws.join("added.txt")).expect("added");
    assert!(
        added.contains("hello") && added.contains("world"),
        "{added}"
    );
    let updated = std::fs::read_to_string(ws.join("target.txt")).expect("updated");
    assert!(
        updated.contains('B') && !updated.contains("\nb\n"),
        "{updated}"
    );
}

#[test]
fn apply_patch_delete_removes_file_and_rejects_missing_target() {
    let ws = workspace("apply-patch-delete");
    std::fs::write(ws.join("gone.txt"), "bye\n").expect("fixture");
    let delete_patch = "*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch\n";
    let missing_patch = "*** Begin Patch\n*** Delete File: nope.txt\n*** End Patch\n";
    let rows = drive(
        "apply-patch-delete",
        &[
            ("ApplyPatch", json!({"patch_text": delete_patch})),
            ("ApplyPatch", json!({"patch_text": missing_patch})),
        ],
    );
    let calls = tool_rows(&rows, "ApplyPatch");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["status"], json!("success"), "{}", calls[0]);
    assert_ne!(calls[1]["status"], json!("success"), "{}", calls[1]);
    assert!(
        output_text(&calls[1]).contains("apply_patch_file_not_exist")
            || output_text(&calls[1]).to_lowercase().contains("not exist"),
        "{}",
        output_text(&calls[1])
    );
    assert!(
        !ws.join("gone.txt").exists(),
        "file must be deleted on disk"
    );
}

#[test]
fn websearch_rejects_invalid_input_before_network() {
    let _ = workspace("websearch-invalid");
    let rows = drive("websearch-invalid", &[("WebSearch", json!({"query": "x"}))]);
    let row = tool_row(&rows, "WebSearch");
    assert_ne!(row["status"], json!("success"), "{row}");
    let text = output_text(&row).to_lowercase();
    assert!(
        text.contains("invalid") || text.contains("query") || text.contains("2 characters"),
        "short query must fail validation: {}",
        output_text(&row)
    );
}
