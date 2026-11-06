//! HTTP remote-session endpoints (`/api/remote/*`, spec §2 / §6).
//!
//! The Electron `rustChannelBridge` forwards `ConnectRemoteWorkspace` here:
//! the producer is this server's own window [`RemoteConnectionRegistry`]
//! (the same instance the channel `window-controller` routes through), so a
//! descriptor minted here resolves over `/ws` on this server. `POST
//! /api/remote/connect` accepts:
//!
//! - `target.kind == "ssh"`: spawns `ssh -N -L` to the remote `zcode-server`
//!   and registers a real WS facade over the tunnel
//!   ([`RemoteConnectionRegistry::connect_ssh_tunnel_ws`]). `target.remotePort`
//!   (remote `zcode-server` TCP port) is required; `target.username` is
//!   supported (`user@host` destination). Fails closed with `RemoteSourceUnavailable`
//!   when the tunnel or WS connect fails — never a fake session.
//! - `target.kind == "ws"`: `target.wsUrl` names an already-forwarded
//!   `ws://…/ws` endpoint (the same shape an SSH `-L` / WSL bind / Docker
//!   volume forward exposes); a real [`WsTaskService`] is connected to it.
//! - `wsl` / `docker`: bind-based transports whose facade is produced
//!   host-side; they register via `POST /api/remote/bind` after the host
//!   established the forward. Unknown kinds are rejected, never guessed.
//!
//! `bind` / `dispose` / `sessions` are thin wrappers over the registry:
//! bind bumps the generation, dispose is idempotent, sessions lists the
//! window's logical sessions. Routes are registered only when the server is
//! constructed with a registry (`ServerOptions::remote`); otherwise the
//! paths stay JSON 404 with the other unknown `/api/*` routes.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use axum::Router;
use nc_code_services_client::remote_registry::RemoteConnectionRegistry;
use nc_code_services_client::WsTaskService;
use serde_json::{json, Value};

use crate::ChannelState;

fn error(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": message.into() })))
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty())
}

/// Register the remote endpoints onto a router that already carries
/// [`ChannelState`]. Called only when `ServerOptions::remote` is `Some`.
pub fn register(router: Router<ChannelState>) -> Router<ChannelState> {
    router
        .route("/api/remote/connect", post(remote_connect))
        .route("/api/remote/bind", post(remote_bind))
        .route("/api/remote/dispose", post(remote_dispose))
        .route("/api/remote/sessions", get(remote_sessions))
}

fn remotes(state: &ChannelState) -> Arc<RemoteConnectionRegistry> {
    state
        .remote()
        .expect("remote routes registered only with a registry")
}

