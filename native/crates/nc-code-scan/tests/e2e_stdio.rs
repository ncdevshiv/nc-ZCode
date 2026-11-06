//! e2e：spawn 真实 `nc-code-scan` 二进制，通过 stdio 验证协议与扫描事件流。
//!
//! 覆盖（spec S1 验收 + 任务清单）：ready 首帧、嵌套/Unicode 树全量扫描的条目
//! 与汇总、scanId 终态后复用、maxDepth 限制、重复 scanId → 409、遍历中 cancel
//! → 部分完成、shutdown 期间在途扫描仍收到终态事件、根为文件 →
//! scan_error(ENOTDIR)、根缺失 → missingRoot、未知 cancel no-op、参数 400 /
//! 未知方法 404、malformed → proto_error、版本不匹配 → exit 1、EOF → exit 0。

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const READY_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
/// 扫描终态事件的上界（spec 事件断言 10s）。
const SCAN_TIMEOUT: Duration = Duration::from_secs(10);
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(20);

struct Sidecar {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<String>,
}

impl Sidecar {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_zcode-scan"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn nc-code-scan");
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

    /// 多帧一次写出：serve 首次读入即拿到整批（扫描类时序测试依赖此确定性）。
    fn send_batch(&mut self, frames: &[Value]) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        for frame in frames {
            stdin
                .write_all(frame.to_string().as_bytes())
                .expect("write frame");
            stdin.write_all(b"\n").expect("write newline");
        }
        stdin.flush().expect("flush batch");
    }

    fn next_value(&self, timeout: Duration) -> Value {
        match self.rx.recv_timeout(timeout) {
            Ok(line) => serde_json::from_str(&line)
                .unwrap_or_else(|err| panic!("frame is not json: {line}: {err}")),
            Err(err) => panic!("no frame within {timeout:?}: {err}"),
        }
    }

    /// 持续读帧（保序、不丢帧）直到整帧集合满足谓词；整体受 timeout 约束。
    fn collect_until(&self, timeout: Duration, want: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + timeout;
        let mut frames: Vec<Value> = Vec::new();
        loop {
            if want(&frames) {
                return frames;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for frames; got {} frames",
                frames.len()
            );
            frames.push(self.next_value(remaining));
        }
    }

    fn wait_response(&self, id: u64, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "no response for id {id}");
            let frame = self.next_value(remaining);
            if is_response(&frame, id) {
                return frame;
            }
        }
    }

    fn wait_event(&self, event: &str, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "no {event} event");
            let frame = self.next_value(remaining);
            if frame.get("event").is_some_and(|name| *name == json!(event)) {
                return frame;
            }
        }
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

fn is_response(frame: &Value, id: u64) -> bool {
    frame.get("id").is_some_and(|got| *got == json!(id)) && frame.get("ok").is_some()
}

fn response(frames: &[Value], id: u64) -> Option<&Value> {
    frames.iter().find(|frame| is_response(frame, id))
}

fn events<'a>(frames: &'a [Value], name: &str) -> Vec<&'a Value> {
    frames
        .iter()
        .filter(|frame| {
            frame
                .get("event")
                .is_some_and(|event| *event == json!(name))
        })
        .collect()
}

fn scan_params(path: &Path, scan_id: &str) -> Value {
    json!({ "path": path, "scanId": scan_id })
}

/// 生成 `dirs × per` 个小文件，返回文件总数（大树让遍历足够慢，使「遍历中
/// 取消」的时序断言在实践上确定：cancel 帧是缓冲读入的 µs 级，遍历是 ms 级）。
fn build_many_files(root: &Path, dirs: usize, per: usize) -> usize {
    for dir_index in 0..dirs {
        let dir = root.join(format!("d{dir_index:02}"));
        std::fs::create_dir(&dir).expect("mkdir");
        for file_index in 0..per {
            std::fs::write(dir.join(format!("f{file_index:03}.txt")), b"x").expect("write");
        }
    }
    dirs * per
}

