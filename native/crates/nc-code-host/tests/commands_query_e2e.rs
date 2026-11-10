//! e2e stdio：`v4/commands/query` 命令幂等查询面。
//!
//! 覆盖：createSession（global 桶）与 sendText（session 桶）落定的 ack 都能被查回；
//! 未知 key 答 `"unknown"` 而不是报错；`clock: true` 是纯时钟探测、答
//! `"unknown"` + `localTtftClockSchema`，且不触碰 session 账本；非法 params 走
//! JSON-RPC error（-32602）而不是塞进 result。
//!
//! 这些性质是本 increment 的全部产品面；单测覆盖解析与塑形，本文件证明
//! 「真的从 stdio 面上能用」，两者缺一都不算接线完成。

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
    let dir =
        std::env::temp_dir().join(format!("zcode-host-cmdquery-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn command_envelope(
    command_type: &str,
    command_id: &str,
    session_id: Option<&str>,
    payload: Value,
) -> Value {
    json!({
        "type": command_type,
        "commandId": command_id,
        "clientId": "cmdquery-client",
        "sessionId": session_id,
        "payload": payload,
        "issuedAt": 1_700_000_000_000.0,
    })
}

fn result_for<'a>(response: &'a Value, command_id: &str) -> &'a Value {
    response["result"]["results"]
        .as_array()
        .expect("results array")
        .iter()
        .find(|item| item["key"]["commandId"] == json!(command_id))
        .expect("query result for the requested key")
}

#[test]
fn commands_query_answers_settled_acks_and_unknown_for_the_rest() {
    let data_dir = temp_dir("settled");
    let mut process = HostProcess::spawn(&data_dir);
    let stdin = process.stdin();
    let stdout = process.stdout();
    let mut client = Client {
        id_counter: 0,
        stdin,
        stdout,
        pending: std::collections::VecDeque::new(),
    };

    // 1) createSession 落定 global 桶 ack。
    let id = client.send(
        "v4/command",
        command_envelope("createSession", "cmd-create", None, json!({})),
    );
    let created = client.response(id);
    assert_eq!(created["result"]["status"], json!("accepted"));
    let session_id = created["result"]["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    // 2) sendText 落定 session 桶 ack（无 provider -> fail-closed 终态，仍是已落定）。
    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-send-1",
            Some(&session_id),
            json!({"text": "hello"}),
        ),
    );
    let sent = client.response(id);
    assert_eq!(sent["result"]["status"], json!("accepted"));

    // 3) 查询：global 桶命中、未知 key 答 unknown。
    //
    // session 桶必须**轮询到终态**：sendText 是 admitted 命令，admission ack 不落账本，
    // 终态 ack 由 turn 任务在轮次结束后才写。TS 侧靠 readyFlights 等这一下，
    // 本 increment 没有那个等待（已记进 crate README 的差异清单），所以这里显式轮询。
    // 轮询本身也是被测性质之一：账本最终必须能查到，不能永远停在 unknown。
    let first_query = client.send(
        "v4/commands/query",
        json!({"commands": [
            {"sessionId": null, "commandId": "cmd-create"},
            {"sessionId": session_id, "commandId": "cmd-send-1"},
            {"sessionId": session_id, "commandId": "never-submitted"},
        ]}),
    );
    let mut response = client.response(first_query);
    let create = result_for(&response, "cmd-create");
    assert_eq!(create["result"]["commandId"], json!("cmd-create"));
    assert_eq!(create["result"]["status"], json!("accepted"));

    // session 桶：轮询到账本里出现终态 ack 为止。
    let mut attempts = 0;
    while result_for(&response, "cmd-send-1")["result"] == json!("unknown") {
        assert!(attempts < 100, "session ack never reached the ledger");
        attempts += 1;
        std::thread::sleep(std::time::Duration::from_millis(50));
        let poll = client.send(
            "v4/commands/query",
            json!({"commands": [{"sessionId": session_id, "commandId": "cmd-send-1"}]}),
        );
        response = client.response(poll);
    }
    // 命中后 status 必须是已知枚举值（accepted/rejected/stale/duplicate/noop/failed），
    // 不能是协议外的形状。fail-closed 场景下通常是 failed。
    let send = result_for(&response, "cmd-send-1");
    assert!(
        [
            "accepted",
            "rejected",
            "stale",
            "duplicate",
            "noop",
            "failed"
        ]
        .contains(&send["result"]["status"].as_str().unwrap_or_default()),
        "unexpected status: {}",
        send["result"]["status"]
    );

    // 未知 key 始终答 unknown，且不因轮询而变成别的东西。
    let id = client.send(
        "v4/commands/query",
        json!({"commands": [
            {"sessionId": null, "commandId": "cmd-create"},
            {"sessionId": session_id, "commandId": "cmd-send-1"},
            {"sessionId": session_id, "commandId": "never-submitted"},
        ]}),
    );
    let response = client.response(id);
    let results = response["result"]["results"].as_array().expect("results");
    assert_eq!(results.len(), 3, "one result per requested key, in order");
    assert_ne!(
        result_for(&response, "cmd-send-1")["result"],
        json!("unknown")
    );
    let unknown = result_for(&response, "never-submitted");
    assert_eq!(unknown["result"], json!("unknown"));
    assert_eq!(unknown["key"]["commandId"], json!("never-submitted"));
    assert_eq!(unknown["key"]["sessionId"], json!(session_id));
}