async fn remote_connect(
    State(state): State<ChannelState>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let workspace_path = match str_field(&body, "workspacePath") {
        Some(path) => path.to_string(),
        None => return error(StatusCode::BAD_REQUEST, "workspacePath is required"),
    };
    let workspace_identity =
        str_field(&body, "workspaceIdentity").unwrap_or(&workspace_path).to_string();
    let target = match body.get("target") {
        Some(target) if target.is_object() => target.clone(),
        _ => return error(StatusCode::BAD_REQUEST, "target is required"),
    };
    let kind = match str_field(&target, "kind") {
        Some(kind) => kind.to_string(),
        None => return error(StatusCode::BAD_REQUEST, "target.kind is required"),
    };
    let registry = remotes(&state);
    let snapshot = match kind.as_str() {
        "ssh" => {
            let host = match str_field(&target, "host") {
                Some(host) => host.to_string(),
                None => return error(StatusCode::BAD_REQUEST, "target.host is required for ssh"),
            };
            let destination = match str_field(&target, "username") {
                Some(user) => format!("{user}@{host}"),
                None => host.clone(),
            };
            let remote_port = match target.get("remotePort").and_then(Value::as_u64) {
                Some(port) if (1..=u16::MAX as u64).contains(&port) => port as u16,
                _ => {
                    return error(
                        StatusCode::BAD_REQUEST,
                        "target.remotePort (remote zcode-server port) is required for ssh remote connect",
                    )
                }
            };
            let registry = registry.clone();
            let workspace_path = workspace_path.clone();
            let workspace_identity = workspace_identity.clone();
            // Tunnel spawn + health probe block; keep them off the async worker.
            let joined = tokio::task::spawn_blocking(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("current-thread runtime for ssh tunnel connect");
                runtime.block_on(registry.connect_ssh_tunnel_ws(
                    destination.as_str(),
                    remote_port,
                    workspace_path,
                    workspace_identity,
                ))
            })
            .await;
            match joined {
                Ok(Ok(snapshot)) => snapshot,
                Ok(Err(e)) => {
                    return error(StatusCode::BAD_GATEWAY, format!("ssh remote connect failed: {e}"))
                }
                Err(e) => {
                    return error(StatusCode::INTERNAL_SERVER_ERROR, format!("ssh remote connect panicked: {e}"))
                }
            }
        }
        "ws" => {
            let ws_url = match str_field(&target, "wsUrl") {
                Some(url) => url.to_string(),
                None => return error(StatusCode::BAD_REQUEST, "target.wsUrl is required for ws remote connect"),
            };
            let facade = match WsTaskService::connect(&ws_url).await {
                Ok(facade) => facade,
                Err(e) => {
                    return error(
                        StatusCode::BAD_GATEWAY,
                        format!("ws remote connect failed ({ws_url}): {e}"),
                    )
                }
            };
            registry.connect_ws_facade(workspace_path.clone(), workspace_identity.clone(), facade)
        }
        other => {
            return error(
                StatusCode::BAD_REQUEST,
                format!("unsupported remote target kind '{other}' (wsl/docker are host-side bind transports; register via /api/remote/bind)"),
            )
        }
    };
    (
        StatusCode::OK,
        Json(json!({
            "remoteSessionId": snapshot.remote_session_id,
            "generation": snapshot.generation,
            "workspacePath": snapshot.workspace_path,
            "workspaceIdentity": snapshot.workspace_identity,
        })),
    )
}

async fn remote_bind(
    State(state): State<ChannelState>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let session_id = match str_field(&body, "remoteSessionId") {
        Some(id) => id.to_string(),
        None => return error(StatusCode::BAD_REQUEST, "remoteSessionId is required"),
    };
    let workspace_path = match str_field(&body, "workspacePath") {
        Some(path) => path.to_string(),
        None => return error(StatusCode::BAD_REQUEST, "workspacePath is required"),
    };
    let workspace_identity = str_field(&body, "workspaceIdentity")
        .map(|s| s.to_string())
        .unwrap_or_else(|| workspace_path.clone());
    match remotes(&state).bind_workspace_context(&session_id, workspace_path, workspace_identity) {
        Ok(generation) => (
            StatusCode::OK,
            Json(json!({ "remoteSessionId": session_id, "generation": generation })),
        ),
        Err(e) => error(StatusCode::NOT_FOUND, e.to_string()),
    }
}

async fn remote_dispose(
    State(state): State<ChannelState>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let session_id = match str_field(&body, "remoteSessionId") {
        Some(id) => id.to_string(),
        None => return error(StatusCode::BAD_REQUEST, "remoteSessionId is required"),
    };
    remotes(&state).dispose_session(&session_id);
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "remoteSessionId": session_id })),
    )
}

async fn remote_sessions(State(state): State<ChannelState>) -> Json<Value> {
    let sessions: Vec<Value> = remotes(&state)
        .list_sessions()
        .into_iter()
        .map(|s| {
            json!({
                "remoteSessionId": s.remote_session_id,
                "workspacePath": s.workspace_path,
                "workspaceIdentity": s.workspace_identity,
                "generation": s.generation,
            })
        })
        .collect();
    Json(json!(sessions))
}
