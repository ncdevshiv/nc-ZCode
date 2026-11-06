//! One client connection: decode requests, dispatch to channels, stream events.
//!
//! The outbound side is an unbounded FIFO of channel-message payloads. Responses and
//! event frames share that FIFO, so within one connection there is a single ordering
//! point — the same discipline the stdio host uses for response/frame co-order.
//!
//! Cancellation: `PromiseCancel` / `EventDispose` set the request's [`CancelToken`].
//! Event tasks select on it, so a disposed subscription stops promptly; promise tasks
//! keep running but their result is dropped (matching TS, where the server CTS is only
//! an advisory signal and the client has already removed the handler).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use nc_code_rpc::{ChannelMessage, WireValue};
use tokio::sync::mpsc;

use crate::error::ChannelFailure;
use crate::events::CancelToken;
use crate::registry::{ChannelContext, ChannelRegistry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolViolation {
    /// The payload is not a valid channel message (wire decode failure).
    Malformed(String),
    /// The client sent a server→client message type.
    UnexpectedMessage(String),
    /// The transport's outbound half was closed.
    OutboundClosed,
}

impl std::fmt::Display for ProtocolViolation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(message) => write!(formatter, "malformed channel message: {message}"),
            Self::UnexpectedMessage(kind) => {
                write!(
                    formatter,
                    "unexpected {kind} message in the client→server direction"
                )
            }
            Self::OutboundClosed => write!(formatter, "connection outbound channel closed"),
        }
    }
}

impl std::error::Error for ProtocolViolation {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveKind {
    Promise,
    Event,
}

struct ActiveRequest {
    cancel: CancelToken,
    kind: ActiveKind,
}

pub struct ChannelConnection {
    registry: Arc<ChannelRegistry>,
    ctx: ChannelContext,
    outbound: mpsc::UnboundedSender<Vec<u8>>,
    active: Arc<Mutex<HashMap<u32, ActiveRequest>>>,
}

impl ChannelConnection {
    pub fn new(
        registry: Arc<ChannelRegistry>,
        ctx: ChannelContext,
        outbound: mpsc::UnboundedSender<Vec<u8>>,
    ) -> Self {
        Self {
            registry,
            ctx,
            outbound,
            active: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn context(&self) -> &ChannelContext {
        &self.ctx
    }

    /// Send the `Initialize` response, matching `new ChannelServer(...)` which is ready
    /// immediately after construction.
    pub fn start(&self) -> Result<(), ProtocolViolation> {
        self.send(ChannelMessage::Initialize)
    }

    /// Dispatch one decoded frame payload.
    ///
    /// Returns `Err` only for protocol violations (malformed or wrong-direction
    /// messages); handler failures are projected into the response and are not errors
    /// of the connection.
    pub fn handle_payload(&self, payload: &[u8]) -> Result<(), ProtocolViolation> {
        let message = ChannelMessage::decode(payload)
            .map_err(|error| ProtocolViolation::Malformed(error.message))?;
        match message {
            ChannelMessage::Promise {
                id,
                channel,
                method,
                arg,
            } => {
                self.spawn_promise(id, channel, method, arg);
                Ok(())
            }
            ChannelMessage::EventListen {
                id,
                channel,
                method,
                arg,
            } => {
                self.spawn_event(id, channel, method, arg);
                Ok(())
            }
            ChannelMessage::PromiseCancel { id } => {
                self.cancel_request(id, ActiveKind::Promise);
                Ok(())
            }
            ChannelMessage::EventDispose { id } => {
                self.cancel_request(id, ActiveKind::Event);
                Ok(())
            }
            other => Err(ProtocolViolation::UnexpectedMessage(
                kind_name(&other).to_string(),
            )),
        }
    }

    /// Active request/subscription count (tests and diagnostics).
    pub fn active_requests(&self) -> usize {
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// Cancel every in-flight request/subscription (connection closing).
    pub fn shutdown(&self) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for request in active.values() {
            request.cancel.cancel();
        }
        active.clear();
    }

    fn send(&self, message: ChannelMessage) -> Result<(), ProtocolViolation> {
        self.outbound
            .send(message.encode())
            .map_err(|_| ProtocolViolation::OutboundClosed)
    }

    fn insert_active(&self, id: u32, request: ActiveRequest) {
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, request);
    }

    fn cancel_request(&self, id: u32, kind: ActiveKind) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(request) = active.get(&id) else {
            return;
        };
        if request.kind != kind {
            return;
        }
        request.cancel.cancel();
        if kind == ActiveKind::Event {
            active.remove(&id);
        }
    }

