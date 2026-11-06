//! Live WS channel e2e against the real `zcode-server` binary.
//!
//! Spawns the binary, parses `LISTENING`, connects a WS client, asserts the
//! server's immediate `Initialize` response, then performs a real
//! `system.info` Promise round-trip over the channel wire. This proves the
//! transport the MessagePort bridge forwards (binary WS messages = framed
//! channel messages).

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use nc_code_rpc::{encode_frame, encode_request, ChannelMessage, FrameDecoder, WireValue};

struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_server() -> (Guard, u16, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("zcode-ws-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let mut child = Command::new(env!("CARGO_BIN_EXE_zcode-server"))
        .args(["--host", "127.0.0.1", "--port", "0", "--static-dir"])
        .arg(&dir)
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
    (Guard(child), port, dir)
}

async fn recv_frame(
    stream: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> ChannelMessage {
    let mut decoder = FrameDecoder::default();
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("ws next timeout")
            .expect("ws message")
            .expect("ws msg ok");
        let bytes = match msg {
            tokio_tungstenite::tungstenite::protocol::Message::Binary(b) => b,
            other => panic!("expected binary ws message, got {other:?}"),
        };
        let frames = decoder.push(&bytes).expect("decode frames");
        for frame in frames {
            if let Ok(message) = ChannelMessage::decode(&frame.payload) {
                return message;
            }
        }
    }
}

#[tokio::test]
async fn live_server_initialize_and_system_info_round_trip() {
    let (_guard, port, dir) = spawn_server();
    let url = format!("ws://127.0.0.1:{port}/ws");
    let (mut stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");

    // Server sends Initialize immediately on connect.
    let init = recv_frame(&mut stream).await;
    assert!(
        matches!(init, ChannelMessage::Initialize),
        "first frame must be Initialize, got {init:?}"
    );

    // Promise(system.info) → PromiseSuccess(id=1).
    let request = encode_request(1, "system", "info", &WireValue::Undefined);
    stream
        .send(tungstenite_frame(&request))
        .await
        .expect("ws send");
    let response = recv_frame(&mut stream).await;
    match response {
        ChannelMessage::PromiseSuccess { id, .. } => assert_eq!(id, 1),
        other => panic!("expected PromiseSuccess id=1, got {other:?}"),
    }

    std::fs::remove_dir_all(&dir).ok();
}

fn tungstenite_frame(payload: &[u8]) -> tokio_tungstenite::tungstenite::protocol::Message {
    let framed = encode_frame(payload);
    tokio_tungstenite::tungstenite::protocol::Message::Binary(framed)
}
