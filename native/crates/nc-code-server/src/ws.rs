//! WebSocket transport for channel RPC.
//!
//! Each binary WebSocket message carries 0..N framed channel messages (the browser
//! `SocketProtocol` writes one frame per message, but the decoder tolerates
//! fragmentation/coalescing). Outbound payloads are framed here, so the channel
//! runtime stays transport-agnostic.
//!
//! Fail-closed: a malformed channel message closes the socket with 1002, an oversized
//! declared frame closes with 1009. Unknown channels/methods are *not* protocol
//! violations — they get an error response and the connection stays open.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use nc_code_channels::{ChannelConnection, ChannelContext, ChannelRegistry, ClientMode};
use nc_code_rpc::{encode_frame, ChannelMessage, FrameDecoder, PROTOCOL_MESSAGE_REGULAR};
use tokio::sync::mpsc;

static CONNECTION_SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub fn next_connection_id(prefix: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let sequence = CONNECTION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{millis}-{sequence}")
}

pub async fn handle_socket(
    mut socket: WebSocket,
    registry: Arc<ChannelRegistry>,
    client_mode: ClientMode,
    connection_id: String,
) {
    let (outbound, mut outgoing) = mpsc::unbounded_channel::<Vec<u8>>();
    let connection = ChannelConnection::new(
        registry,
        ChannelContext::new(connection_id.clone(), client_mode),
        outbound,
    );
    if connection.start().is_err() {
        return;
    }

    let mut decoder = FrameDecoder::default();
    let violation = loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Binary(bytes))) => {
                        match decoder.push(&bytes) {
                            Ok(frames) => {
                                let mut protocol_error = None;
                                for frame in frames {
                                    if frame.message_type != PROTOCOL_MESSAGE_REGULAR {
                                        continue;
                                    }
                                    if let Err(error) = connection.handle_payload(&frame.payload) {
                                        protocol_error = Some(error);
                                        break;
                                    }
                                }
                                if let Some(error) = protocol_error {
                                    break Some((1002, error.to_string()));
                                }
                            }
                            Err(error) => break Some((1009, error.message)),
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break None,
                    Some(Ok(_)) => {}
                    Some(Err(error)) => break Some((1011, format!("websocket receive failed: {error}"))),
                }
            }
            payload = outgoing.recv() => {
                match payload {
                    Some(payload) => {
                        log_channel_error(&payload);
                        if socket
                            .send(Message::Binary(encode_frame(&payload).into()))
                            .await
                            .is_err()
                        {
                            break None;
                        }
                    }
                    None => break None,
                }
            }
        }
    };

    connection.shutdown();
    if let Some((code, reason)) = violation {
        eprintln!("[nc-code-server] channel connection {connection_id} closed ({code}): {reason}");
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code,
                reason: reason.into(),
            })))
            .await;
    }
}

/// Surface channel-level failures in server logs: a missing channel/method must never
/// be silent during the TS→Rust migration.
fn log_channel_error(payload: &[u8]) {
    let Ok(message) = ChannelMessage::decode(payload) else {
        return;
    };
    match message {
        ChannelMessage::PromiseError { id, data } => {
            let name = data
                .get("name")
                .and_then(|value| value.as_str())
                .unwrap_or("Error");
            let text = data
                .get("message")
                .and_then(|value| value.as_str())
                .unwrap_or("(no message)");
            eprintln!("[nc-code-server] rpc error id={id} {name}: {text}");
        }
        ChannelMessage::PromiseErrorObj { id, .. } => {
            eprintln!("[nc-code-server] rpc error id={id} thrown non-Error value");
        }
        _ => {}
    }
}