#[test]
fn ready_is_first_frame_then_shutdown_exits_zero() {
    let mut sidecar = Sidecar::spawn();
    let ready = sidecar.next_value(READY_TIMEOUT);
    assert_eq!(
        ready,
        json!({"v":1,"event":"ready","data":{"sidecar":"zcode-scan","proto":1}}),
        "首帧必须是 ready 事件"
    );

    sidecar.send(&json!({"v":1,"id":1,"method":"shutdown","params":{}}));
    let response = sidecar.wait_response(1, RESPONSE_TIMEOUT);
    assert_eq!(response["ok"], json!(true), "shutdown 必须回成功响应");
    assert_eq!(sidecar.wait_exit(EXIT_TIMEOUT), 0);
}

#[test]
fn scan_nested_unicode_tree_streams_entries_then_done_and_allows_id_reuse() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::create_dir_all(root.join("a").join("b")).expect("mkdir");
    std::fs::create_dir_all(root.join("e f")).expect("mkdir");
    std::fs::write(root.join("a").join("b").join("c.txt"), b"ccc").expect("write");
    std::fs::write(root.join("文件 名.txt"), b"unicode").expect("write");
    std::fs::write(root.join("e f").join("g.txt"), b"gg").expect("write");

    let mut sidecar = Sidecar::spawn();
    sidecar.next_value(READY_TIMEOUT);

    sidecar.send(&json!({"v":1,"id":1,"method":"scan","params":scan_params(root, "u1")}));
    let frames = sidecar.collect_until(SCAN_TIMEOUT, |frames| {
        response(frames, 1).is_some() && !events(frames, "scan_done").is_empty()
    });

    let resp = response(&frames, 1).expect("scan response");
    assert_eq!(resp["ok"], json!(true), "scan 失败: {resp}");
    assert_eq!(resp["result"], json!({}));

    assert!(
        events(&frames, "scan_error").is_empty(),
        "正常扫描不得有 scan_error"
    );

    let entries = events(&frames, "scan_entry");
    assert_eq!(entries.len(), 3, "每个文件恰好一条 scan_entry");
    let mut paths: Vec<&str> = entries
        .iter()
        .map(|frame| {
            frame["data"]["entry"]["relativePath"]
                .as_str()
                .expect("relativePath")
        })
        .collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        vec!["a/b/c.txt", "e f/g.txt", "文件 名.txt"],
        "根相对路径必须 / 分隔且保留 Unicode/空格（镜像 TS toRelative）"
    );

    // bytes 精确对账：b"ccc"=3、b"unicode"=7、b"gg"=2。
    let mut sizes: Vec<(&str, u64)> = entries
        .iter()
        .map(|frame| {
            (
                frame["data"]["entry"]["relativePath"]
                    .as_str()
                    .expect("rel"),
                frame["data"]["entry"]["bytes"].as_u64().expect("bytes"),
            )
        })
        .collect();
    sizes.sort_unstable();
    assert_eq!(
        sizes,
        vec![("a/b/c.txt", 3), ("e f/g.txt", 2), ("文件 名.txt", 7)],
        "bytes 必须等于文件实际字节数"
    );
    for entry in &entries {
        assert!(
            entry["data"]["entry"]["mtimeMs"].as_u64().expect("mtimeMs") > 1_600_000_000_000,
            "mtimeMs 必须是 epoch 毫秒"
        );
    }

    let done = events(&frames, "scan_done");
    assert_eq!(done.len(), 1, "终态事件恰好一条");
    let data = &done[0]["data"];
    assert_eq!(data["scanId"], json!("u1"));
    assert_eq!(data["filesScanned"], json!(3));
    assert_eq!(
        data["directoriesScanned"],
        json!(4),
        "root + a + a/b + 'e f'"
    );
    assert_eq!(data["missingRoot"], json!(false));
    assert_eq!(data["cancelled"], json!(false));

    // 收到终态事件后复用 scanId 必然成功（释放先于 scan_done 发出）。
    sidecar.send(&json!({"v":1,"id":2,"method":"scan","params":scan_params(root, "u1")}));
    let reuse = sidecar.collect_until(SCAN_TIMEOUT, |frames| response(frames, 2).is_some());
    let resp = response(&reuse, 2).expect("reuse response");
    assert_eq!(
        resp["ok"],
        json!(true),
        "收到 done 后复用 scanId 必须成功: {resp}"
    );

    sidecar.send(&json!({"v":1,"id":3,"method":"shutdown","params":{}}));
    let resp = sidecar.wait_response(3, RESPONSE_TIMEOUT);
    assert_eq!(resp["ok"], json!(true));
    assert_eq!(sidecar.wait_exit(EXIT_TIMEOUT), 0);
}