    fn spawn_promise(&self, id: u32, channel: String, method: String, arg: WireValue) {
        let cancel = CancelToken::new();
        self.insert_active(
            id,
            ActiveRequest {
                cancel: cancel.clone(),
                kind: ActiveKind::Promise,
            },
        );
        let registry = Arc::clone(&self.registry);
        let ctx = self.ctx.clone();
        let outbound = self.outbound.clone();
        let active = Arc::clone(&self.active);
        tokio::spawn(async move {
            let result = match registry.get(&channel) {
                None => Err(ChannelFailure::unknown_channel(&channel)),
                Some(handler) => {
                    handler
                        .call(&ctx, &method, normalize_args(arg), cancel.clone())
                        .await
                }
            };
            if cancel.is_cancelled() {
                remove_if_same(&active, id, &cancel);
                return;
            }
            let message = match result {
                Ok(data) => ChannelMessage::PromiseSuccess { id, data },
                Err(ChannelFailure::Thrown(data)) => ChannelMessage::PromiseErrorObj { id, data },
                Err(failure) => ChannelMessage::PromiseError {
                    id,
                    data: failure.payload().cloned().unwrap_or_default().to_value(),
                },
            };
            let _ = outbound.send(message.encode());
            remove_if_same(&active, id, &cancel);
        });
    }

    fn spawn_event(&self, id: u32, channel: String, event: String, arg: WireValue) {
        let cancel = CancelToken::new();
        self.insert_active(
            id,
            ActiveRequest {
                cancel: cancel.clone(),
                kind: ActiveKind::Event,
            },
        );
        let registry = Arc::clone(&self.registry);
        let ctx = self.ctx.clone();
        let outbound = self.outbound.clone();
        let active = Arc::clone(&self.active);
        tokio::spawn(async move {
            let Some(handler) = registry.get(&channel) else {
                // TypeScript waits for the channel, then drops the listener silently.
                remove_if_same(&active, id, &cancel);
                return;
            };
            let subscription = tokio::select! {
                _ = cancel.cancelled() => {
                    remove_if_same(&active, id, &cancel);
                    return;
                }
                result = handler.listen(&ctx, &event, arg) => result,
            };
            let mut subscription = match subscription {
                Ok(subscription) => subscription,
                Err(_) => {
                    // EventListen failures produce no response in the TS server.
                    remove_if_same(&active, id, &cancel);
                    return;
                }
            };
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    next = subscription.next() => match next {
                        Some(data) => {
                            if outbound
                                .send(ChannelMessage::EventFire { id, data }.encode())
                                .is_err()
                            {
                                break;
                            }
                        }
                        None => break,
                    },
                }
            }
            remove_if_same(&active, id, &cancel);
        });
    }
}

impl Drop for ChannelConnection {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// `ProxyChannel.toService` sends positional argument arrays. A raw client may send
/// `undefined` for a no-arg call; a single non-array value is treated as one argument.
fn normalize_args(arg: WireValue) -> Vec<WireValue> {
    match arg {
        WireValue::Undefined => Vec::new(),
        WireValue::Array(items) => items,
        other => vec![other],
    }
}

fn remove_if_same(active: &Mutex<HashMap<u32, ActiveRequest>>, id: u32, cancel: &CancelToken) {
    let mut active = active
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if active
        .get(&id)
        .is_some_and(|request| request.cancel.is_same(cancel))
    {
        active.remove(&id);
    }
}

fn kind_name(message: &ChannelMessage) -> &'static str {
    match message {
        ChannelMessage::Promise { .. } => "Promise",
        ChannelMessage::PromiseCancel { .. } => "PromiseCancel",
        ChannelMessage::EventListen { .. } => "EventListen",
        ChannelMessage::EventDispose { .. } => "EventDispose",
        ChannelMessage::Initialize => "Initialize",
        ChannelMessage::PromiseSuccess { .. } => "PromiseSuccess",
        ChannelMessage::PromiseError { .. } => "PromiseError",
        ChannelMessage::PromiseErrorObj { .. } => "PromiseErrorObj",
        ChannelMessage::EventFire { .. } => "EventFire",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positional_arguments_are_normalized() {
        assert!(normalize_args(WireValue::Undefined).is_empty());
        assert_eq!(
            normalize_args(WireValue::Array(vec![WireValue::Int(1)])),
            vec![WireValue::Int(1)]
        );
        assert_eq!(
            normalize_args(WireValue::String("x".into())),
            vec![WireValue::String("x".into())]
        );
    }
}
