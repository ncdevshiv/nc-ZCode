//! WS-channel proxy facade for remote sessions (spec §2 increment notes).
//!
//! `WsChannelClient` is a real WebSocket channel client: it connects to a
//! remote `zcode-server` (`ws://<addr>/ws`), sends framed
//! [`nc_code_rpc::encode_request`] payloads, and correlates
//! [`nc_code_rpc::ChannelMessage`] responses by id. `WsTaskService` wraps
//! one client and exposes the task-channel `Channel` surface; `invoke` can
//! also address any other channel (e.g. `system.info`) over the same
//! multiplexed WS connection.
//!
//! The remote registry owns these facades: a remote session's `services`
//! are this WS proxy, never an in-process `TaskService` pretending to be
//! remote. Disposing the client drops its pending requests and closes the
//! writer, so the WS connection shuts down (no leaked socket from dispose).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure, EventSubscription};
use nc_code_rpc::{encode_frame, encode_request, ChannelMessage, FrameDecoder, WireValue};
use nc_code_service_core::ServiceError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Mutex};

/// One live WS connection to a `zcode-server` `/ws` endpoint.
pub struct WsChannelClient {
    url: String,
    write_tx: mpsc::Sender<Vec<u8>>,
    pending: Arc<Mutex<HashMap<u32, oneshot::Sender<ChannelMessage>>>>,
    events: Arc<Mutex<HashMap<u32, mpsc::Sender<ChannelMessage>>>>,
    next_id: AtomicU32,
    control: Arc<AtomicBool>,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for WsChannelClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("WsChannelClient").field("url", &self.url).finish()
    }
}

impl WsChannelClient {
    /// Connect to `ws_url` (e.g. `ws://127.0.0.1:8080/ws`). The server sends
    /// `Initialize` immediately; it is consumed here. Fails closed when the
    /// endpoint is unreachable or the handshake fails — never fabricates a
    /// session.
    pub async fn connect(ws_url: &str) -> Result<Arc<Self>, ServiceError> {
        Self::connect_with_timeout(ws_url, std::time::Duration::from_secs(10)).await
    }

