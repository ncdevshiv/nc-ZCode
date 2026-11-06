//! Channel-level tests: the `terminal` service driven through the real
//! `nc_code_channels` registry and connection runtime with the exact message shapes the
//! TypeScript client sends (`ProxyChannel.toService` → positional argument array,
//! `listen(name, arg)` for `onDynamic…` events).

use std::sync::Arc;
use std::time::Duration;

use nc_code_channels::{ChannelConnection, ChannelContext, ChannelRegistry, ClientMode};
use nc_code_rpc::{ChannelMessage, WireValue};
use nc_code_service_core::AppPaths;
use nc_code_services_terminal::{TerminalService, TerminalServices};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;

struct Harness {
    connection: ChannelConnection,
    outbound: mpsc::UnboundedReceiver<Vec<u8>>,
    service: Arc<TerminalService>,
    registry: Arc<ChannelRegistry>,
    /// Keeps the registered services alive for the whole test.
    _services: TerminalServices,
}

fn harness() -> Harness {
    let registry = Arc::new(ChannelRegistry::new());
    let home = std::env::temp_dir().join(format!("zcode-terminal-channel-{}", std::process::id()));
    let services = TerminalServices::new(AppPaths::new(&home, &home));
    services
        .register(&registry)
        .expect("the terminal channel registers once");
    let service = Arc::clone(&services.terminal);
    let (sender, receiver) = mpsc::unbounded_channel();
    let connection = ChannelConnection::new(
        Arc::clone(&registry),
        ChannelContext::new("conn-terminal", ClientMode::DesktopContinuous),
        sender,
    );
    Harness {
        connection,
        outbound: receiver,
        service,
        registry,
        _services: services,
    }
}

fn promise(id: u32, method: &str, args: Vec<WireValue>) -> Vec<u8> {
    ChannelMessage::Promise {
        id,
        channel: "terminal".to_string(),
        method: method.to_string(),
        arg: WireValue::Array(args),
    }
    .encode()
}

/// A no-argument direct call still sends `undefined`, like the raw channel client.
fn promise_no_args(id: u32, method: &str) -> Vec<u8> {
    ChannelMessage::Promise {
        id,
        channel: "terminal".to_string(),
        method: method.to_string(),
        arg: WireValue::Undefined,
    }
    .encode()
}

fn event_listen(id: u32, method: &str, arg: WireValue) -> Vec<u8> {
    ChannelMessage::EventListen {
        id,
        channel: "terminal".to_string(),
        method: method.to_string(),
        arg,
    }
    .encode()
}

async fn next_message(harness: &mut Harness, budget: Duration) -> Option<ChannelMessage> {
    let payload = timeout(budget, harness.outbound.recv())
        .await
        .ok()
        .flatten()?;
    Some(ChannelMessage::decode(&payload).expect("outbound payload decodes"))
}

/// Waits for the `PromiseSuccess` carrying the `create` result.
async fn create_terminal(harness: &mut Harness, request_id: u32, cols: u32, rows: u32) -> Value {
    harness
        .connection
        .handle_payload(&promise(
            request_id,
            "create",
            vec![WireValue::Json(json!({ "cols": cols, "rows": rows }))],
        ))
        .expect("dispatch create");
    loop {
        match next_message(harness, Duration::from_secs(30)).await {
            Some(ChannelMessage::PromiseSuccess { id, data }) if id == request_id => {
                let WireValue::Json(value) = data else {
                    panic!("create returns an object");
                };
                return value;
            }
            Some(ChannelMessage::PromiseError { id, data }) if id == request_id => {
                panic!("create failed: {data}");
            }
            // Other frames may be interspersed during the waiting period (subscription push, etc.): skipped, the timeout will still be reported by the None branch.
            Some(_) => continue,
            None => panic!("no response for create"),
        }
    }
}

