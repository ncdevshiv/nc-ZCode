//! e2e: The real `nc-code-diff` binary uses the complete sidecar protocol,
//! Covers four method round-trips, service continuation after 400, timeout → null and exit code semantics.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use nc_code_sidecar_proto::VERSION_MISMATCH_PREFIX;
use serde_json::{json, Value};

/// Protocol client: single question and single answer (sidecar single-threaded serve, order determined).
struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Client {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_zcode-diff"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn nc-code-diff");
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout piped"));
        Self {
            child,
            stdin: Some(stdin),
            stdout,
        }
    }

    fn read_frame(&mut self) -> Value {
        let mut line = String::new();
        let read = self.stdout.read_line(&mut line).expect("read frame");
        assert!(read > 0, "sidecar closed stdout before expected response");
        serde_json::from_str(line.trim()).expect("frame must be json")
    }

    fn send_raw(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{line}").expect("write frame");
        stdin.flush().expect("flush frame");
    }

    fn send(&mut self, frame: Value) {
        let line = frame.to_string();
        self.send_raw(&line);
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"v":1,"id":id,"method":method,"params":params}));
        let frame = self.read_frame();
        assert_eq!(frame["id"], json!(id), "Response echo id");
        frame
    }

    fn wait_exit(&mut self) -> i32 {
        let status = self.child.wait().expect("wait exit");
        status.code().unwrap_or(-1)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_ready() -> Client {
    let mut client = Client::spawn();
    let ready = client.read_frame();
    assert_eq!(
        ready["event"],
        json!("ready"),
        "The first frame must be ready: {ready}"
    );
    assert_eq!(ready["data"]["sidecar"], json!("zcode-diff"));
    assert_eq!(ready["data"]["proto"], json!(1));
    client
}

fn assert_error_code(frame: &Value, code: u64) {
    assert_eq!(
        frame["ok"],
        json!(false),
        "Expected error response: {frame}"
    );
    assert_eq!(frame["error"]["code"], json!(code), "Error code: {frame}");
}

/// Completely disjoint large input: the number of edits peaks far after 1ms, timeoutMs=0 will inevitably time out.
fn disjoint_large() -> (String, String) {
    let old: String = (0..5000).map(|i| format!("old {i}\n")).collect();
    let new: String = (0..5000).map(|i| format!("new {i}\n")).collect();
    (old, new)
}

#[test]
fn protocol_lifecycle_ready_mismatch_404_and_shutdown() {
    let mut client = spawn_ready();

    // Version mismatch → 400 + prefix + exit 1 (fail-closed).
    client.send(json!({"v":2,"id":9,"method":"diff.lines","params":{"old":"a","new":"b"}}));
    let frame = client.read_frame();
    assert_error_code(&frame, 400);
    let message = frame["error"]["message"].as_str().expect("message");
    assert!(
        message.starts_with(VERSION_MISMATCH_PREFIX),
        "Messages must have version prefix: {message}"
    );
    assert_eq!(client.wait_exit(), 1, "version mismatch → exit 1");

    // New instance: bad line → proto_error event; unknown method → ​​404; shutdown → 0.
    let mut client = spawn_ready();
    client.send_raw("not json at all");
    let event = client.read_frame();
    assert_eq!(event["event"], json!("proto_error"));
    let resp = client.request(1, "no_such_method", json!({}));
    assert_error_code(&resp, 404);
    client.send(json!({"v":1,"id":2,"method":"shutdown","params":{}}));
    let frame = client.read_frame();
    assert_eq!(frame["ok"], json!(true));
    assert_eq!(client.wait_exit(), 0, "shutdown → exit 0");
}

#[test]
fn unified_and_structured_roundtrip_over_stdio() {
    let mut client = spawn_ready();

    // diff_unified: complete patch text (Index/file header/hunk line).
    let resp = client.request(
        1,
        "diff.unified",
        json!({
            "old": "a\nb\nc\n",
            "new": "a\nB\nc\n",
            "oldLabel": "f.txt",
            "newLabel": "f.txt",
        }),
    );
    assert_eq!(resp["ok"], json!(true), "diff_unified failed: {resp}");
    let patch = resp["result"]["patch"].as_str().expect("patch text");
    assert!(patch.starts_with("Index: f.txt\n"), "{patch}");
    assert!(patch.contains("@@ -1,3 +1,3 @@"), "{patch}");
    assert!(patch.contains("-b\n+B\n"), "{patch}");
    assert!(
        patch.ends_with('\n'),
        "patch must end with a newline: {patch}"
    );

    // structured_patch: bare patch object (no outer packaging).
    let resp = client.request(
        2,
        "structuredPatch",
        json!({
            "old": "a\nb\nc\n",
            "new": "a\nB\nc\n",
            "oldLabel": "f.txt",
            "newLabel": "f.txt",
            "context": 1,
        }),
    );
    assert_eq!(resp["ok"], json!(true), "structured_patch failed: {resp}");
    assert_eq!(resp["result"]["oldFileName"], json!("f.txt"));
    let hunks = resp["result"]["hunks"].as_array().expect("hunks");
    assert_eq!(hunks.len(), 1);
    let hunk = &hunks[0];
    // context=1: The preceding context is only 1 line (the first line), oldStart = 2-1 = 1.
    assert_eq!(hunk["oldStart"], json!(1));
    assert_eq!(hunk["oldLines"], json!(3));
    assert_eq!(hunk["lines"], json!([" a", "-b", "+B", " c"]));

    // No tag → undefined key omitted (mirrors JSON.stringify).
    let resp = client.request(3, "structuredPatch", json!({"old": "a\n", "new": "b\n"}));
    assert_eq!(resp["ok"], json!(true));
    assert!(
        resp["result"].get("oldFileName").is_none(),
        "No label shall appear oldFileName: {}",
        resp["result"]
    );

    client.send(json!({"v":1,"id":4,"method":"shutdown","params":{}}));
    assert_eq!(client.read_frame()["ok"], json!(true));
    assert_eq!(client.wait_exit(), 0);
}

#[test]
fn diff_lines_reports_chunks_with_explicit_booleans() {
    let mut client = spawn_ready();

    // Same input → single common chunk, added/removed explicit false.
    let resp = client.request(1, "diff.lines", json!({"old": "a\nb\n", "new": "a\nb\n"}));
    assert_eq!(resp["ok"], json!(true), "diff_lines failed: {resp}");
    let chunks = resp["result"]["chunks"].as_array().expect("chunks");
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0]["count"], json!(2));
    assert_eq!(chunks[0]["added"], json!(false));
    assert_eq!(chunks[0]["removed"], json!(false));
    assert_eq!(chunks[0]["value"], json!("a\nb\n"));

    // Insert → public + Add two new paragraphs.
    let resp = client.request(2, "diff.lines", json!({"old": "a\n", "new": "a\nb\n"}));
    let chunks = resp["result"]["chunks"].as_array().expect("chunks");
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[1]["added"], json!(true));
    assert_eq!(chunks[1]["value"], json!("b\n"));

    // empty ↔ empty → empty array.
    let resp = client.request(3, "diff.lines", json!({"old": "", "new": ""}));
    assert_eq!(resp["result"]["chunks"], json!([]));

    client.send(json!({"v":1,"id":4,"method":"shutdown","params":{}}));
    assert_eq!(client.read_frame()["ok"], json!(true));
    assert_eq!(client.wait_exit(), 0);
}

