//! e2e：spawn 真实 `nc-code-watch` 二进制，通过 stdio 验证协议与事件流。
//!
//! 覆盖（spec S1 验收 + 任务清单）：ready 首帧、watch→create/modify/delete 事件
//! （10s 上界）、unwatch 后静默、malformed 行 → proto_error 且继续服务、版本不匹配
//! → exit 1、shutdown → exit 0、stdin EOF → exit 0、错误码映射（400/404）。

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const READY_TIMEOUT: Duration = Duration::from_secs(5);
/// spec 要求的事件断言上界。
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);
/// unwatch 后的静默观察窗（远大于测试用 40ms 防抖窗口）。
const QUIET_WINDOW: Duration = Duration::from_millis(700);
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(20);

struct Sidecar {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<String>,
}

impl Sidecar {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_zcode-watch"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn nc-code-watch");
        let stdout = child.stdout.take().expect("stdout piped");
        let stdin = child.stdin.take().expect("stdin piped");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = Vec::new();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let text = String::from_utf8_lossy(&line)
                            .trim_end_matches('\n')
                            .to_string();
                        if tx.send(text).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            child,
            stdin: Some(stdin),
            rx,
        }
    }

    fn send_line(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(line.as_bytes()).expect("write line");
        stdin.write_all(b"\n").expect("write newline");
        stdin.flush().expect("flush stdin");
    }

    fn send(&mut self, frame: &Value) {
        self.send_line(&frame.to_string());
    }

    fn recv(&self, timeout: Duration) -> Option<String> {
        self.rx.recv_timeout(timeout).ok()
    }

    fn next_value(&self, timeout: Duration) -> Value {
        let line = self
            .recv(timeout)
            .unwrap_or_else(|| panic!("no frame within {timeout:?}"));
        serde_json::from_str(&line).unwrap_or_else(|err| panic!("frame is not json: {line}: {err}"))
    }

    /// 读帧直到谓词命中（跳过不匹配的帧），整体受 timeout 约束。
    fn wait_for(&self, timeout: Duration, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "timed out waiting for frame");
            let value = self.next_value(remaining);
            if pred(&value) {
                return value;
            }
        }
    }

    fn wait_response(&self, id: u64, timeout: Duration) -> Value {
        self.wait_for(timeout, |frame| {
            frame.get("id").is_some_and(|got| *got == json!(id)) && frame.get("ok").is_some()
        })
    }

    fn wait_event(&self, event: &str, timeout: Duration) -> Value {
        self.wait_for(timeout, |frame| {
            frame.get("event").is_some_and(|name| *name == json!(event))
        })
    }

    fn wait_exit(mut self, timeout: Duration) -> i32 {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return status.code().expect("exit code"),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        panic!("child did not exit within {timeout:?}");
                    }
                    thread::sleep(EXIT_POLL_INTERVAL);
                }
                Err(err) => panic!("try_wait failed: {err}"),
            }
        }
    }
}

fn is_fs_event(frame: &Value, watch_id: &Value, path: &Path, kind: &str) -> bool {
    frame
        .get("event")
        .is_some_and(|name| *name == json!("fs_event"))
        && frame["data"]["watchId"] == *watch_id
        && frame["data"]["path"]
            .as_str()
            .is_some_and(|emitted| Path::new(emitted) == path)
        && frame["data"]["kind"].as_str() == Some(kind)
}

#[test]
fn ready_event_is_first_frame_then_shutdown_exits_zero() {
    let mut sidecar = Sidecar::spawn();
    let ready = sidecar.next_value(READY_TIMEOUT);
    assert_eq!(
        ready,
        json!({"v":1,"event":"ready","data":{"sidecar":"zcode-watch","proto":1}}),
        "首帧必须是 ready 事件"
    );

    sidecar.send(&json!({"v":1,"id":1,"method":"shutdown","params":{}}));
    let response = sidecar.wait_response(1, RESPONSE_TIMEOUT);
    assert_eq!(response["ok"], json!(true), "shutdown 必须回成功响应");
    assert_eq!(sidecar.wait_exit(EXIT_TIMEOUT), 0);
}

#[test]
fn watch_temp_dir_emits_create_change_delete_then_unwatch_stops_events() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut sidecar = Sidecar::spawn();
    sidecar.next_value(READY_TIMEOUT);

    sidecar.send(&json!({
        "v": 1, "id": 1, "method": "watch",
        "params": {"paths": [dir.path()], "recursive": true, "debounceMs": 40}
    }));
    let response = sidecar.wait_response(1, RESPONSE_TIMEOUT);
    assert_eq!(response["ok"], json!(true), "watch 失败: {response}");
    let watch_id = response["result"]["watchId"].clone();
    assert!(!watch_id.as_str().expect("watchId").is_empty());

    // create → 等到 create 事件再做下一步，保证每个动作落在独立防抖窗口。
    let target = dir.path().join("created.txt");
    std::fs::write(&target, b"hello").expect("create file");
    sidecar.wait_for(EVENT_TIMEOUT, |frame| {
        is_fs_event(frame, &watch_id, &target, "create")
    });

    std::fs::write(&target, b"hello with more content").expect("modify file");
    sidecar.wait_for(EVENT_TIMEOUT, |frame| {
        is_fs_event(frame, &watch_id, &target, "change")
    });

    std::fs::remove_file(&target).expect("delete file");
    sidecar.wait_for(EVENT_TIMEOUT, |frame| {
        is_fs_event(frame, &watch_id, &target, "delete")
    });

    // unwatch 后：新文件创建不得再产生任何帧。
    sidecar.send(&json!({"v":1,"id":2,"method":"unwatch","params":{"watchId": watch_id}}));
    let response = sidecar.wait_response(2, RESPONSE_TIMEOUT);
    assert_eq!(response["ok"], json!(true), "unwatch 失败: {response}");

    let stray = dir.path().join("after-unwatch.txt");
    std::fs::write(&stray, b"should not be reported").expect("write stray");
    assert!(
        sidecar.recv(QUIET_WINDOW).is_none(),
        "unwatch 后静默窗口内不得再收到任何帧"
    );

    sidecar.send(&json!({"v":1,"id":3,"method":"shutdown","params":{}}));
    let response = sidecar.wait_response(3, RESPONSE_TIMEOUT);
    assert_eq!(response["ok"], json!(true));
    assert_eq!(sidecar.wait_exit(EXIT_TIMEOUT), 0);
}