#[tokio::test]
async fn initialize_is_sent_immediately_and_the_channel_is_registered() {
    let mut harness = harness();
    assert!(harness.registry.contains("terminal"));
    assert_eq!(harness.registry.names(), vec!["terminal".to_string()]);

    harness.connection.start().expect("start");
    let message = next_message(&mut harness, Duration::from_secs(5))
        .await
        .expect("initialize frame");
    assert_eq!(message, ChannelMessage::Initialize);
}

#[tokio::test]
async fn registering_the_same_channel_twice_is_a_startup_error() {
    let registry = Arc::new(ChannelRegistry::new());
    let services = TerminalServices::new(AppPaths::new(std::env::temp_dir(), std::env::temp_dir()));
    services.register(&registry).expect("first registration");
    let error = services
        .register(&registry)
        .expect_err("duplicate registration must fail");
    assert_eq!(error.name, "terminal");
}

#[tokio::test]
async fn create_write_resize_and_dispose_round_trip_over_the_connection() {
    let mut harness = harness();
    harness.connection.start().expect("start");
    let initialize = next_message(&mut harness, Duration::from_secs(5)).await;
    assert_eq!(initialize, Some(ChannelMessage::Initialize));

    let created = create_terminal(&mut harness, 0, 96, 28).await;
    let id = created["id"].as_str().expect("id").to_string();
    assert_eq!(id, "0");
    assert!(!created["shell"].as_str().expect("shell").is_empty());

    // onDynamicData is subscribed with the terminal id as the listen argument.
    harness
        .connection
        .handle_payload(&event_listen(
            1,
            "onDynamicData",
            WireValue::String(id.clone()),
        ))
        .expect("dispatch listen");

    // The child answers, so the connection must emit EventFire frames for id 1.
    harness
        .connection
        .handle_payload(&promise(
            2,
            "write",
            vec![WireValue::Json(json!({ "id": &id, "data": "exit\r" }))],
        ))
        .expect("dispatch write");

    let mut saw_exit_prompt = false;
    let mut saw_write_response = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline && !(saw_exit_prompt && saw_write_response) {
        let Some(message) = next_message(&mut harness, Duration::from_secs(10)).await else {
            break;
        };
        match message {
            ChannelMessage::EventFire { id: listener, data } => {
                assert_eq!(listener, 1, "events carry the listen id");
                let WireValue::String(text) = data else {
                    panic!("onDynamicData payload must be a string, got {data:?}");
                };
                if !text.is_empty() {
                    saw_exit_prompt = true;
                }
            }
            ChannelMessage::PromiseSuccess { id: request, data } => {
                if request == 2 {
                    assert_eq!(data, WireValue::Undefined, "write resolves to undefined");
                    saw_write_response = true;
                }
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }
    assert!(saw_exit_prompt, "no PTY output reached the connection");
    assert!(saw_write_response, "no response for write");

    harness
        .connection
        .handle_payload(&promise(
            3,
            "resize",
            vec![WireValue::Json(
                json!({ "id": &id, "cols": 64, "rows": 16 }),
            )],
        ))
        .expect("dispatch resize");
    let resized = next_message(&mut harness, Duration::from_secs(10)).await;
    assert_eq!(
        resized,
        Some(ChannelMessage::PromiseSuccess {
            id: 3,
            data: WireValue::Undefined
        })
    );
    assert_eq!(
        harness.service.terminal_size(&id),
        Some((64, 16)),
        "the resize went through to the PTY"
    );

    // The shell exits, so the exit subscription (id 4) reports the status once.
    harness
        .connection
        .handle_payload(&event_listen(
            4,
            "onDynamicExit",
            WireValue::String(id.clone()),
        ))
        .expect("dispatch exit listen");
    // …which was started above; if it already ended the listener is gone like in TS.
    harness
        .connection
        .handle_payload(&promise(
            5,
            "dispose",
            vec![WireValue::Json(json!({ "id": &id }))],
        ))
        .expect("dispatch dispose");
    let disposed = next_message(&mut harness, Duration::from_secs(15)).await;
    assert!(
        matches!(disposed, Some(ChannelMessage::PromiseSuccess { id: 5, .. })),
        "dispose must resolve, got {disposed:?}"
    );
    assert_eq!(harness.service.open_count(), 0);

    // Writing to a disposed terminal is the TypeScript `Terminal not found` error.
    harness
        .connection
        .handle_payload(&promise(
            6,
            "write",
            vec![WireValue::Json(json!({ "id": &id, "data": "x" }))],
        ))
        .expect("dispatch write after dispose");
    let mut error = None;
    for _ in 0..10 {
        match next_message(&mut harness, Duration::from_secs(5)).await {
            Some(ChannelMessage::PromiseError { id: 6, data }) => {
                error = Some(data);
                break;
            }
            Some(ChannelMessage::PromiseSuccess { .. }) => continue,
            Some(ChannelMessage::EventFire { .. }) => continue,
            Some(other) => panic!("unexpected frame: {other:?}"),
            None => break,
        }
    }
    let error = error.expect("PromiseError for the disposed terminal");
    assert_eq!(error["name"], json!("Error"));
    assert_eq!(error["message"], json!(format!("Terminal not found: {id}")));
}

#[tokio::test]
async fn unknown_methods_fail_like_the_typescript_channel() {
    let mut harness = harness();
    harness.connection.start().expect("start");
    let _ = next_message(&mut harness, Duration::from_secs(5)).await;

    harness
        .connection
        .handle_payload(&promise_no_args(7, "launchNuclearMissile"))
        .expect("dispatch");
    match next_message(&mut harness, Duration::from_secs(10)).await {
        Some(ChannelMessage::PromiseError { id, data }) => {
            assert_eq!(id, 7);
            assert_eq!(data["name"], json!("Error"));
            assert_eq!(
                data["message"],
                json!("Method not found: launchNuclearMissile")
            );
        }
        other => panic!("expected PromiseError, got {other:?}"),
    }

    // Bad arguments keep the ZodError name the argument kit produces.
    harness
        .connection
        .handle_payload(&promise(
            8,
            "create",
            vec![WireValue::Json(json!({ "cols": 80 }))],
        ))
        .expect("dispatch");
    match next_message(&mut harness, Duration::from_secs(10)).await {
        Some(ChannelMessage::PromiseError { id, data }) => {
            assert_eq!(id, 8);
            assert_eq!(data["name"], json!("ZodError"));
        }
        other => panic!("expected a ZodError-shaped rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn listening_to_an_unknown_event_is_dropped_silently() {
    let mut harness = harness();
    harness.connection.start().expect("start");
    let _ = next_message(&mut harness, Duration::from_secs(5)).await;

    harness
        .connection
        .handle_payload(&event_listen(
            9,
            "onDynamicMystery",
            WireValue::String("0".into()),
        ))
        .expect("dispatch");
    assert_eq!(
        next_message(&mut harness, Duration::from_millis(500)).await,
        None,
        "EventListen failures produce no frame, matching the TS server"
    );
}

#[tokio::test]
async fn an_unregistered_channel_fails_fast() {
    let mut harness = harness();
    harness.connection.start().expect("start");
    let _ = next_message(&mut harness, Duration::from_secs(5)).await;

    harness
        .connection
        .handle_payload(
            &ChannelMessage::Promise {
                id: 10,
                channel: "pty".to_string(),
                method: "create".to_string(),
                arg: WireValue::Array(vec![WireValue::Json(json!({ "cols": 80, "rows": 24 }))]),
            }
            .encode(),
        )
        .expect("dispatch");
    match next_message(&mut harness, Duration::from_secs(5)).await {
        Some(ChannelMessage::PromiseError { id, data }) => {
            assert_eq!(id, 10);
            assert_eq!(data["name"], json!("Unknown channel"));
        }
        other => panic!("expected the fail-fast Unknown channel error, got {other:?}"),
    }
}