#[test]
fn commands_query_clock_is_a_pure_probe() {
    let data_dir = temp_dir("clock");
    let mut process = HostProcess::spawn(&data_dir);
    let stdin = process.stdin();
    let stdout = process.stdout();
    let mut client = Client {
        id_counter: 0,
        stdin,
        stdout,
        pending: std::collections::VecDeque::new(),
    };

    // 先落一条 global 桶 ack，证明 clock 分支**不**把它泄露出去。
    let id = client.send(
        "v4/command",
        command_envelope("createSession", "cmd-create", None, json!({})),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));

    let id = client.send(
        "v4/commands/query",
        json!({"commands": [{"sessionId": null, "commandId": "cmd-create"}], "clock": true}),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["results"][0]["result"], json!("unknown"));
    let clock = &response["result"]["clock"];
    let instance_id = clock["instanceId"].as_str().expect("instanceId");
    assert!(!instance_id.is_empty() && instance_id.len() <= 128);
    assert!(clock["receivedAt"].as_f64().expect("receivedAt") >= 0.0);
    assert!(clock["sentAt"].as_f64().expect("sentAt") >= clock["receivedAt"].as_f64().unwrap());
}

#[test]
fn commands_query_rejects_invalid_params_with_a_jsonrpc_error() {
    let data_dir = temp_dir("invalid");
    let mut process = HostProcess::spawn(&data_dir);
    let stdin = process.stdin();
    let stdout = process.stdout();
    let mut client = Client {
        id_counter: 0,
        stdin,
        stdout,
        pending: std::collections::VecDeque::new(),
    };

    // 空数组违反 min(1)。
    let id = client.send("v4/commands/query", json!({"commands": []}));
    let response = client.response(id);
    assert_eq!(response["error"]["code"], json!(-32602));
    assert!(
        response.get("result").is_none(),
        "errors must not carry a result"
    );

    // clock 不能带 session 桶 key（schema 的 .refine）。
    let id = client.send(
        "v4/commands/query",
        json!({"commands": [{"sessionId": "s1", "commandId": "c1"}], "clock": true}),
    );
    assert_eq!(client.response(id)["error"]["code"], json!(-32602));

    // strict：多余键必须被拒，而不是被静默忽略。
    let id = client.send(
        "v4/commands/query",
        json!({"commands": [{"sessionId": null, "commandId": "c1"}], "unexpected": true}),
    );
    assert_eq!(client.response(id)["error"]["code"], json!(-32602));

    // v4/controller/subscribe is wired (spec §1): ACK-only response, snapshot
    // arrives as a post-response v4/controller/frame notification.
    let id = client.send(
        "v4/controller/subscribe",
        json!({"topic": "controller/workspaces"}),
    );
    let response = client.response(id);
    assert!(
        response.get("error").is_none(),
        "controller subscribe must succeed: {response}"
    );
    let ack = &response["result"]["ack"];
    assert_eq!(ack["mode"], json!("snapshot"));
    assert!(!ack["subscriptionId"].as_str().unwrap_or("").is_empty());
    // The snapshot frame is read from the pending queue (post-response notification).
    // Drain one notification line and check its shape.
    let frame = client.next_notification();
    assert_eq!(frame["method"], json!("v4/controller/frame"));
    let params = &frame["params"];
    assert_eq!(params["topic"], json!("controller/workspaces"));
    assert_eq!(params["payload"]["kind"], json!("snapshot"));
    assert_eq!(params["payload"]["snapshot"]["protocolVersion"], json!(1));
}