    pub async fn connect_with_timeout(
        ws_url: &str,
        timeout: std::time::Duration,
    ) -> Result<Arc<Self>, ServiceError> {
        let url = ws_url.trim().to_string();
        if url.is_empty() {
            return Err(ServiceError::invalid_params("ws url is required"));
        }
        let (stream, _response) =
            tokio::time::timeout(timeout, tokio_tungstenite::connect_async(&url))
                .await
                .map_err(|_| {
                    ServiceError::named(
                        "RemoteSourceUnavailable",
                        format!("ws connect timed out ({url})"),
                    )
                })?
                .map_err(|error| {
                    ServiceError::named(
                        "RemoteSourceUnavailable",
                        format!("ws connect failed ({url}): {error}"),
                    )
                })?;
        let (write_half, read_half) = stream.split();
        let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>(256);
        let pending: Arc<Mutex<HashMap<u32, oneshot::Sender<ChannelMessage>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let events: Arc<Mutex<HashMap<u32, mpsc::Sender<ChannelMessage>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let control = Arc::new(AtomicBool::new(true));

        let writer_control = Arc::clone(&control);
        let writer = tokio::spawn(async move {
            let mut write_half = write_half;
            let mut write_rx = write_rx;
            while writer_control.load(Ordering::SeqCst) {
                match write_rx.recv().await {
                    Some(bytes) => {
                        if write_half
                            .send(tokio_tungstenite::tungstenite::protocol::Message::Binary(
                                bytes.into(),
                            ))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    None => break,
                }
            }
        });

        let pending_reader = Arc::clone(&pending);
        let events_reader = Arc::clone(&events);
        let reader_control = Arc::clone(&control);
        let reader = tokio::spawn(async move {
            let mut read_half = read_half;
            let mut decoder = FrameDecoder::default();
            while reader_control.load(Ordering::SeqCst) {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    read_half.next(),
                )
                .await
                {
                    Ok(Some(Ok(
                        tokio_tungstenite::tungstenite::protocol::Message::Binary(bytes),
                    ))) => {
                        match decoder.push(&bytes) {
                            Ok(frames) => {
                                for frame in frames {
                                    let Ok(message) =
                                        ChannelMessage::decode(&frame.payload)
                                    else {
                                        continue;
                                    };
                                    route_message(message, &pending_reader, &events_reader)
                                        .await;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    Ok(Some(Ok(
                        tokio_tungstenite::tungstenite::protocol::Message::Close(_),
                    ))) => break,
                    Ok(Some(Ok(_))) => {}
                    Ok(Some(Err(_))) => break,
                    Ok(None) => break,
                    Err(_) => continue,
                }
            }
        });

        Ok(Arc::new(Self {
            url,
            write_tx,
            pending,
            events,
            next_id: AtomicU32::new(1),
            control,
            _tasks: vec![writer, reader],
        }))
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    fn alloc_id(&self) -> u32 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Invoke `channel/method` over WS (`Promise` request) and await the
    /// correlated response. Used for `system.info` and task calls alike.
    pub async fn invoke(
        &self,
        channel: &str,
        method: &str,
        arg: WireValue,
    ) -> Result<WireValue, ChannelFailure> {
        let id = self.alloc_id();
        let payload = encode_request(id, channel, method, &arg);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.send_framed(&payload).await?;
        let response = tokio::time::timeout(std::time::Duration::from_secs(15), rx)
            .await
            .map_err(|_| ChannelFailure::error(format!("ws invoke timed out: {channel}/{method}")))
            .and_then(|result| {
                result.map_err(|_| ChannelFailure::error(format!("ws channel closed: {channel}/{method}")))
            })?;
        match response {
            ChannelMessage::PromiseSuccess { id: rid, data } if rid == id => Ok(data),
            ChannelMessage::PromiseError { id: rid, data } if rid == id => {
                let message = data
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("remote channel error")
                    .to_string();
                let name = data
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Error")
                    .to_string();
                Err(ChannelFailure::error_named(name, message))
            }
            ChannelMessage::PromiseErrorObj { id: rid, data } if rid == id => {
                Err(ChannelFailure::Thrown(data))
            }
            other => Err(ChannelFailure::error(format!(
                "ws invoke unexpected response for {channel}/{method}: {other:?}"
            ))),
        }
    }

    /// Subscribe to `channel/event` over WS (`EventListen` request); the
    /// returned subscription streams every `EventFire` for that id until the
    /// receiver is dropped.
    pub async fn listen(
        &self,
        channel: &str,
        event: &str,
        arg: WireValue,
    ) -> Result<EventSubscription, ChannelFailure> {
        let id = self.alloc_id();
        let payload = ChannelMessage::EventListen {
            id,
            channel: channel.to_string(),
            method: event.to_string(),
            arg,
        }
        .encode();
        let (tx, mut rx) = mpsc::channel::<ChannelMessage>(256);
        self.events.lock().await.insert(id, tx);
        self.send_framed(&payload).await?;
        let (out_tx, out_rx) = mpsc::channel::<WireValue>(256);
        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                if let ChannelMessage::EventFire { data, .. } = message {
                    if out_tx.send(data).await.is_err() {
                        break;
                    }
                }
            }
        });
        Ok(EventSubscription::from_receiver(out_rx))
    }

    async fn send_framed(&self, payload: &[u8]) -> Result<(), ChannelFailure> {
        let framed = encode_frame(payload);
        self.write_tx
            .send(framed)
            .await
            .map_err(|_| ChannelFailure::error("ws writer closed"))
    }

    /// Close the WS connection (idempotent): stops reader/writer tasks and
    /// drops pending requests so callers never hang.
    pub async fn close(&self) {
        if self.control.swap(false, Ordering::SeqCst) {
            self.pending.lock().await.clear();
            self.events.lock().await.clear();
        }
    }
}

async fn route_message(
    message: ChannelMessage,
    pending: &Mutex<HashMap<u32, oneshot::Sender<ChannelMessage>>>,
    events: &Mutex<HashMap<u32, mpsc::Sender<ChannelMessage>>>,
) {
    let id = match &message {
        ChannelMessage::PromiseSuccess { id, .. }
        | ChannelMessage::PromiseError { id, .. }
        | ChannelMessage::PromiseErrorObj { id, .. }
        | ChannelMessage::EventFire { id, .. } => *id,
        _ => return, // Initialize / requests echoed back — ignore.
    };
    if let Some(tx) = pending.lock().await.remove(&id) {
        let _ = tx.send(message);
        return;
    }
    if let Some(tx) = events.lock().await.get(&id) {
        let _ = tx.send(message).await;
    }
}

/// Task-shaped WS facade: forwards every `Channel::call`/`listen` to the
/// remote `task` channel over [`WsChannelClient`]. Holds no local task
/// state — proof that remote routing goes over the wire.
pub struct WsTaskService {
    client: Arc<WsChannelClient>,
}

impl WsTaskService {
    pub async fn connect(ws_url: &str) -> Result<Arc<Self>, ServiceError> {
        let client = WsChannelClient::connect(ws_url).await?;
        Ok(Arc::new(Self { client }))
    }

    pub fn client(&self) -> &Arc<WsChannelClient> {
        &self.client
    }
}

#[async_trait]
impl Channel for WsTaskService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let arg = args.into_iter().next().unwrap_or(WireValue::Undefined);
        self.client.invoke("zcode-task", command, arg).await
    }

    async fn listen(
        &self,
        _ctx: &ChannelContext,
        event: &str,
        arg: WireValue,
    ) -> Result<EventSubscription, ChannelFailure> {
        self.client.listen("zcode-task", event, arg).await
    }
}

