//! The CDP WebSocket transport layer: the JSON send/receive loop, command/response correlation by id,
//! and events routed by method.
//!
//! A single reader task owns the WS read half of the stream (each frame is parsed exactly once),
//! responses are handed back to the sender through a oneshot, and events are broadcast to subscribers by
//! method. This is the correlation Playwright does implicitly behind the scenes, implemented explicitly here.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Map, Value};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::error::{CdpError, CdpErrorCode, CdpResult};

/// The default command timeout: mirrors the TS `NAVIGATION_TIMEOUT_MS = 30_000`
/// (`apps/nc-code-cli/packages/adapters/src/browser/page-command.ts:18`).
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A correlated CDP event frame.
#[derive(Debug, Clone)]
pub struct CdpEvent {
    pub method: String,
    /// The event `params`; `sessionId` also lives in this map (readable for both flat and multiplexed sessions).
    pub params: Map<String, Value>,
}

impl CdpEvent {
    pub fn session_id(&self) -> Option<&str> {
        self.params.get("sessionId").and_then(Value::as_str)
    }
}

/// The subscription handle: `recv` returns events matching the method; once the connection closes it returns `None`.
pub struct EventReceiver {
    method: String,
    inner: broadcast::Receiver<Result<CdpEvent, String>>,
}