#[test]
fn max_depth_limits_descent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::create_dir_all(root.join("a").join("b")).expect("mkdir");
    std::fs::write(root.join("f.txt"), b"1").expect("write");
    std::fs::write(root.join("a").join("f2.txt"), b"22").expect("write");
    std::fs::write(root.join("a").join("b").join("f3.txt"), b"333").expect("write");

    let mut sidecar = Sidecar::spawn();
    sidecar.next_value(READY_TIMEOUT);

    sidecar.send(&json!({
        "v": 1, "id": 1, "method": "scan",
        "params": {"path": root, "scanId": "depth", "maxDepth": 1}
    }));
    let frames = sidecar.collect_until(SCAN_TIMEOUT, |frames| {
        response(frames, 1).is_some() && !events(frames, "scan_done").is_empty()
    });

    let resp = response(&frames, 1).expect("scan response");
    assert_eq!(resp["ok"], json!(true), "scan 失败: {resp}");
    assert!(events(&frames, "scan_error").is_empty());

    let entries = events(&frames, "scan_entry");
    let mut paths: Vec<&str> = entries
        .iter()
        .map(|frame| {
            frame["data"]["entry"]["relativePath"]
                .as_str()
                .expect("relativePath")
        })
        .collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        vec!["a/f2.txt", "f.txt"],
        "maxDepth=1 只扫根 + 一级子目录"
    );

    let done = events(&frames, "scan_done");
    assert_eq!(done[0]["data"]["filesScanned"], json!(2));
    assert_eq!(done[0]["data"]["directoriesScanned"], json!(2), "root + a");

    sidecar.send(&json!({"v":1,"id":2,"method":"shutdown","params":{}}));
    let resp = sidecar.wait_response(2, RESPONSE_TIMEOUT);
    assert_eq!(resp["ok"], json!(true));
    assert_eq!(sidecar.wait_exit(EXIT_TIMEOUT), 0);
}

#[test]
fn duplicate_scan_id_conflicts_and_cancel_stops_walk_with_partials() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let total = build_many_files(tmp.path(), 40, 75);

    let mut sidecar = Sidecar::spawn();
    sidecar.next_value(READY_TIMEOUT);

    // 扫描 + 同 id 重复扫描一次写出：重复请求必在线程完成前到达 → 409。
    sidecar.send_batch(&[
        json!({"v":1,"id":1,"method":"scan","params":scan_params(tmp.path(), "big")}),
        json!({"v":1,"id":2,"method":"scan","params":scan_params(tmp.path(), "big")}),
    ]);
    let started = sidecar.collect_until(SCAN_TIMEOUT, |frames| {
        response(frames, 1).is_some()
            && response(frames, 2).is_some()
            && events(frames, "scan_entry").len() >= 5
    });
    let resp = response(&started, 1).expect("scan response");
    assert_eq!(resp["ok"], json!(true), "scan 失败: {resp}");
    let dup = response(&started, 2).expect("duplicate response");
    assert_eq!(dup["ok"], json!(false), "重复 scanId 必须冲突: {dup}");
    assert_eq!(dup["error"]["code"], json!(409));

    // 已收到首批条目（遍历进行中）→ 取消。
    sidecar.send(&json!({"v":1,"id":3,"method":"cancel","params":{"scanId":"big"}}));
    let mut frames = started;
    let mut rest = sidecar.collect_until(SCAN_TIMEOUT, |frames| {
        response(frames, 3).is_some() && !events(frames, "scan_done").is_empty()
    });
    frames.append(&mut rest);

    let cancel = response(&frames, 3).expect("cancel response");
    assert_eq!(cancel["ok"], json!(true), "cancel 失败: {cancel}");
    assert_eq!(cancel["result"], json!({}));

    assert!(
        events(&frames, "scan_error").is_empty(),
        "取消不是错误，不得产生 scan_error"
    );

    let done = events(&frames, "scan_done");
    assert_eq!(done.len(), 1, "必须恰好一条终态事件");
    let data = &done[0]["data"];
    assert_eq!(data["scanId"], json!("big"));
    assert_eq!(
        data["cancelled"],
        json!(true),
        "取消后终态必须标记 cancelled"
    );
    assert_eq!(data["missingRoot"], json!(false));
    let files = data["filesScanned"].as_u64().expect("filesScanned");
    assert!(
        files > 0 && files < total as u64,
        "部分完成：filesScanned={files} 应在 (0, {total}) 内"
    );
    assert!(
        data["directoriesScanned"]
            .as_u64()
            .expect("directoriesScanned")
            >= 1
    );
    let entry_count = events(&frames, "scan_entry").len() as u64;
    assert_eq!(
        entry_count, files,
        "每条 scan_entry 与 filesScanned 计数一一对应"
    );

    // stdin EOF → 优雅退出（扫描已终态，join 立即返回）。
    drop(sidecar.stdin.take());
    assert_eq!(sidecar.wait_exit(EXIT_TIMEOUT), 0);
}