#[test]
fn malformed_line_emits_proto_error_and_sidecar_keeps_serving_then_eof_exits_zero() {
    let mut sidecar = Sidecar::spawn();
    sidecar.next_value(READY_TIMEOUT);

    sidecar.send_line("garbage! not json");
    let frame = sidecar.wait_event("proto_error", RESPONSE_TIMEOUT);
    assert_eq!(frame["data"]["raw"], json!("garbage! not json"));
    assert!(frame["data"]["reason"]
        .as_str()
        .expect("reason")
        .starts_with("invalid json"));

    // 超长行：raw 截断到 512 字节。
    let long_line = "y".repeat(600);
    sidecar.send_line(&long_line);
    let frame = sidecar.wait_event("proto_error", RESPONSE_TIMEOUT);
    assert_eq!(frame["data"]["raw"].as_str().expect("raw").len(), 512);

    // 服务未中断：合法请求仍可执行。
    sidecar.send(&json!({"v":1,"id":1,"method":"list","params":{}}));
    let response = sidecar.wait_response(1, RESPONSE_TIMEOUT);
    assert_eq!(response["ok"], json!(true));
    assert_eq!(response["result"]["ids"], json!([]));

    // stdin EOF → 优雅退出 0。
    drop(sidecar.stdin.take());
    assert_eq!(sidecar.wait_exit(EXIT_TIMEOUT), 0);
}

#[test]
fn version_mismatch_returns_400_then_exits_one() {
    let mut sidecar = Sidecar::spawn();
    sidecar.next_value(READY_TIMEOUT);

    sidecar.send(&json!({"v":2,"id":9,"method":"list","params":{}}));
    let response = sidecar.wait_response(9, RESPONSE_TIMEOUT);
    assert_eq!(response["ok"], json!(false));
    assert_eq!(response["error"]["code"], json!(400));
    assert!(response["error"]["message"]
        .as_str()
        .expect("message")
        .starts_with("PROTOCOL_VERSION_MISMATCH"));
    assert_eq!(
        sidecar.wait_exit(EXIT_TIMEOUT),
        1,
        "版本不匹配必须 fail closed"
    );
}

#[test]
fn method_error_paths_map_to_400_404_and_list_tracks_ids() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut sidecar = Sidecar::spawn();
    sidecar.next_value(READY_TIMEOUT);

    // 监视不存在的路径 → 400，文案镜像 TS。
    sidecar.send(&json!({
        "v": 1, "id": 1, "method": "watch",
        "params": {"paths": ["zcode-e2e-missing-dir-xyz"]}
    }));
    let response = sidecar.wait_response(1, RESPONSE_TIMEOUT);
    assert_eq!(response["ok"], json!(false));
    assert_eq!(response["error"]["code"], json!(400));
    assert!(
        response["error"]["message"]
            .as_str()
            .expect("message")
            .contains("无法监视目录"),
        "错误文案应保持 TS 形态: {response}"
    );

    // 未知 watchId unwatch → 404。
    sidecar.send(&json!({"v":1,"id":2,"method":"unwatch","params":{"watchId":"999"}}));
    let response = sidecar.wait_response(2, RESPONSE_TIMEOUT);
    assert_eq!(response["ok"], json!(false));
    assert_eq!(response["error"]["code"], json!(404));

    // 正常 watch → list 含 id → unwatch → list 为空。
    sidecar.send(&json!({"v":1,"id":3,"method":"watch","params":{"paths":[dir.path()]}}));
    let response = sidecar.wait_response(3, RESPONSE_TIMEOUT);
    assert_eq!(response["ok"], json!(true), "watch 失败: {response}");
    let watch_id = response["result"]["watchId"].clone();

    sidecar.send(&json!({"v":1,"id":4,"method":"list","params":{}}));
    let response = sidecar.wait_response(4, RESPONSE_TIMEOUT);
    assert_eq!(response["result"]["ids"], json!([watch_id]));

    sidecar.send(&json!({"v":1,"id":5,"method":"unwatch","params":{"watchId": watch_id}}));
    let response = sidecar.wait_response(5, RESPONSE_TIMEOUT);
    assert_eq!(response["ok"], json!(true));

    sidecar.send(&json!({"v":1,"id":6,"method":"list","params":{}}));
    let response = sidecar.wait_response(6, RESPONSE_TIMEOUT);
    assert_eq!(response["result"]["ids"], json!([]));

    sidecar.send(&json!({"v":1,"id":7,"method":"shutdown","params":{}}));
    let response = sidecar.wait_response(7, RESPONSE_TIMEOUT);
    assert_eq!(response["ok"], json!(true));
    assert_eq!(sidecar.wait_exit(EXIT_TIMEOUT), 0);
}