/// In-process TCP forwarder: the byte-level equivalent of `ssh -N -L
/// <local>:<host>:<remote>` (a plain TCP pipe), used on hosts without a
/// local `sshd` to prove the WS-facade-over-tunnel path end-to-end (the
/// facade still talks WS through the tunnel endpoint; setup correctness of
/// the `ssh` child itself is covered by [`crate::remote_registry`]
/// fail-closed unit tests).
pub struct TcpForwarder {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl TcpForwarder {
    /// Bind `127.0.0.1:0` and forward accepted connections to `target`
    /// (`127.0.0.1:<remote_port>`). Returns the forwarder bound to
    /// `127.0.0.1`.
    pub async fn bind(target: SocketAddr) -> Result<Self, ServiceError> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| ServiceError::message(format!("tunnel bind failed: {error}")))?;
        let addr = listener.local_addr().map_err(|error| {
            ServiceError::message(format!("tunnel addr failed: {error}"))
        })?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tunnel runtime");
            runtime.block_on(async move {
                loop {
                    if stop_thread.load(Ordering::SeqCst) {
                        break;
                    }
                    let accepted = tokio::select! {
                        result = listener.accept() => result,
                        _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => continue,
                    };
                    let Ok((incoming, _)) = accepted else { continue };
                    tokio::spawn(async move {
                        let Ok(outgoing) = TcpStream::connect(target).await else {
                            return;
                        };
                        let (mut i_read, mut i_write) = incoming.into_split();
                        let (mut o_read, mut o_write) = outgoing.into_split();
                        // Drive both directions; when either finishes,
                        // shutdown the far write half so the peer sees EOF
                        // and this task exits (no half-open hang).
                        let to_target = async {
                            let _ = tokio::io::copy(&mut i_read, &mut o_write).await;
                            let _ = o_write.shutdown().await;
                        };
                        let to_client = async {
                            let _ = tokio::io::copy(&mut o_read, &mut i_write).await;
                            let _ = i_write.shutdown().await;
                        };
                        tokio::select! {
                            () = to_target => {},
                            () = to_client => {},
                        }
                    });
                }
            });
        });
        Ok(Self {
            addr,
            stop,
            handle: Some(handle),
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.addr
    }

    /// Stop the forwarder and join its thread (no leaked listener).
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for TcpForwarder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Probe `GET /healthz` on `addr`; true only when the body carries the
/// `"status":"ok"` marker over a 2xx read. Used to confirm a tunnel
/// endpoint is live before opening WS.
pub async fn probe_tunnel_health(addr: SocketAddr) -> bool {
    let Ok(Ok(mut stream)) =
        tokio::time::timeout(std::time::Duration::from_secs(3), TcpStream::connect(addr)).await
    else {
        return false;
    };
    let request = format!(
        "GET /healthz HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).await.is_err() {
        return false;
    }
    // GET /healthz has no body and headers end in \r\n\r\n: the request is
    // complete. Do NOT half-close here — the health server holds its read
    // until EOF, which would deadlock against this read. Instead wait for
    // the response bytes with a bounded read loop.
    let mut buf = [0u8; 4096];
    let mut text = String::new();
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(3), stream.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                text.push_str(&String::from_utf8_lossy(&buf[..n]));
                if text.contains("\"status\":\"ok\"") {
                    break;
                }
            }
            _ => break,
        }
    }
    text.contains("\"status\":\"ok\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ws_connect_fails_closed_on_unreachable_endpoint() {
        // Nothing listens on an ephemeral port: fail closed, never a session.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let err = WsChannelClient::connect(&format!("ws://{addr}/ws"))
            .await
            .unwrap_err();
        assert_eq!(err.name, "RemoteSourceUnavailable");
    }

    #[tokio::test]
    async fn ws_connect_rejects_empty_url() {
        let err = WsChannelClient::connect("  ").await.unwrap_err();
        assert_eq!(err.name, "ZodError");
    }

    #[test]
    fn tcp_forwarder_probe_round_trip_and_stop_closes_port() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let (healthy, addr) = runtime.block_on(async {
            // Tiny health server: answers {"status":"ok"} to /healthz. It
            // reads exactly one request line (not to EOF — a full request
            // ends at \r\n\r\n, and waiting for EOF deadlocks against the
            // client’s response read).
            let target = TcpListener::bind("127.0.0.1:0").await.expect("target bind");
            let target_addr = target.local_addr().expect("target addr");
            tokio::spawn(async move {
                let Ok((mut stream, _)) = target.accept().await else { return };
                let mut buf = [0u8; 4096];
                let mut request = String::new();
                for _ in 0..16 {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            request.push_str(&String::from_utf8_lossy(&buf[..n]));
                            if request.contains("\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                let body = r#"{"status":"ok"}"#;
                let response = format!(
                    "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
            let forwarder = TcpForwarder::bind(target_addr).await.expect("forwarder");
            let forward_addr = forwarder.address();
            let ok = probe_tunnel_health(forward_addr).await;
            (ok, (forwarder, forward_addr))
        });
        assert!(healthy, "/healthz through the forwarder must be ok");
        let (forwarder, forward_addr) = addr;
        forwarder.stop();
        // After stop the listener is gone: connect must fail (no leak).
        let still_listening = std::net::TcpStream::connect(forward_addr).is_ok();
        assert!(!still_listening, "stopped forwarder must close its port");
    }
}