#[test]
fn apply_patch_success_failure_and_shape_errors() {
    let mut client = spawn_ready();

    // First use structured_patch to get the hunks, and then reinject them as they are (real combined links).
    let patch = client.request(
        1,
        "structuredPatch",
        json!({"old": "a\nb\nc\n", "new": "a\nB\nc\n"}),
    );
    let hunks = patch["result"]["hunks"].clone();

    let ok = client.request(
        2,
        "diff.applyPatch",
        json!({"source": "a\nb\nc\n", "hunks": hunks}),
    );
    assert_eq!(ok["ok"], json!(true), "apply failed: {ok}");
    assert_eq!(
        ok["result"],
        json!({"applied": true, "result": "a\nB\nc\n"})
    );

    // None of the contexts match → applied:false (not a protocol error).
    let fail = client.request(
        3,
        "diff.applyPatch",
        json!({
            "source": "zzz",
            "hunks": [{"oldStart":1,"oldLines":3,"newStart":1,"newLines":3,
                       "lines":[" a","-b","+B"," c"]}],
        }),
    );
    assert_eq!(
        fail["ok"],
        json!(true),
        "applied:false is still a successful response: {fail}"
    );
    assert_eq!(fail["result"], json!({"applied": false}));

    // Empty hunks → return source unchanged.
    let empty = client.request(4, "diff.applyPatch", json!({"source": "raw", "hunks": []}));
    assert_eq!(empty["result"], json!({"applied": true, "result": "raw"}));

    client.send(json!({"v":1,"id":5,"method":"shutdown","params":{}}));
    assert_eq!(client.read_frame()["ok"], json!(true));
    assert_eq!(client.wait_exit(), 0);
}