#[test]
fn shutdown_with_inflight_scan_delivers_terminal_event_then_exits_zero() {
    let tmp = tempfile::tempdir().expect("tempdir");
    build_many_files(tmp.path(), 2, 100);

    let mut sidecar = Sidecar::spawn();
    sidecar.next_value(READY_TIMEOUT);

    // 扫描 + shutdown 一次写出：serve 处理 shutdown 时扫描仍在进行。
    sidecar.send_batch(&[
        json!({"v":1,"id":1,"method":"scan","params":scan_params(tmp.path(), "exit")}),
        json!({"v":1,"id":9,"method":"shutdown","params":{}}),
    ]);
    let frames = sidecar.collect_until(SCAN_TIMEOUT, |frames| {
        response(frames, 1).is_some()
            && response(frames, 9).is_some()
            && !events(frames, "scan_done").is_empty()
    });

    let resp = response(&frames, 1).expect("scan response");
    assert_eq!(resp["ok"], json!(true), "scan 失败: {resp}");
    let resp = response(&frames, 9).expect("shutdown response");
    assert_eq!(resp["ok"], json!(true), "shutdown 必须回成功响应");
    let done = events(&frames, "scan_done");
    assert_eq!(
        done[0]["data"]["scanId"],
        json!("exit"),
        "退出前必须收到在途扫描的终态事件"
    );

    assert_eq!(sidecar.wait_exit(EXIT_TIMEOUT), 0, "join 后必须 exit 0");
}

