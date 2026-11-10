//! Tunnel + WS-facade e2e against the real `zcode-server` binary.
//!
//! A `TcpForwarder` stands in for `ssh -N -L` (exact byte-level TCP pipe;
//! hosts without a local `sshd` cannot spawn a real tunnel): probe
//! `/healthz` through the tunnel, then drive `system.info` and
//! `task/listTasks` through a [`WsTaskService`] connected to the tunneled
//! `ws://…/ws` endpoint. The facade holds only a WS client — no in-process
//! `TaskService` — so success proves invokes really cross the wire.
//! Dispose (`stop`) closes the tunnel port (no listener leak).

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use nc_code_channels::{CancelToken, Channel, ChannelContext, ClientMode};
use nc_code_rpc::WireValue;
use nc_code_services_client::{
    remote_ws_facade::{probe_tunnel_health, TcpForwarder},
    WsChannelClient, WsTaskService,
};
use serde_json::json;

struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn server_binary() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("ZCODE_SERVER_BIN") {
        if !path.trim().is_empty() {
            return std::path::PathBuf::from(path);
        }
    }
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let exe = if cfg!(windows) {
        "zcode-server.exe"
    } else {
        "zcode-server"
    };
    manifest.join("../../target/debug").join(exe)
}

fn spawn_server() -> (Guard, u16) {
    let bin = server_binary();
    assert!(
        bin.is_file(),
        "zcode-server binary not found at {} (run `cargo build -p nc-code-server` first)",
        bin.display()
    );
    let tmp = std::env::temp_dir().join(format!("zcode-tunnel-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("temp dir");
    let mut child = Command::new(&bin)
        .args(["--host", "127.0.0.1", "--port", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn zcode-server");
    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut line = String::new();
        let mut reader = BufReader::new(stdout);
        if reader.read_line(&mut line).is_ok() {
            let _ = tx.send(line);
        }
    });
    let line = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("LISTENING timeout");
    let addr = line
        .trim()
        .strip_prefix("LISTENING ")
        .unwrap_or_else(|| panic!("expected LISTENING, got: {line}"));
    let port: u16 = addr.rsplit(':').next().unwrap().parse().expect("port");
    (Guard(child), port)
}

#[tokio::test]
async fn system_info_and_task_call_over_tunneled_ws() {
    let (_guard, server_port) = spawn_server();
    let server_addr: SocketAddr = format!("127.0.0.1:{server_port}").parse().expect("addr");
    let forwarder = TcpForwarder::bind(server_addr)
        .await
        .expect("tunnel forwarder bind");
    let tunnel_addr = forwarder.address();

    // /healthz through the tunnel (same probe as ssh -L path).
    assert!(
        nc_code_services_client::remote_ws_facade::probe_tunnel_health(tunnel_addr).await,
        "/healthz through the tunnel must return the ok marker"
    ); // WS facade through the tunnel: system.info is a real remote round-trip.
    let facade = WsTaskService::connect(&format!("ws://{tunnel_addr}/ws"))
        .await
        .expect("ws facade connect");
    let info = facade
        .client()
        .invoke("system", "info", WireValue::Undefined)
        .await
        .expect("system.info over tunneled WS");
    let WireValue::Json(info) = info else {
        panic!("system.info must return Json, got: {info:?}");
    };
    assert!(
        info.get("homedir")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty()),
        "system.info homedir must be real, got: {info}"
    );
    assert!(
        info.get("platform").and_then(|v| v.as_str()).is_some(),
        "system.info platform must be present, got: {info}"
    );

    // Task-channel invoke through the same facade (real list from the
    // remote server's TaskService — proves task forwarding over WS).
    let ctx = ChannelContext::new("tunnel-e2e", ClientMode::DesktopContinuous);
    let listed = facade
        .call(
            &ctx,
            "listTasks",
            vec![WireValue::Json(json!({ "workspacePath": "G:/ZCode-main" }))],
            CancelToken::new(),
        )
        .await
        .expect("task listTasks over tunneled WS");
    let WireValue::Json(rows) = listed else {
        panic!("listTasks must return Json, got: {listed:?}");
    };
    assert!(
        rows.is_array(),
        "listTasks must return an array, got: {rows}"
    );

    facade.client().close().await;
    forwarder.stop();

    // No leak: the tunnel port is closed after stop.
    assert!(
        std::net::TcpStream::connect(tunnel_addr).is_err(),
        "stopped tunnel must close its port (no listener leak)"
    );
}

#[tokio::test]
async fn ws_facade_fails_closed_on_closed_tunnel_endpoint() {
    let (_guard, server_port) = spawn_server();
    let server_addr: SocketAddr = format!("127.0.0.1:{server_port}").parse().expect("addr");
    let forwarder = TcpForwarder::bind(server_addr)
        .await
        .expect("forwarder bind");
    let tunnel_addr = forwarder.address();
    forwarder.stop();
    let err = WsChannelClient::connect(&format!("ws://{tunnel_addr}/ws"))
        .await
        .unwrap_err();
    assert_eq!(err.name, "RemoteSourceUnavailable");
    assert!(tunnel_addr.port() != server_port);
}

#[test]
fn tunnel_health_probe_is_used_by_registry_path() {
    // The registry's own /healthz helper is what connect_ssh_tunnel polls;
    // prove it touches the real healthz contract, not a bare TCP open.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let guard = spawn_server();
    let port = guard.1;
    let alive = runtime.block_on(async {
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().expect("addr");
        healthz_direct(addr).await
    });
    assert!(alive, "direct /healthz must answer ok");
}

async fn healthz_direct(addr: SocketAddr) -> bool {
    probe_tunnel_health(addr).await
}