#[test]
fn invalid_params_map_to_400_and_keep_serving() {
    let mut client = spawn_ready();

    // Missing old → 400.
    assert_error_code(&client.request(1, "diff.unified", json!({"new": "x"})), 400);
    // old type does not match (empty string is legal input, no error will be reported) → only 400 for non-string.
    assert_error_code(
        &client.request(2, "diff.unified", json!({"old": 7, "new": "x"})),
        400,
    );
    // context negative / float → 400.
    assert_error_code(
        &client.request(
            3,
            "structuredPatch",
            json!({"old": "a", "new": "b", "context": -1}),
        ),
        400,
    );
    assert_error_code(
        &client.request(
            4,
            "structuredPatch",
            json!({"old": "a", "new": "b", "context": 1.5}),
        ),
        400,
    );
    // fuzzFactor non-0 → 400 (exact application only).
    let resp = client.request(
        5,
        "diff.applyPatch",
        json!({"source": "a", "hunks": [], "fuzzFactor": 1}),
    );
    assert_error_code(&resp, 400);
    assert!(
        resp["error"]["message"]
            .as_str()
            .expect("message")
            .contains("must be 0"),
        "{}",
        resp["error"]["message"]
    );
    // hunks illegal shape → 400.
    assert_error_code(
        &client.request(
            6,
            "diff.applyPatch",
            json!({"source": "a", "hunks": "nope"}),
        ),
        400,
    );
    assert_error_code(
        &client.request(
            7,
            "diff.applyPatch",
            json!({"source": "a", "hunks": [{"oldStart": 1}]}),
        ),
        400,
    );

    // After all 400, the service will still be normal, and finally exit gracefully.
    let ok = client.request(8, "diff.lines", json!({"old": "a\n", "new": "b\n"}));
    assert_eq!(
        ok["ok"],
        json!(true),
        "Service must continue after error: {ok}"
    );

    client.send(json!({"v":1,"id":9,"method":"shutdown","params":{}}));
    assert_eq!(client.read_frame()["ok"], json!(true));
    assert_eq!(client.wait_exit(), 0);
}

#[test]
fn timeout_zero_yields_null_patch_and_bare_structured_null() {
    let mut client = spawn_ready();
    let (old, new) = disjoint_large();

    let resp = client.request(
        1,
        "diff.unified",
        json!({"old": old, "new": new, "oldLabel": "f", "newLabel": "f", "timeoutMs": 0}),
    );
    assert_eq!(
        resp["ok"],
        json!(true),
        "Timeout is business null not an error: {resp}"
    );
    assert_eq!(
        resp["result"],
        json!({"patch": null}),
        "timeout → patch:null"
    );

    let resp = client.request(
        2,
        "structuredPatch",
        json!({"old": old, "new": new, "timeoutMs": 0}),
    );
    assert_eq!(
        resp["ok"],
        json!(true),
        "Timeout is business null not an error: {resp}"
    );
    assert_eq!(resp["result"], Value::Null, "timeout → naked null");

    client.send(json!({"v":1,"id":3,"method":"shutdown","params":{}}));
    assert_eq!(client.read_frame()["ok"], json!(true));
    assert_eq!(
        client.wait_exit(),
        0,
        "Exit gracefully after timeout request"
    );
}
