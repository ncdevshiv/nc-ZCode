//! End-to-end tests for the server runtime, driven with the exact channel-message
//! bytes the TypeScript client sends (see `nc-code-rpc`'s golden vectors).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nc_code_channels::{
    CancelToken, Channel, ChannelConnection, ChannelContext, ChannelFailure, ChannelRegistry,
    ClientMode, EventBus, EventSubscription,
};
use nc_code_rpc::{ChannelMessage, WireValue};
use serde_json::json;
use tokio::sync::{mpsc, Notify};

struct TestChannel {
    bus: EventBus,
    slow_started: Arc<Notify>,
    slow_cancelled: Arc<AtomicBool>,
}

#[async_trait]
impl Channel for TestChannel {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        match command {
            "echo" => Ok(WireValue::Array(args)),
            "noop" => Ok(WireValue::Undefined),
            "fail" => Err(ChannelFailure::error("boom").with_code("test.fail")),
            "thrown" => Err(ChannelFailure::Thrown(WireValue::Json(
                json!({"custom": true}),
            ))),
            "slow" => {
                self.slow_started.notify_one();
                cancel.cancelled().await;
                self.slow_cancelled.store(true, Ordering::SeqCst);
                Ok(WireValue::Undefined)
            }
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }

    async fn listen(
        &self,
        _ctx: &ChannelContext,
        event: &str,
        _arg: WireValue,
    ) -> Result<EventSubscription, ChannelFailure> {
        match event {
            "tick" => Ok(EventSubscription::from_broadcast(self.bus.subscribe())),
            other => Err(ChannelFailure::event_not_found(other)),
        }
    }
}

struct Harness {
    connection: ChannelConnection,
    outbound: mpsc::UnboundedReceiver<Vec<u8>>,
    bus: EventBus,
    slow_started: Arc<Notify>,
    slow_cancelled: Arc<AtomicBool>,
}

fn harness() -> Harness {
    let bus = EventBus::new(256);
    let slow_started = Arc::new(Notify::new());
    let slow_cancelled = Arc::new(AtomicBool::new(false));
    let registry = Arc::new(ChannelRegistry::new());
    registry
        .register(
            "test",
            Arc::new(TestChannel {
                bus: bus.clone(),
                slow_started: Arc::clone(&slow_started),
                slow_cancelled: Arc::clone(&slow_cancelled),
            }),
        )
        .expect("register test channel");
    let (sender, receiver) = mpsc::unbounded_channel();
    let connection = ChannelConnection::new(
        registry,
        ChannelContext::new("conn-test", ClientMode::WebRemoteReplayable),
        sender,
    );
    Harness {
        connection,
        outbound: receiver,
        bus,
        slow_started,
        slow_cancelled,
    }
}

async fn next_message(harness: &mut Harness, timeout_ms: u64) -> Option<ChannelMessage> {
    let payload = tokio::time::timeout(Duration::from_millis(timeout_ms), harness.outbound.recv())
        .await
        .ok()??;
    Some(ChannelMessage::decode(&payload).expect("outbound payload must decode"))
}

