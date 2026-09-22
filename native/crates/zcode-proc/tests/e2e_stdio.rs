//! e2e：真实 `zcode-proc` 二进制走完整 sidecar 协议 —— ready / 版本 fail-closed /
//! proto_error 恢复 / 404 / 参数 400 / shutdown 退出码 / 真实进程树杀灭 / job 生命周期。

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use zcode_sidecar_proto::VERSION_MISMATCH_PREFIX;

/// 两平台 pid 空间（Windows 22 位 / Linux pid_max≤4194304）之外的“必然不存在” pid。
const MISSING_PID: u64 = 2_147_483_647;

/// 协议客户端：单问单答（sidecar 单线程 serve，顺序确定）。
struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Client {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_zcode-proc"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // 诊断走 stderr（eprintln）；e2e 以 stdout 帧为准，null 避免管道占满。
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn zcode-proc");
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

    /// 关闭 stdin：触发 sidecar 的 EOF 优雅退出路径。
    fn close_stdin(&mut self) {
        drop(self.stdin.take());
    }

    fn send(&mut self, frame: Value) {
        let line = frame.to_string();
        self.send_raw(&line);
    }

    /// 发请求并读回对应响应（校验 id 回显）。
    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"v":1,"id":id,"method":method,"params":params}));
        let frame = self.read_frame();
        assert_eq!(frame["id"], json!(id), "响应回显 id");
        frame
    }

    fn shutdown(&mut self) {
        self.send(json!({"v":1,"id":9_999,"method":"shutdown","params":{}}));
        let frame = self.read_frame();
        assert_eq!(frame["ok"], json!(true), "shutdown 必须成功响应: {frame}");
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

/// 启动并断言第一帧是规范的 `ready`。
fn spawn_ready() -> Client {
    let mut client = Client::spawn();
    let ready = client.read_frame();
    assert_eq!(ready["event"], json!("ready"), "首帧必须是 ready: {ready}");
    assert_eq!(ready["data"]["sidecar"], json!("zcode-proc"));
    assert_eq!(ready["data"]["proto"], json!(1));
    client
}

/// 轮询直到目标 pid 从进程表消失（返回是否已死）。
fn wait_dead(client: &mut Client, pid: u64, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        let resp = client.request(90, "list_tree", json!({"pid": pid}));
        let alive = resp["result"]["processes"]
            .as_array()
            .map(|procs| !procs.is_empty())
            .unwrap_or(true);
        if !alive {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn ready_then_version_mismatch_fails_closed_with_exit_1() {
    let mut client = spawn_ready();
    client.send(json!({"v":2,"id":9,"method":"list_tree","params":{"pid":1}}));
    let frame = client.read_frame();
    assert_eq!(frame["ok"], json!(false));
    assert_eq!(frame["error"]["code"], json!(400));
    let message = frame["error"]["message"].as_str().expect("message");
    assert!(
        message.starts_with(VERSION_MISMATCH_PREFIX),
        "消息须带版本前缀: {message}"
    );
    assert_eq!(client.wait_exit(), 1, "版本不匹配必须 fail closed");
}

#[test]
fn malformed_frame_recovers_404_400_then_shutdown_exits_zero() {
    let mut client = spawn_ready();

    // 非 JSON 行 → proto_error 事件，循环不断。
    client.send_raw("this is not json");
    let event = client.read_frame();
    assert_eq!(event["event"], json!("proto_error"));
    assert_eq!(event["data"]["raw"], json!("this is not json"));

    // 未知方法 → 404，继续服务。
    let resp = client.request(1, "no_such_method", json!({}));
    assert_eq!(resp["error"]["code"], json!(404));

    // 真实枚举：测试进程自身必须出现在闭包内，且根优先、name 为字符串。
    let me = u64::from(std::process::id());
    let resp = client.request(2, "list_tree", json!({"pid": me}));
    assert_eq!(resp["ok"], json!(true), "list_tree 失败: {resp}");
    let procs = resp["result"]["processes"].as_array().expect("array");
    assert!(!procs.is_empty(), "自身 pid 必须可见");
    assert_eq!(procs[0]["pid"], json!(me), "首元素是根（BFS 根优先）");
    assert!(procs[0]["name"].is_string());

    // 参数非法 → 400。
    let resp = client.request(3, "list_tree", json!({"pid":"not-a-number"}));
    assert_eq!(resp["error"]["code"], json!(400));

    client.shutdown();
    assert_eq!(client.wait_exit(), 0, "shutdown → exit 0");
}

#[test]
fn eof_without_shutdown_is_graceful_exit_0() {
    let mut client = spawn_ready();
    client.close_stdin();
    assert_eq!(client.wait_exit(), 0, "stdin EOF 同样是优雅退出");
}

#[test]
fn kill_tree_and_list_tree_on_missing_pid_are_noop() {
    let mut client = spawn_ready();
    let resp = client.request(1, "kill_tree", json!({"pid": MISSING_PID, "timeoutMs": 50}));
    assert_eq!(resp["ok"], json!(true), "已死根必须是无操作成功: {resp}");
    assert_eq!(resp["result"]["killed"], json!([]));
    assert_eq!(resp["result"]["failed"], json!([]));

    let resp = client.request(2, "list_tree", json!({"pid": MISSING_PID}));
    assert_eq!(resp["result"]["processes"], json!([]));
}

/// Windows 真实进程树：`start /WAIT` 链出 ≥2 层，`kill_tree` 全灭。
#[cfg(windows)]
#[test]
fn kill_tree_kills_real_windows_process_tree() {
    // 外层 cmd 等待内层 cmd，内层跑长驻 ping → 树至少 outer/cmd/ping 三层。
    let mut child = Command::new("cmd")
        .args([
            "/C",
            "start",
            "/WAIT",
            "cmd",
            "/C",
            "ping -n 300 127.0.0.1 >nul",
        ])
        .spawn()
        .expect("spawn tree");
    let root = u64::from(child.id());
    let mut client = spawn_ready();

    // start 异步拉起内层，轮询到树成型（预算 3s）。
    let deadline = Instant::now() + Duration::from_secs(3);
    let procs: Vec<Value> = loop {
        let resp = client.request(1, "list_tree", json!({"pid": root}));
        let procs = resp["result"]["processes"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if procs.len() >= 2 || Instant::now() >= deadline {
            break procs;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(procs.len() >= 2, "期望 ≥2 层进程树，实际: {procs:?}");
    assert_eq!(procs[0]["pid"], json!(root), "首元素必须是根");

    let first_round = client.request(2, "kill_tree", json!({"pid": root, "timeoutMs": 10000}));
    assert_eq!(
        first_round["ok"],
        json!(true),
        "kill_tree 失败: {first_round}"
    );
    assert_eq!(
        first_round["result"]["failed"],
        json!([]),
        "10s 内必须杀干净"
    );
    let killed = first_round["result"]["killed"].as_array().expect("killed");
    assert!(killed.len() >= 2, "killed 必须覆盖整棵树: {killed:?}");

    // 根已死 → 二轮为无操作成功。断言消息携带一轮完整结果与根 pid：
    // 偶发失败时可直接对比两轮 killed 集合，判定「一轮误报已死」还是「根后新生」。
    let resp = client.request(3, "kill_tree", json!({"pid": root, "timeoutMs": 50}));
    assert_eq!(
        resp["result"]["killed"],
        json!([]),
        "二轮必须无操作；一轮={first_round} root={root}"
    );
    assert_eq!(
        resp["result"]["failed"],
        json!([]),
        "二轮必须无失败；一轮={first_round} root={root}"
    );

    client.shutdown();
    assert_eq!(client.wait_exit(), 0);
    let _ = child.wait(); // 树已被杀：回收句柄即可
}

/// Windows：spawn_in_job → 可见 → kill_job 全灭 → 重复/未知 key false → 400 路径。
#[cfg(windows)]
#[test]
fn spawn_in_job_then_kill_job_lifecycle_on_windows() {
    let mut client = spawn_ready();
    let resp = client.request(
        1,
        "spawn_in_job",
        json!({
            "cmd": "ping",
            "args": ["-n", "300", "127.0.0.1"],
            "env": {"ZCODE_E2E": "1"},
        }),
    );
    assert_eq!(resp["ok"], json!(true), "spawn_in_job 失败: {resp}");
    let pid = resp["result"]["pid"].as_u64().expect("pid");
    let job_key = resp["result"]["jobKey"]
        .as_str()
        .expect("jobKey")
        .to_string();
    assert!(job_key.starts_with("job-") && job_key.ends_with(&format!("-{pid}")));

    let resp = client.request(2, "list_tree", json!({"pid": pid}));
    let procs = resp["result"]["processes"].as_array().expect("array");
    assert_eq!(procs[0]["pid"], json!(pid), "新进程必须在进程表");

    let resp = client.request(3, "kill_job", json!({"jobKey": job_key}));
    assert_eq!(resp["result"]["terminated"], json!(true));
    assert!(
        wait_dead(&mut client, pid, Duration::from_secs(5)),
        "kill_job 后子进程必须退出"
    );

    // 幂等：第二次 / 未知 key → false。
    let resp = client.request(4, "kill_job", json!({"jobKey": job_key}));
    assert_eq!(resp["result"]["terminated"], json!(false));
    let resp = client.request(5, "kill_job", json!({"jobKey": "job-0-0"}));
    assert_eq!(resp["result"]["terminated"], json!(false));

    // 缺 cmd → 400；cmd 不存在 → CreateProcess 失败 → 400（未产生子进程）。
    let resp = client.request(6, "spawn_in_job", json!({"args": []}));
    assert_eq!(resp["error"]["code"], json!(400));
    let resp = client.request(
        7,
        "spawn_in_job",
        json!({"cmd": "zcode-no-such-binary-e2e"}),
    );
    assert_eq!(
        resp["error"]["code"],
        json!(400),
        "spawn 失败须 400: {resp}"
    );

    client.shutdown();
    assert_eq!(client.wait_exit(), 0);
}

/// unix（Linux/macOS 运行时）：setsid 组 spawn → kill_job 组杀。
#[cfg(unix)]
#[test]
fn spawn_in_job_then_kill_job_lifecycle_on_unix() {
    let mut client = spawn_ready();
    let resp = client.request(1, "spawn_in_job", json!({"cmd": "sleep", "args": ["300"]}));
    assert_eq!(resp["ok"], json!(true), "spawn_in_job 失败: {resp}");
    let pid = resp["result"]["pid"].as_u64().expect("pid");
    let job_key = resp["result"]["jobKey"]
        .as_str()
        .expect("jobKey")
        .to_string();

    let resp = client.request(2, "list_tree", json!({"pid": pid}));
    let procs = resp["result"]["processes"].as_array().expect("array");
    assert_eq!(procs[0]["pid"], json!(pid));

    let resp = client.request(3, "kill_job", json!({"jobKey": job_key}));
    assert_eq!(resp["result"]["terminated"], json!(true));
    assert!(wait_dead(&mut client, pid, Duration::from_secs(5)));

    let resp = client.request(4, "kill_job", json!({"jobKey": job_key}));
    assert_eq!(resp["result"]["terminated"], json!(false));

    let resp = client.request(
        5,
        "spawn_in_job",
        json!({"cmd": "zcode-no-such-binary-e2e"}),
    );
    assert_eq!(resp["error"]["code"], json!(400));

    client.shutdown();
    assert_eq!(client.wait_exit(), 0);
}
