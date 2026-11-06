//! e2e stdio: `v4/conversation/attachmentRead|Stat` (spec §4).
//!
//! Uploads a text attachment (begin/chunk/commit), sends it with `sendText`,
//! then proves the share read/stat surface authorizes by row/index and slices
//! bytes metadata-consistent with the store.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use serde_json::{json, Value};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

struct HostProcess {
    child: Child,
}

impl HostProcess {
    fn spawn(data_dir: &std::path::Path) -> Self {
        let bin = env!("CARGO_BIN_EXE_zcode-host");
        let mut command = Command::new(bin);
        command
            .arg("--workspace")
            .arg(env!("CARGO_MANIFEST_DIR"))
            .arg("--data-dir")
            .arg(data_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        let child = command.spawn().expect("spawn zcode-host");
        Self { child }
    }
    fn stdin(&mut self) -> std::process::ChildStdin {
        self.child.stdin.take().expect("stdin")
    }
    fn stdout(&mut self) -> BufReader<std::process::ChildStdout> {
        BufReader::new(self.child.stdout.take().expect("stdout"))
    }
}

impl Drop for HostProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Client {
    id_counter: u64,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    pending: std::collections::VecDeque<Value>,
}

impl Client {
    fn send(&mut self, method: &str, params: Value) -> u64 {
        self.id_counter += 1;
        let id = self.id_counter;
        let request = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let line = serde_json::to_string(&request).expect("request");
        self.stdin
            .write_all(line.as_bytes())
            .expect("write request");
        self.stdin.write_all(b"\n").expect("write newline");
        self.stdin.flush().expect("flush");
        id
    }
    fn next_line(&mut self) -> Value {
        if let Some(buffered) = self.pending.pop_front() {
            return buffered;
        }
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).expect("read line");
            assert!(read > 0, "stdout closed before a protocol line arrived");
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            return serde_json::from_str(trimmed).expect("protocol line must be JSON");
        }
    }
    fn response(&mut self, id: u64) -> Value {
        loop {
            let message = self.next_line();
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                return message;
            }
            self.pending.push_back(message);
        }
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("zcode-host-share-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn command_envelope(
    kind: &str,
    command_id: &str,
    session_id: Option<&str>,
    payload: Value,
) -> Value {
    json!({
        "type": kind,
        "commandId": command_id,
        "clientId": "share-e2e",
        "sessionId": session_id,
        "payload": payload,
        "issuedAt": 1_700_000_000_000.0,
    })
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(triple >> 18) as usize & 63] as char);
        out.push(TABLE[(triple >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(triple >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[triple as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    // Use a tiny pure-Rust hex of sha256 via the host's content_hash? For the
    // test binary we cannot link it; re-implement with `sha2` is not
    // available, so call the host through a real upload: the checksum must be
    // correct or commit fails. Use the test's own sha256 from `nc-code-host`'s
    // dependency tree by spawning a second verification: simpler — compute via
    // the `sha256` CLI is unavailable. Instead, import the logic from the test
    // file itself using a minimal implementation is too large; link the crate
    // under test via `nc_code_host::content_hash` (integration tests can use it).
    nc_code_host::content_hash::sha256_hex(bytes)
}

#[test]
fn share_read_and_stat_authorize_by_row_index() {
    let data_dir = temp_dir("share");
    let mut proc = HostProcess::spawn(&data_dir);
    let stdin = proc.stdin();
    let stdout = proc.stdout();
    let mut client = Client {
        id_counter: 0,
        stdin,
        stdout,
        pending: std::collections::VecDeque::new(),
    };

    // createSession
    let create_id = client.send(
        "v4/command",
        command_envelope("createSession", "cmd-s1", None, json!({})),
    );
    let created = client.response(create_id);
    assert_eq!(created["result"]["status"], json!("accepted"));
    let session_id = created["result"]["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    // upload a text/plain attachment (share must accept non-media MIME).
    let plaintext = b"hello share";
    let file_name = "note.txt";
    let mime = "text/plain";
    let checksum = format!("sha256:{}", sha256_hex(plaintext));
    let begin_id = client.send("v4/attachment/begin", json!({
        "connectionId": "conn-1", "sessionId": session_id, "uploadId": "up-1",
        "fileName": file_name, "mime": mime, "totalBytes": plaintext.len(), "totalChunks": 1, "checksum": checksum,
    }));
    let begin = client.response(begin_id);
    assert_eq!(begin["result"]["state"], json!("staging"));
    let chunk_id = client.send(
        "v4/attachment/chunk",
        json!({
            "connectionId": "conn-1", "sessionId": session_id, "uploadId": "up-1",
            "chunkIndex": 0, "dataBase64": base64_encode(plaintext),
        }),
    );
    let chunk = client.response(chunk_id);
    assert_eq!(chunk["result"]["nextChunkIndex"], json!(1));
    let commit_id = client.send(
        "v4/attachment/commit",
        json!({ "connectionId": "conn-1", "sessionId": session_id, "uploadId": "up-1" }),
    );
    let commit = client.response(commit_id);
    let att_ref = commit["result"]["ref"].as_str().expect("ref").to_string();
    assert!(att_ref.starts_with("zcode-artifact://"));

    // sendText with the attachment (provider-less turn still projects the row).
    let send_id = client.send("v4/command", command_envelope("sendText", "cmd-send-1", Some(&session_id), json!({
        "text": "see attached",
        "attachments": [{ "ref": att_ref, "fileName": file_name, "mime": mime, "bytes": plaintext.len() }],
    })));
    let sent = client.response(send_id);
    assert_eq!(sent["result"]["status"], json!("accepted"));

    // Poll rowsRange until the userInput row with attachments appears.
    let mut target = None;
    for _ in 0..50 {
        let rr_id = client.send(
            "v4/conversation/rowsRange",
            json!({ "sessionId": session_id }),
        );
        let rr = client.response(rr_id);
        let rows = rr["result"]["rows"].as_array().cloned().unwrap_or_default();
        for row in &rows {
            if row.get("kind").and_then(Value::as_str) == Some("userInput") {
                if let Some(atts) = row.get("attachments").and_then(Value::as_array) {
                    if !atts.is_empty() {
                        target =
                            Some(json!({ "rowId": row["rowId"], "entityId": row["entityId"] }));
                    }
                }
            }
        }
        if target.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let target = target.expect("userInput row with attachments must be projected");
    let row_id = target["rowId"].as_u64().expect("rowId");
    let entity_id = target["entityId"].as_str().expect("entityId").to_string();
    assert!(row_id >= 1);
    assert!(entity_id.starts_with("ui:"));

    // stat: metadata-only, agrees with upload bytes.
    let stat_id = client.send(
        "v4/conversation/attachmentStat",
        json!({ "sessionId": session_id, "ref": att_ref, "target": target, "attachmentIndex": 0 }),
    );
    let stat = client.response(stat_id);
    assert!(stat.get("error").is_none(), "stat must succeed: {stat}");
    assert_eq!(stat["result"]["mediaType"], json!(mime));
    assert_eq!(stat["result"]["totalBytes"], json!(plaintext.len()));

    // read: full bytes in one chunk (11 bytes < 512 KiB).
    let read_id = client.send("v4/conversation/attachmentRead", json!({ "sessionId": session_id, "ref": att_ref, "target": target, "attachmentIndex": 0, "offset": 0, "limit": 512 }));
    let read = client.response(read_id);
    assert!(read.get("error").is_none(), "read must succeed: {read}");
    assert_eq!(read["result"]["mediaType"], json!(mime));
    assert_eq!(read["result"]["totalBytes"], json!(plaintext.len()));
    assert_eq!(
        read["result"]["dataBase64"],
        json!(base64_encode(plaintext))
    );
    assert_eq!(read["result"]["nextOffset"], Value::Null);

    // unauthorized ref (wrong index) fails closed, never leaks bytes.
    let bad_id = client.send("v4/conversation/attachmentRead", json!({ "sessionId": session_id, "ref": att_ref, "target": target, "attachmentIndex": 5, "offset": 0, "limit": 512 }));
    let bad = client.response(bad_id);
    assert_eq!(bad["error"]["code"], json!(-32602));
    let reason = bad["error"]["data"]["reasonCode"].as_str().unwrap_or("");
    assert_eq!(reason, "fault.attachment.previewRefNotAuthorized");

    // missing target fails validation (not an auth oracle).
    let miss_id = client.send(
        "v4/conversation/attachmentStat",
        json!({ "sessionId": session_id, "ref": att_ref, "attachmentIndex": 0 }),
    );
    let miss = client.response(miss_id);
    assert_eq!(miss["error"]["code"], json!(-32602));

    // offset beyond end fails.
    let off_id = client.send("v4/conversation/attachmentRead", json!({ "sessionId": session_id, "ref": att_ref, "target": target, "attachmentIndex": 0, "offset": 999, "limit": 512 }));
    let off = client.response(off_id);
    assert_eq!(off["error"]["code"], json!(-32602));
    assert_eq!(
        off["error"]["data"]["reasonCode"],
        json!("fault.attachment.previewRangeInvalid")
    );
}