#[test]
fn root_file_reports_enotdir_and_missing_root_is_empty_root() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let file_root = tmp.path().join("plain.txt");
    std::fs::write(&file_root, b"x").expect("write");

    let mut sidecar = Sidecar::spawn();
    sidecar.next_value(READY_TIMEOUT);

    // 根为文件：镜像 TS onError({path:"", code:"ENOTDIR"}) → scan_error + 空完成。
    sidecar.send(&json!({"v":1,"id":1,"method":"scan","params":scan_params(&file_root, "f")}));
    let frames = sidecar.collect_until(SCAN_TIMEOUT, |frames| {
        response(frames, 1).is_some() && !events(frames, "scan_done").is_empty()
    });
    let resp = response(&frames, 1).expect("scan response");
    assert_eq!(resp["ok"], json!(true), "根为文件不是请求级错误: {resp}");
    let errors = events(&frames, "scan_error");
    assert_eq!(errors.len(), 1, "必须上报一条根级错误: {errors:?}");
    assert_eq!(errors[0]["data"]["path"], json!(""));
    assert_eq!(errors[0]["data"]["code"], json!("ENOTDIR"));
    assert!(events(&frames, "scan_entry").is_empty());
    let done = events(&frames, "scan_done");
    assert_eq!(done[0]["data"]["filesScanned"], json!(0));
    assert_eq!(done[0]["data"]["directoriesScanned"], json!(0));
    assert_eq!(
        done[0]["data"]["missingRoot"],
        json!(false),
        "ENOTDIR ≠ ENOENT"
    );
    assert_eq!(done[0]["data"]["cancelled"], json!(false));

    // 根缺失：视为空根，非错误（镜像 TS missingRoot，不产生 scan_error）。
    let missing = tmp.path().join("no-such-root-xyz");
    sidecar.send(&json!({"v":1,"id":2,"method":"scan","params":scan_params(&missing, "m")}));
    let frames = sidecar.collect_until(SCAN_TIMEOUT, |frames| {
        response(frames, 2).is_some() && !events(frames, "scan_done").is_empty()
    });
    let resp = response(&frames, 2).expect("scan response");
    assert_eq!(resp["ok"], json!(true));
    assert!(
        events(&frames, "scan_error").is_empty(),
        "missingRoot 不是错误，不得产生 scan_error"
    );
    let done = events(&frames, "scan_done");
    assert_eq!(done[0]["data"]["missingRoot"], json!(true));
    assert_eq!(done[0]["data"]["filesScanned"], json!(0));
    assert_eq!(done[0]["data"]["cancelled"], json!(false));

    sidecar.send(&json!({"v":1,"id":3,"method":"shutdown","params":{}}));
    let resp = sidecar.wait_response(3, RESPONSE_TIMEOUT);
    assert_eq!(resp["ok"], json!(true));
    assert_eq!(sidecar.wait_exit(EXIT_TIMEOUT), 0);
}

#[test]
fn error_paths_400_404_noop_cancel_and_proto_error_then_eof_exits_zero() {
    let mut sidecar = Sidecar::spawn();
    sidecar.next_value(READY_TIMEOUT);

    // malformed 行 → proto_error，服务不中断（spec 规则 2）。
    sidecar.send_line("garbage! not json");
    let frame = sidecar.wait_event("proto_error", RESPONSE_TIMEOUT);
    assert_eq!(frame["data"]["raw"], json!("garbage! not json"));
    assert!(frame["data"]["reason"]
        .as_str()
        .expect("reason")
        .starts_with("invalid json"));

    sidecar.send_batch(&[
        json!({"v":1,"id":1,"method":"scan","params":{"path":"x"}}),
        json!({"v":1,"id":2,"method":"warp","params":{}}),
        json!({"v":1,"id":3,"method":"cancel","params":{"scanId":"ghost"}}),
        json!({"v":1,"id":4,"method":"cancel","params":{}}),
    ]);
    let frames = sidecar.collect_until(RESPONSE_TIMEOUT, |frames| {
        (1u64..=4).all(|id| response(frames, id).is_some())
    });

    // 缺 scanId → 400。
    let resp = response(&frames, 1).expect("response 1");
    assert_eq!(resp["ok"], json!(false));
    assert_eq!(resp["error"]["code"], json!(400));
    assert!(resp["error"]["message"]
        .as_str()
        .expect("message")
        .contains("scanId"));
    // 未知方法 → 404。
    let resp = response(&frames, 2).expect("response 2");
    assert_eq!(resp["error"]["code"], json!(404));
    // 未知 scanId 的 cancel → no-op 成功（镜像 TS 非当前 job）。
    let resp = response(&frames, 3).expect("response 3");
    assert_eq!(resp["ok"], json!(true));
    assert_eq!(resp["result"], json!({}));
    // cancel 缺参数 → 400。
    let resp = response(&frames, 4).expect("response 4");
    assert_eq!(resp["error"]["code"], json!(400));

    // stdin EOF → 优雅退出 0。
    drop(sidecar.stdin.take());
    assert_eq!(sidecar.wait_exit(EXIT_TIMEOUT), 0);
}

#[test]
fn version_mismatch_returns_400_then_exits_one() {
    let mut sidecar = Sidecar::spawn();
    sidecar.next_value(READY_TIMEOUT);

    sidecar.send(&json!({"v":2,"id":9,"method":"scan","params":{}}));
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
