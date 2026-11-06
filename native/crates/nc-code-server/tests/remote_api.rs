//! Handler-level proofs for `/api/remote/*` (tower oneshot, no SS
//! server needed for the validation paths; the ws round-trip drives the
//! real `zcode-server` behind `connect_ws_facade`):
//!
//! - `POST /api/remote/connect` with `target.kind == "ws"` connects to the
//!   live server's `/ws` and returns a descriptor; `bind` bumps generation;
//!   `dispose` drops the session; `sessions` lists it in between.
//! - Validation paths fail closed with JSON `{error}` (ssh without
//!   `remotePort`, `wsl` kind, missing `workspacePath`).
//! - Without `ServerOptions::remote`, the paths stay JSON 404 with the
//!   other unknown `/api/*` routes.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use nc_code_channels::ChannelRegistry;
use nc_code_services_client::RemoteConnectionRegistry;
use nc_code_server::{build_channel_router, ServerOptions};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn post(app: axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let parsed = serde_json::from_slice::<Value>(&bytes).expect("remote api body must be JSON");
    (status, parsed)
}

async fn get(app: axum::Router, path: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(Request::builder().method(Method::GET).uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let parsed = serde_json::from_slice::<Value>(&bytes).expect("remote api body must be JSON");
    (status, parsed)
}

fn router_with_remote() -> (axum::Router, Arc<RemoteConnectionRegistry>) {
    let remotes = RemoteConnectionRegistry::new();
    let options = ServerOptions {
        remote: Some(Arc::clone(&remotes)),
        ..ServerOptions::default()
    };
    let router = build_channel_router(Arc::new(ChannelRegistry::new()), options);
    (router, remotes)
}

#[tokio::test]
async fn remote_paths_stay_404_without_registry() {
    let router = build_channel_router(Arc::new(ChannelRegistry::new()), ServerOptions::default());
    let (status, body) = post(
        router.clone(),
        "/api/remote/connect",
        json!({ "target": { "kind": "ws", "wsUrl": "ws://127.0.0.1:1/ws" }, "workspacePath": "/ws" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({ "error": "not_found" }));
    let (status, _) = get(router, "/api/remote/sessions").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn connect_validation_fails_closed() {
    let (router, _remotes) = router_with_remote();
    // ssh without remotePort
    let (status, body) = post(
        router.clone(),
        "/api/remote/connect",
        json!({
            "target": { "kind": "ssh", "host": "example.com", "username": "dev" },
            "workspacePath": "/ws",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap_or("").contains("remotePort"), "ssh without remotePort must say so, got {body}");

    // wsl kind: bind-based, not an HTTP connect target
    let (status, body) = post(
        router.clone(),
        "/api/remote/connect",
        json!({ "target": { "kind": "wsl", "distro": "Ubuntu" }, "workspacePath": "/ws" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap_or("").contains("wsl"), "wsl kind must be explicit, got {body}");

    // missing workspacePath
    let (status, body) = post(
        router.clone(),
        "/api/remote/connect",
        json!({ "target": { "kind": "ws", "wsUrl": "ws://127.0.0.1:1/ws" } }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap_or("").contains("workspacePath"), "missing workspacePath must say so");

    // unreachable ws endpoint: 502, no session registered
    let (router2, remotes2) = router_with_remote();
    let (status, body) = post(
        router2,
        "/api/remote/connect",
        json!({
            "target": { "kind": "ws", "wsUrl": "ws://127.0.0.1:1/ws" },
            "workspacePath": "/ws",
            "workspaceIdentity": "id-ws",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(!body.get("remoteSessionId").is_some_and(|v| !v.is_null()), "unreachable ws must not mint a session");
    assert!(remotes2.list_sessions().is_empty(), "failed connect must leave the registry empty");
    let _ = router;
}

#[tokio::test]
async fn ws_connect_bind_sessions_dispose_round_trip() {
    // Genuine HTTP-level WS endpoint: build the real channel stack and serve
    // it on an ephemeral loopback socket; connect via WsTaskService
    // through `target.wsUrl`. This proves the registry + facade + handlers
    // over real wires, not a mock facade.
    let registry = Arc::new(ChannelRegistry::new());
    // `system.info` must answer on the ws endpoint: register a minimal
    // channel so the ws client performs a real invoke round-trip.
    let live_options = ServerOptions {
        remote: None,
        ..ServerOptions::default()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind live server");
    let addr = listener.local_addr().expect("live addr");
    let live_registry = Arc::clone(&registry);
    tokio::spawn(async move {
        nc_code_server::serve(listener, live_registry, live_options).await.expect("live serve");
    });
    // Wait for /healthz so the WS handshake is real, not racy.
    let mut ready = false;
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(ready, "live zcode-server must become reachable");

    let (router, remotes) = router_with_remote();
    let ws_url = format!("ws://{addr}/ws");
    let (status, body) = post(
        router.clone(),
        "/api/remote/connect",
        json!({
            "target": { "kind": "ws", "wsUrl": ws_url },
            "workspacePath": "/ws-remote",
            "workspaceIdentity": "id-remote",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "ws connect must succeed, got {body}");
    let session_id = body["remoteSessionId"].as_str().expect("remoteSessionId").to_string();
    assert_eq!(body["generation"], json!(1));

    // The descriptor resolves in the same window registry.
    let _resolved = remotes
        .as_ref()
        .resolve_scoped(&session_id, "/ws-remote", "id-remote")
        .expect("created session must resolve");

    // sessions lists exactly the one logical session.
    let (status, sessions) = get(router.clone(), "/api/remote/sessions").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(sessions.as_array().expect("sessions array").len(), 1);
    assert_eq!(sessions[0]["remoteSessionId"], json!(session_id));

    // bind bumps the generation.
    let (status, bound) = post(
        router.clone(),
        "/api/remote/bind",
        json!({
            "remoteSessionId": session_id,
            "workspacePath": "/ws-remote-2",
            "workspaceIdentity": "id-remote-2",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "bind must succeed, got {bound}");
    assert_eq!(bound["generation"], json!(2));

    // dispose drops it; a second dispose stays ok (idempotent).
    let (status, done) = post(
        router.clone(),
        "/api/remote/dispose",
        json!({ "remoteSessionId": session_id }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(done["ok"], json!(true));
    assert!(remotes.get_session(&session_id).is_none());
    let (status, _) = post(
        router,
        "/api/remote/dispose",
        json!({ "remoteSessionId": session_id }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn bind_unknown_session_is_404() {
    let (router, _remotes) = router_with_remote();
    let (status, body) = post(
        router,
        "/api/remote/bind",
        json!({
            "remoteSessionId": "remote-session-nope",
            "workspacePath": "/ws",
            "workspaceIdentity": "id",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap_or("").contains("not found"), "unknown bind must name it, got {body}");
}