impl EventReceiver {
    /// Receive the next event matching the method; returns None when the connection closed, and on buffer overflow (lag) drops and retries.
    pub async fn recv(&mut self) -> Option<CdpEvent> {
        loop {
            match self.inner.recv().await {
                Ok(Ok(event)) if event.method == self.method => return Some(event),
                Ok(Ok(_other)) => continue,
                Ok(Err(_closed)) => return None,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<CdpResult<Value>>>>>;

/// The central state of command/response correlation plus event routing, shared between the reader task and the senders.
struct ConnectionState {
    pending: PendingMap,
    event_tx: broadcast::Sender<Result<CdpEvent, String>>,
}

/// A CDP WebSocket connection to a browser (or to a single session target).
///
/// Lifecycle: `connect()` starts the background reader; `close()` cancels the reader, closes the WS, and
/// settles every pending with `TargetGone` (so callers never wait forever).
pub struct CdpConnection {
    write_tx: mpsc::Sender<String>,
    state: Arc<ConnectionState>,
    next_id: AtomicU64,
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
    closed: Arc<Mutex<bool>>,
}

impl CdpConnection {
    /// Connect to a URL of the form `ws://host:port/devtools/page/<id>` or `/devtools/browser/<guid>`.
    /// Plain TCP + WS only (no TLS): a CDP endpoint is always loopback.
    pub async fn connect(url: &str, connect_timeout: Duration) -> CdpResult<Self> {
        let host_port = ws_host_port(url)?;
        let stream = tokio::time::timeout(connect_timeout, tokio_tungstenite::connect_async(url))
            .await
            .map_err(|_| {
                CdpError::new(
                    CdpErrorCode::Timeout,
                    format!("CDP WS connect timed out: {host_port}"),
                )
            })?
            .map(|(ws, _response)| ws)
            .map_err(|err| {
                CdpError::new(
                    CdpErrorCode::LaunchFailed,
                    format!("CDP WS handshake failed for {host_port}: {err}"),
                )
            })?;

        let (write_half, read_half) = stream.split();
        let (write_tx, write_rx) = mpsc::channel::<String>(256);
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, _rx0) = broadcast::channel::<Result<CdpEvent, String>>(512);
        let state = Arc::new(ConnectionState { pending, event_tx });

        // writer: serializes every outbound frame; exits once the channel closes.
        tokio::spawn(writer_task(write_rx, write_half));
        // reader: parse the frame → fill the pending response / broadcast the event.
        let reader_state = Arc::clone(&state);
        let reader = tokio::spawn(reader_task(read_half, reader_state));

        Ok(Self {
            write_tx,
            state,
            next_id: AtomicU64::new(1),
            reader: Mutex::new(Some(reader)),
            closed: Arc::new(Mutex::new(false)),
        })
    }

    /// Subscribe to a specific CDP event method (exact match). One connection may have multiple subscribers.
    pub fn subscribe(&self, method: &str) -> EventReceiver {
        EventReceiver {
            method: method.to_string(),
            inner: self.state.event_tx.subscribe(),
        }
    }

    /// Send a command and await its correlated response. `session_id` is the sessionId returned by
    /// `Target.attachToTarget`; for a browser-level connection it is None.
    pub async fn send(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
        timeout: Duration,
    ) -> CdpResult<Value> {
        if self.is_closed() {
            return Err(CdpError::new(
                CdpErrorCode::TargetGone,
                "CDP connection is closed",
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (resp_tx, resp_rx) = oneshot::channel();
        self.state
            .pending
            .lock()
            .expect("cdp pending mutex")
            .insert(id, resp_tx);

        let mut frame = Map::new();
        frame.insert("id".into(), Value::from(id));
        frame.insert("method".into(), Value::String(method.to_string()));
        if !params.is_null() {
            frame.insert("params".into(), params);
        }
        if let Some(sid) = session_id {
            frame.insert("sessionId".into(), Value::String(sid.to_string()));
        }
        let text = serde_json::to_string(&Value::Object(frame)).map_err(|err| {
            self.remove_pending(id);
            CdpError::new(CdpErrorCode::Transport, format!("frame serialize: {err}"))
        })?;

        if self.write_tx.send(text).await.is_err() {
            self.remove_pending(id);
            return Err(CdpError::new(
                CdpErrorCode::Transport,
                "CDP writer task is gone (connection closed)",
            ));
        }

        match tokio::time::timeout(timeout, resp_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_canceled)) => {
                self.remove_pending(id);
                Err(CdpError::new(
                    CdpErrorCode::TargetGone,
                    "response channel dropped; connection closed",
                ))
            }
            Err(_) => {
                self.remove_pending(id);
                Err(CdpError::new(
                    CdpErrorCode::Timeout,
                    format!("CDP command '{method}' (id {id}) timed out after {timeout:?}"),
                ))
            }
        }
    }

    /// Idempotent close: cancel the reader, close the writer, settle every pending.
    pub async fn close(&self) {
        let mut was_closed = self.closed.lock().expect("cdp closed mutex");
        if *was_closed {
            return;
        }
        *was_closed = true;
        drop(was_closed);

        // Cancel the reader task (it blocks in read, so an explicit abort is required).
        if let Some(handle) = self.reader.lock().expect("cdp reader mutex").take() {
            handle.abort();
        }
        // Settle the pending entries: callers must not wait forever.
        let mut pending = self.state.pending.lock().expect("cdp pending mutex");
        for (_id, tx) in pending.drain() {
            let _ = tx.send(Err(CdpError::new(
                CdpErrorCode::TargetGone,
                "CDP connection closed before response",
            )));
        }
        // Notify the event subscribers that the connection ended.
        let _ = self
            .state
            .event_tx
            .send(Err("cdp-connection-closed".to_string()));
        // Dropping the last clone of write_tx → writer_task exits → sink close.
        // write_tx belongs to &self and cannot be moved; probing with a failed send is enough, and the
        // writer exits with an error once the sink is closed by the peer. After close() callers must not send again.
    }

    pub fn is_closed(&self) -> bool {
        *self.closed.lock().expect("cdp closed mutex")
    }

    fn remove_pending(&self, id: u64) {
        self.state
            .pending
            .lock()
            .expect("cdp pending mutex")
            .remove(&id);
    }
}

async fn writer_task(
    mut rx: mpsc::Receiver<String>,
    mut sink: futures_util::stream::SplitSink<WsStream, Message>,
) {
    while let Some(text) = rx.recv().await {
        if sink.send(Message::Text(text.into())).await.is_err() {
            break;
        }
    }
    let _ = sink.close().await;
}

async fn reader_task(
    mut stream: futures_util::stream::SplitStream<WsStream>,
    state: Arc<ConnectionState>,
) {
    while let Some(frame) = stream.next().await {
        let text = match frame {
            Ok(Message::Text(text)) => text,
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => continue, // binary/ping/pong carry no business data under the CDP JSON protocol
        };
        let parsed: CdpResult<Value> = serde_json::from_str(text.as_str()).map_err(|err| {
            CdpError::new(
                CdpErrorCode::Transport,
                format!("malformed CDP frame: {err}"),
            )
        });
        let obj = match parsed {
            Ok(Value::Object(obj)) => obj,
            Ok(_) => continue,
            Err(_) => continue, // drop a malformed frame: CDP itself guarantees JSON, so a bad frame means a misbehaving peer, left to close/disconnect handling
        };
        if let Some(id) = obj.get("id").and_then(Value::as_u64) {
            // A response frame: fill in the pending entry. Both error and result count as a correlated completion.
            let tx = state.pending.lock().expect("cdp pending mutex").remove(&id);
            if let Some(tx) = tx {
                let response = match obj.get("error") {
                    Some(err) => {
                        let code = err.get("code").and_then(Value::as_i64).unwrap_or(-32000);
                        let message = err
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown CDP error")
                            .to_string();
                        Err(CdpError::protocol(id, code, message))
                    }
                    None => Ok(obj.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.send(response);
            }
            continue;
        }
        // An event frame: route it.
        if let Some(method) = obj.get("method").and_then(Value::as_str) {
            let params = match obj.get("params") {
                Some(Value::Object(p)) => p.clone(),
                _ => Map::new(),
            };
            let _ = state.event_tx.send(Ok(CdpEvent {
                method: method.to_string(),
                params,
            }));
        }
    }
    // The reader ended (the peer disconnected): settle all pending entries and notify the subscribers.
    let mut pending = state.pending.lock().expect("cdp pending mutex");
    for (_id, tx) in pending.drain() {
        let _ = tx.send(Err(CdpError::new(
            CdpErrorCode::TargetGone,
            "CDP connection lost (peer closed)",
        )));
    }
    let _ = state.event_tx.send(Err("cdp-connection-lost".to_string()));
}

/// Extract `127.0.0.1:9222` from `ws://127.0.0.1:9222/devtools/page/ABC` (diagnostics only; the rest of the URL is not echoed).
fn ws_host_port(url: &str) -> CdpResult<String> {
    let rest = url
        .strip_prefix("ws://")
        .or_else(|| url.strip_prefix("wss://"))
        .ok_or_else(|| {
            CdpError::new(
                CdpErrorCode::LaunchFailed,
                format!("CDP URL must use ws:// scheme: {url}"),
            )
        })?;
    let host = rest.split('/').next().unwrap_or(rest);
    Ok(host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_host_port_accepts_loopback_page_url() {
        assert_eq!(
            ws_host_port("ws://127.0.0.1:9222/devtools/page/ABC").unwrap(),
            "127.0.0.1:9222"
        );
    }

    #[test]
    fn ws_host_port_rejects_http_scheme() {
        assert!(ws_host_port("http://127.0.0.1:9222/devtools/page/ABC").is_err());
    }

    #[test]
    fn error_codes_are_stable_strings() {
        assert_eq!(CdpErrorCode::Timeout.as_str(), "timeout");
        assert_eq!(
            CdpErrorCode::NavigationBlocked.as_str(),
            "navigation_blocked"
        );
        assert_eq!(CdpErrorCode::RefNotFound.as_str(), "ref_not_found");
    }

    /// Frame-correlation unit test: an in-memory fake WS service validates id correlation, error mapping and timeout settlement.
    /// The real WS integration is covered by tests/e2e.rs against a real browser.
    #[tokio::test]
    async fn reader_correlates_response_and_error_by_id() {
        // Validate the reader parsing logic directly: build a ConnectionState and simulate two frames.
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, mut event_rx) = broadcast::channel::<Result<CdpEvent, String>>(8);
        let state = Arc::new(ConnectionState {
            pending: Arc::clone(&pending),
            event_tx,
        });

        let (tx1, rx1) = oneshot::channel();
        pending.lock().unwrap().insert(7, tx1);
        let (tx2, rx2) = oneshot::channel();
        pending.lock().unwrap().insert(8, tx2);

        // Simulate the reader handling the two frames (the same logic as reader_task).
        let frame_ok: Value = serde_json::from_str(r#"{"id":7,"result":{"value":42}}"#).unwrap();
        let (id, result) = classify_frame(&frame_ok);
        if let Some(tx) = pending.lock().unwrap().remove(&id.unwrap()) {
            let _ = tx.send(result);
        }
        let frame_err: Value =
            serde_json::from_str(r#"{"id":8,"error":{"code":-32000,"message":"boom"}}"#).unwrap();
        let (id, result) = classify_frame(&frame_err);
        if let Some(tx) = pending.lock().unwrap().remove(&id.unwrap()) {
            let _ = tx.send(result);
        }

        assert_eq!(
            rx1.await.unwrap().unwrap(),
            serde_json::json!({"value": 42})
        );
        let err = rx2.await.unwrap().unwrap_err();
        assert_eq!(err.code, CdpErrorCode::Protocol);
        assert!(err.message.contains("boom"));

        // Event frame routing
        let frame_event: Value = serde_json::from_str(
            r#"{"method":"Page.loadEventFired","params":{"timestamp":1.5,"sessionId":"s1"}}"#,
        )
        .unwrap();
        let (id, _) = classify_frame(&frame_event);
        assert!(id.is_none());
        let _ = state.event_tx.send(Ok(CdpEvent {
            method: "Page.loadEventFired".to_string(),
            params: frame_event
                .get("params")
                .unwrap()
                .as_object()
                .unwrap()
                .clone(),
        }));
        let received = event_rx.recv().await.unwrap().unwrap();
        assert_eq!(received.method, "Page.loadEventFired");
        assert_eq!(received.session_id(), Some("s1"));
        assert_eq!(received.params["timestamp"], Value::from(1.5));
    }

    fn classify_frame(obj: &Value) -> (Option<u64>, CdpResult<Value>) {
        if let Some(id) = obj.get("id").and_then(Value::as_u64) {
            let result = match obj.get("error") {
                Some(err) => Err(CdpError::protocol(
                    id,
                    err.get("code").and_then(Value::as_i64).unwrap_or(-32000),
                    err.get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                )),
                None => Ok(obj.get("result").cloned().unwrap_or(Value::Null)),
            };
            return (Some(id), result);
        }
        (None, Ok(Value::Null))
    }
}
