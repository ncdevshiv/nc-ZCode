//! e2e：真实 `zcode-diff` 二进制走完整 sidecar 协议，
//! 覆盖四个方法的往返、400 后继续服务、超时 → null 与退出码语义。

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};
use zcode_sidecar_proto::VERSION_MISMATCH_PREFIX;

/// 协议客户端：单问单答（sidecar 单线程 serve，顺序确定）。
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
            .expect("spawn zcode-diff");
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
        assert!(read > 0, "sidecar 在预期响应前关闭了 stdout");
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
        assert_eq!(frame["id"], json!(id), "响应回显 id");
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
    assert_eq!(ready["event"], json!("ready"), "首帧必须是 ready: {ready}");
    assert_eq!(ready["data"]["sidecar"], json!("zcode-diff"));
    assert_eq!(ready["data"]["proto"], json!(1));
    client
}

fn assert_error_code(frame: &Value, code: u64) {
    assert_eq!(frame["ok"], json!(false), "期望错误响应: {frame}");
    assert_eq!(frame["error"]["code"], json!(code), "错误码: {frame}");
}

/// 完全不相交的大输入：编辑数触顶远在 1ms 之后，timeoutMs=0 必然超时。
fn disjoint_large() -> (String, String) {
    let old: String = (0..5000).map(|i| format!("old {i}\n")).collect();
    let new: String = (0..5000).map(|i| format!("new {i}\n")).collect();
    (old, new)
}

#[test]
fn protocol_lifecycle_ready_mismatch_404_and_shutdown() {
    let mut client = spawn_ready();

    // 版本不匹配 → 400 + 前缀 + exit 1（fail-closed）。
    client.send(json!({"v":2,"id":9,"method":"diff.lines","params":{"old":"a","new":"b"}}));
    let frame = client.read_frame();
    assert_error_code(&frame, 400);
    let message = frame["error"]["message"].as_str().expect("message");
    assert!(
        message.starts_with(VERSION_MISMATCH_PREFIX),
        "消息须带版本前缀: {message}"
    );
    assert_eq!(client.wait_exit(), 1, "版本不匹配 → exit 1");

    // 新实例：坏行 → proto_error 事件；未知方法 → 404；shutdown → 0。
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

    // diff_unified：完整 patch 文本（Index/文件头/hunk 行）。
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
    assert_eq!(resp["ok"], json!(true), "diff_unified 失败: {resp}");
    let patch = resp["result"]["patch"].as_str().expect("patch 文本");
    assert!(patch.starts_with("Index: f.txt\n"), "{patch}");
    assert!(patch.contains("@@ -1,3 +1,3 @@"), "{patch}");
    assert!(patch.contains("-b\n+B\n"), "{patch}");
    assert!(patch.ends_with('\n'), "patch 必须以换行收尾: {patch}");

    // structured_patch：裸补丁对象（无外层包装）。
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
    assert_eq!(resp["ok"], json!(true), "structured_patch 失败: {resp}");
    assert_eq!(resp["result"]["oldFileName"], json!("f.txt"));
    let hunks = resp["result"]["hunks"].as_array().expect("hunks");
    assert_eq!(hunks.len(), 1);
    let hunk = &hunks[0];
    // context=1：前置 context 仅 1 行（首行），oldStart = 2-1 = 1。
    assert_eq!(hunk["oldStart"], json!(1));
    assert_eq!(hunk["oldLines"], json!(3));
    assert_eq!(hunk["lines"], json!([" a", "-b", "+B", " c"]));

    // 无标签 → undefined 键省略（镜像 JSON.stringify）。
    let resp = client.request(3, "structuredPatch", json!({"old": "a\n", "new": "b\n"}));
    assert_eq!(resp["ok"], json!(true));
    assert!(
        resp["result"].get("oldFileName").is_none(),
        "无标签不得出现 oldFileName: {}",
        resp["result"]
    );

    client.send(json!({"v":1,"id":4,"method":"shutdown","params":{}}));
    assert_eq!(client.read_frame()["ok"], json!(true));
    assert_eq!(client.wait_exit(), 0);
}

