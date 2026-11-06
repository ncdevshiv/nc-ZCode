//! e2e stdio: `v4/controller/*` (spec §1).
//!
//! Proves the stdio surface is wired with live state: create a session, then
//! subscribe to tasks-index and assert the new task appears in the snapshot.

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
    fn next_notification(&mut self) -> Value {
        loop {
            let message = self.next_line();
            if message.get("method").is_some() {
                return message;
            }
            self.pending.push_back(message);
        }
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zcode-host-controller-{name}-{}",
        std::process::id()
    ));
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
        "clientId": "controller-e2e",
        "sessionId": session_id,
        "payload": payload,
        "issuedAt": 1_700_000_000_000.0,
    })
}

#[test]
fn controller_tasks_snapshot_reflects_created_session() {
    let data_dir = temp_dir("tasks");
    let mut proc = HostProcess::spawn(&data_dir);
    let stdin = proc.stdin();
    let stdout = proc.stdout();
    let mut client = Client {
        id_counter: 0,
        stdin,
        stdout,
        pending: std::collections::VecDeque::new(),
    };

    let create_id = client.send(
        "v4/command",
        command_envelope("createSession", "cmd-c1", None, json!({})),
    );
    let created = client.response(create_id);
    assert_eq!(created["result"]["status"], json!("accepted"));
    let session_id = created["result"]["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    let sub_id = client.send(
        "v4/controller/subscribe",
        json!({ "topic": "controller/tasks-index" }),
    );
    let sub = client.response(sub_id);
    assert!(sub.get("error").is_none(), "subscribe must succeed: {sub}");
    let ack = &sub["result"]["ack"];
    assert_eq!(ack["mode"], json!("snapshot"));
    let subscription_id = ack["subscriptionId"]
        .as_str()
        .expect("subscriptionId")
        .to_string();

    let frame = client.next_notification();
    assert_eq!(frame["method"], json!("v4/controller/frame"));
    let params = &frame["params"];
    assert_eq!(params["topic"], json!("controller/tasks-index"));
    assert_eq!(params["subscriptionId"], json!(subscription_id));
    assert_eq!(params["fromSeq"], json!(0));
    assert_eq!(params["payload"]["kind"], json!("snapshot"));
    let tasks = params["payload"]["snapshot"]["tasks"]
        .as_array()
        .expect("tasks array");
    assert_eq!(
        tasks.len(),
        1,
        "snapshot must contain the created session: {tasks:?}"
    );
    let row = &tasks[0];
    assert_eq!(row["address"]["taskId"], json!(session_id));
    assert_eq!(
        row["address"]["workspacePath"],
        json!(env!("CARGO_MANIFEST_DIR"))
    );
    assert_eq!(row["meta"]["taskId"], json!(session_id));
    assert_eq!(row["membership"]["active"], json!(true));
    assert_eq!(row["sourceAvailability"], json!("online"));
    assert!(row["address"].get("remoteSessionId").is_none());
    let log_epoch = params["logEpoch"].as_str().expect("logEpoch").to_string();
    assert_eq!(params["payload"]["snapshot"]["logEpoch"], json!(log_epoch));
    assert_eq!(ack["logEpoch"], json!(log_epoch));

    // resync re-emits a snapshot for the same subscription.
    let rs_id = client.send(
        "v4/controller/resync",
        json!({ "subscriptionId": subscription_id }),
    );
    let rs = client.response(rs_id);
    assert!(rs.get("error").is_none(), "resync must succeed: {rs}");
    assert_eq!(
        rs["result"]["ack"]["subscriptionId"],
        json!(subscription_id)
    );
    let frame2 = client.next_notification();
    assert_eq!(frame2["method"], json!("v4/controller/frame"));
    let tasks2 = frame2["params"]["payload"]["snapshot"]["tasks"]
        .as_array()
        .expect("tasks array");
    assert_eq!(tasks2.len(), 1);

    // unknown subscription fails closed.
    let bad_id = client.send(
        "v4/controller/resync",
        json!({ "subscriptionId": "missing" }),
    );
    let bad = client.response(bad_id);
    assert_eq!(bad["error"]["code"], json!(-32602));

    // unsubscribe is idempotent.
    let un_id = client.send(
        "v4/controller/unsubscribe",
        json!({ "subscriptionId": subscription_id }),
    );
    let un = client.response(un_id);
    assert!(un.get("error").is_none(), "unsubscribe must succeed: {un}");
    let un2_id = client.send(
        "v4/controller/unsubscribe",
        json!({ "subscriptionId": subscription_id }),
    );
    let un2 = client.response(un2_id);
    assert!(
        un2.get("error").is_none(),
        "second unsubscribe must also succeed: {un2}"
    );

    // bad topic fails.
    let topic_id = client.send(
        "v4/controller/subscribe",
        json!({ "topic": "controller/nope" }),
    );
    let topic_err = client.response(topic_id);
    assert_eq!(topic_err["error"]["code"], json!(-32602));
}