/// MAJOR-1：本 increment 刻意让 global 桶账本**只在进程内**。这条测试把那个选择钉住：
/// 杀掉 host、用**同一个 data dir** 重启，再查同一个 createSession 的 commandKey，
/// 正确答案必须是 `"unknown"`（fail-closed），而不是把进程内存里的 ack 复活出来。
///
/// 没有这条测试，「刻意只在进程内」就只是散文；有了它，HEAD 曾经做过的那次 durable
/// write（`store.append_ack` 写进 `sessions/<id>/transcript.jsonl`）被移除这件事
/// 才有一个**可执行的**契约兜着。详见 RUST-PORT-LOG ENTRY 026 的审计小节。
#[test]
fn global_ack_is_volatile_by_design_across_restart() {
    let data_dir = temp_dir("restart");
    let mut first = HostProcess::spawn(&data_dir);
    let stdin = first.stdin();
    let stdout = first.stdout();
    let mut client = Client {
        id_counter: 0,
        stdin,
        stdout,
        pending: std::collections::VecDeque::new(),
    };

    // 第一次进程：createSession 落定一个 global 桶 ack。
    let id = client.send(
        "v4/command",
        command_envelope("createSession", "cmd-durable", None, json!({})),
    );
    let created = client.response(id);
    assert_eq!(created["result"]["status"], json!("accepted"));
    let session_id = created["result"]["result"]["sessionId"]
        .as_str()
        .expect("createSession 必须回 sessionId")
        .to_string();
    drop(client);

    // 杀掉第一个进程（Drop 会 kill + wait），确认它真的死了。
    drop(first);

    // 第二个进程：同一个 data dir。账本是进程内的，所以查不到。
    let mut second = HostProcess::spawn(&data_dir);
    let stdin = second.stdin();
    let stdout = second.stdout();
    let mut client = Client {
        id_counter: 0,
        stdin,
        stdout,
        pending: std::collections::VecDeque::new(),
    };
    let id = client.send(
        "v4/commands/query",
        json!({"commands": [{"sessionId": null, "commandId": "cmd-durable"}]}),
    );
    let response = client.response(id);
    assert_eq!(
        response["result"]["results"][0]["result"],
        json!("unknown"),
        "global 桶账本刻意只在进程内：重启后必须答 unknown（fail-closed）"
    );
    // 而且不能是错误：查询面必须仍然可用。
    assert!(response.get("error").is_none(), "查询面本身必须仍然工作");

    // 关键的一条：**按 session 查同一个 commandKey 也必须是 unknown**。
    // 上面那条 `sessionId: null` 走的是 `global_acks`（本次新增的唯一读点），
    // 而 HEAD 那次被移除的 durable write 落在 `Session.transcript_acks`，
    // 只能经 `Some(session_id)` → `Session::settled_ack` 读到。两条查询路径
    // 互斥，所以**只有这一条**会因为 durable write 是否被恢复而改变取值——
    // 没有它，这个测试在写回 durable 的情况下也会同样通过。
    let id = client.send(
        "v4/commands/query",
        json!({"commands": [{"sessionId": session_id, "commandId": "cmd-durable"}]}),
    );
    let response = client.response(id);
    assert_eq!(
        response["result"]["results"][0]["result"],
        json!("unknown"),
        "HEAD 上 createSession 的 ack 曾 durable 落进该 session 的 transcript；\
         本 increment 移除它后，重启后按 session 查也必须答 unknown"
    );
}