#[test]
fn diff_lines_reports_chunks_with_explicit_booleans() {
    let mut client = spawn_ready();

    // 相同输入 → 单个公共 chunk，added/removed 显式 false。
    let resp = client.request(1, "diff.lines", json!({"old": "a\nb\n", "new": "a\nb\n"}));
    assert_eq!(resp["ok"], json!(true), "diff_lines 失败: {resp}");
    let chunks = resp["result"]["chunks"].as_array().expect("chunks");
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0]["count"], json!(2));
    assert_eq!(chunks[0]["added"], json!(false));
    assert_eq!(chunks[0]["removed"], json!(false));
    assert_eq!(chunks[0]["value"], json!("a\nb\n"));

    // 插入 → 公共 + 新增两段。
    let resp = client.request(2, "diff.lines", json!({"old": "a\n", "new": "a\nb\n"}));
    let chunks = resp["result"]["chunks"].as_array().expect("chunks");
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[1]["added"], json!(true));
    assert_eq!(chunks[1]["value"], json!("b\n"));

    // 空↔空 → 空数组。
    let resp = client.request(3, "diff.lines", json!({"old": "", "new": ""}));
    assert_eq!(resp["result"]["chunks"], json!([]));

    client.send(json!({"v":1,"id":4,"method":"shutdown","params":{}}));
    assert_eq!(client.read_frame()["ok"], json!(true));
    assert_eq!(client.wait_exit(), 0);
}

#[test]
fn apply_patch_success_failure_and_shape_errors() {
    let mut client = spawn_ready();

    // 先用 structured_patch 取 hunks，再原样回灌（真实组合链路）。
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
    assert_eq!(ok["ok"], json!(true), "apply 失败: {ok}");
    assert_eq!(
        ok["result"],
        json!({"applied": true, "result": "a\nB\nc\n"})
    );

    // 上下文全不匹配 → applied:false（不是协议错误）。
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
        "applied:false 仍是成功响应: {fail}"
    );
    assert_eq!(fail["result"], json!({"applied": false}));

    // 空 hunks → 原样返回 source。
    let empty = client.request(4, "diff.applyPatch", json!({"source": "raw", "hunks": []}));
    assert_eq!(empty["result"], json!({"applied": true, "result": "raw"}));

    client.send(json!({"v":1,"id":5,"method":"shutdown","params":{}}));
    assert_eq!(client.read_frame()["ok"], json!(true));
    assert_eq!(client.wait_exit(), 0);
}

#[test]
fn invalid_params_map_to_400_and_keep_serving() {
    let mut client = spawn_ready();

    // 缺 old → 400。
    assert_error_code(&client.request(1, "diff.unified", json!({"new": "x"})), 400);
    // old 类型不符（空串是合法输入，不报错）→ 非字符串才 400。
    assert_error_code(
        &client.request(2, "diff.unified", json!({"old": 7, "new": "x"})),
        400,
    );
    // context 负数 / 浮点 → 400。
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
    // fuzzFactor 非 0 → 400（仅支持精确应用）。
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
    // hunks 形状非法 → 400。
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

    // 所有 400 之后仍正常服务，最后优雅退出。
    let ok = client.request(8, "diff.lines", json!({"old": "a\n", "new": "b\n"}));
    assert_eq!(ok["ok"], json!(true), "错误后必须继续服务: {ok}");

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
    assert_eq!(resp["ok"], json!(true), "超时是业务 null 不是错误: {resp}");
    assert_eq!(resp["result"], json!({"patch": null}), "超时 → patch:null");

    let resp = client.request(
        2,
        "structuredPatch",
        json!({"old": old, "new": new, "timeoutMs": 0}),
    );
    assert_eq!(resp["ok"], json!(true), "超时是业务 null 不是错误: {resp}");
    assert_eq!(resp["result"], Value::Null, "超时 → 裸 null");

    client.send(json!({"v":1,"id":3,"method":"shutdown","params":{}}));
    assert_eq!(client.read_frame()["ok"], json!(true));
    assert_eq!(client.wait_exit(), 0, "超时请求后仍优雅退出");
}