async fn wait_for(mut predicate: impl FnMut() -> bool) {
    for _ in 0..200 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("condition was not reached in 1s");
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn promise(id: u32, channel: &str, method: &str, arg: WireValue) -> Vec<u8> {
    ChannelMessage::Promise {
        id,
        channel: channel.to_string(),
        method: method.to_string(),
        arg,
    }
    .encode()
}

#[tokio::test]
async fn initialize_matches_the_typescript_golden_bytes() {
    let mut harness = harness();
    harness.connection.start().expect("start");
    let payload = harness.outbound.recv().await.expect("initialize payload");
    assert_eq!(hex(&payload), "040106c80100");
}

#[tokio::test]
async fn promise_success_and_errors_are_projected_like_typescript() {
    let mut harness = harness();

    harness
        .connection
        .handle_payload(&promise(
            0,
            "test",
            "echo",
            WireValue::Array(vec![WireValue::Int(7)]),
        ))
        .expect("dispatch");
    assert_eq!(
        next_message(&mut harness, 1000).await,
        Some(ChannelMessage::PromiseSuccess {
            id: 0,
            data: WireValue::Array(vec![WireValue::Int(7)]),
        })
    );

    harness
        .connection
        .handle_payload(&promise(1, "test", "noop", WireValue::Undefined))
        .expect("dispatch");
    assert_eq!(
        next_message(&mut harness, 1000).await,
        Some(ChannelMessage::PromiseSuccess {
            id: 1,
            data: WireValue::Undefined,
        })
    );

    harness
        .connection
        .handle_payload(&promise(2, "test", "fail", WireValue::Undefined))
        .expect("dispatch");
    match next_message(&mut harness, 1000).await {
        Some(ChannelMessage::PromiseError { id, data }) => {
            assert_eq!(id, 2);
            assert_eq!(data["message"], json!("boom"));
            assert_eq!(data["name"], json!("Error"));
            assert_eq!(data["code"], json!("test.fail"));
        }
        other => panic!("expected PromiseError, got {other:?}"),
    }

    harness
        .connection
        .handle_payload(&promise(3, "test", "thrown", WireValue::Undefined))
        .expect("dispatch");
    assert_eq!(
        next_message(&mut harness, 1000).await,
        Some(ChannelMessage::PromiseErrorObj {
            id: 3,
            data: WireValue::Json(json!({"custom": true})),
        })
    );

    harness
        .connection
        .handle_payload(&promise(4, "test", "nope", WireValue::Undefined))
        .expect("dispatch");
    match next_message(&mut harness, 1000).await {
        Some(ChannelMessage::PromiseError { id, data }) => {
            assert_eq!(id, 4);
            assert_eq!(data["message"], json!("Method not found: nope"));
        }
        other => panic!("expected PromiseError, got {other:?}"),
    }

    harness
        .connection
        .handle_payload(&promise(5, "missing", "echo", WireValue::Undefined))
        .expect("dispatch");
    match next_message(&mut harness, 1000).await {
        Some(ChannelMessage::PromiseError { id, data }) => {
            assert_eq!(id, 5);
            assert_eq!(data["name"], json!("Unknown channel"));
            assert!(data["message"]
                .as_str()
                .expect("message")
                .contains("missing"));
        }
        other => panic!("expected PromiseError, got {other:?}"),
    }
}

#[tokio::test]
async fn events_are_forwarded_and_dispose_stops_them() {
    let mut harness = harness();
    harness
        .connection
        .handle_payload(
            &ChannelMessage::EventListen {
                id: 10,
                channel: "test".to_string(),
                method: "tick".to_string(),
                arg: WireValue::Undefined,
            }
            .encode(),
        )
        .expect("dispatch");
    wait_for(|| harness.bus.receiver_count() > 0).await;

    harness.bus.send(WireValue::Json(json!({"n": 1})));
    assert_eq!(
        next_message(&mut harness, 1000).await,
        Some(ChannelMessage::EventFire {
            id: 10,
            data: WireValue::Json(json!({"n": 1})),
        })
    );

    harness
        .connection
        .handle_payload(&ChannelMessage::EventDispose { id: 10 }.encode())
        .expect("dispatch");
    wait_for(|| harness.bus.receiver_count() == 0).await;
    harness.bus.send(WireValue::Json(json!({"n": 2})));
    assert_eq!(next_message(&mut harness, 100).await, None);
}

#[tokio::test]
async fn unknown_event_and_unknown_channel_listen_are_silent() {
    let mut harness = harness();
    for (id, channel, event) in [(11, "test", "nope"), (12, "missing", "tick")] {
        harness
            .connection
            .handle_payload(
                &ChannelMessage::EventListen {
                    id,
                    channel: channel.to_string(),
                    method: event.to_string(),
                    arg: WireValue::Undefined,
                }
                .encode(),
            )
            .expect("dispatch");
    }
    assert_eq!(next_message(&mut harness, 150).await, None);
}

#[tokio::test]
async fn promise_cancel_is_observed_and_suppresses_the_result() {
    let mut harness = harness();
    harness
        .connection
        .handle_payload(&promise(20, "test", "slow", WireValue::Undefined))
        .expect("dispatch");
    tokio::time::timeout(Duration::from_secs(1), harness.slow_started.notified())
        .await
        .expect("handler must start");

    harness
        .connection
        .handle_payload(&ChannelMessage::PromiseCancel { id: 20 }.encode())
        .expect("dispatch");
    wait_for(|| harness.slow_cancelled.load(Ordering::SeqCst)).await;
    assert_eq!(next_message(&mut harness, 150).await, None);
    assert_eq!(harness.connection.active_requests(), 0);
}

#[tokio::test]
async fn shutdown_disposes_event_subscriptions() {
    let harness = harness();
    harness
        .connection
        .handle_payload(
            &ChannelMessage::EventListen {
                id: 30,
                channel: "test".to_string(),
                method: "tick".to_string(),
                arg: WireValue::Undefined,
            }
            .encode(),
        )
        .expect("dispatch");
    wait_for(|| harness.bus.receiver_count() > 0).await;
    harness.connection.shutdown();
    wait_for(|| harness.bus.receiver_count() == 0).await;
}

#[tokio::test]
async fn malformed_and_wrong_direction_messages_are_protocol_violations() {
    let harness = harness();
    assert!(matches!(
        harness.connection.handle_payload(&[0xff, 0x00]),
        Err(nc_code_channels::ProtocolViolation::Malformed(_))
    ));
    assert!(matches!(
        harness
            .connection
            .handle_payload(&ChannelMessage::Initialize.encode()),
        Err(nc_code_channels::ProtocolViolation::UnexpectedMessage(_))
    ));
}
