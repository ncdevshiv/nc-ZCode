//! 端到端（近 stdio 验收风格）：拉起真实 `zcode-server` 二进制，
//! 解析 `LISTENING <addr>` 行，用裸 TCP 发 HTTP/1.1 请求打真实 /healthz
//! 与 SPA 路由，最后 kill 进程。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Drop 时兜底 kill/wait，断言失败也不会留下孤儿进程。
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn temp_static_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("zcode-server-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp static dir");
    std::fs::write(
        dir.join("index.html"),
        "<html><body>e2e-spa-marker</body></html>",
    )
    .expect("write index.html");
    dir
}

/// 启动真实二进制并等待 LISTENING 行，返回 (guard, port)。
fn spawn_server(static_dir: &Path) -> (Guard, u16) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_zcode-server"))
        .args(["--host", "127.0.0.1", "--port", "0", "--static-dir"])
        .arg(static_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn zcode-server");
    let stdout = child.stdout.take().expect("piped stdout");

    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut line = String::new();
        let mut reader = BufReader::new(stdout);
        if reader.read_line(&mut line).is_ok() {
            let _ = tx.send(line);
        }
    });
    // 超时兜底：防止 bind 失败时测试永久挂起；错误信息带上 stderr 诊断。
    let line = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("timeout waiting for LISTENING line");
    let addr = line
        .trim()
        .strip_prefix("LISTENING ")
        .unwrap_or_else(|| panic!("first stdout line must be LISTENING, got: {line}"));
    let port: u16 = addr
        .rsplit(':')
        .next()
        .expect("addr has port")
        .parse()
        .expect("port number");
    (Guard(child), port)
}

/// 裸 TCP 发一个 HTTP/1.1 GET（Connection: close），返回完整响应文本。
fn http_get(port: u16, path: &str) -> String {
    let stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    let mut stream = stream;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .expect("write request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    response
}

#[test]
fn real_bin_serves_healthz_and_spa_over_tcp() {
    let dir = temp_static_dir();
    let (_guard, port) = spawn_server(&dir);

    let health = http_get(port, "/healthz");
    assert!(
        health.starts_with("HTTP/1.1 200"),
        "healthz must be 200, got: {health}"
    );
    assert!(
        health.contains("\"status\":\"ok\""),
        "healthz body missing status ok: {health}"
    );

    let spa = http_get(port, "/workspace/x");
    assert!(
        spa.starts_with("HTTP/1.1 200"),
        "SPA route must be 200, got: {spa}"
    );
    assert!(
        spa.contains("e2e-spa-marker"),
        "SPA route must serve index.html: {spa}"
    );

    let api404 = http_get(port, "/api/nope");
    assert!(
        api404.starts_with("HTTP/1.1 404"),
        "unknown /api must be 404, got: {api404}"
    );
    assert!(
        api404.contains("application/json"),
        "unknown /api must be JSON: {api404}"
    );

    std::fs::remove_dir_all(&dir).ok();
}
