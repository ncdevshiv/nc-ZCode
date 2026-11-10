//! `nc-code-server`：`packages/server`（Hono HTTP 层）的 axum 镜像（S5）。
//!
//! 当前交付面（spec: `specs/rust-native-crates.md` S5 验收）：
//! - `GET /healthz` —— 存活探测，聚合 [`HostPort`] 的 host 健康态；
//! - `GET /api/version` —— 版本来自 `env!("CARGO_PKG_VERSION")`，crates 版本与仓库根
//!   `package.json` 的 `version` 由测试做偏移守护；
//! - 静态资源（tower-http `ServeDir`）+ SPA fallback（`ServeFile(index.html)`）；
//! - 未知 `/api/*` 一律返回 JSON 404，绝不落到 HTML fallback。
//!
//! Host 接线（真实 session/host 后端）是 S3/S5 的开放集成项：本 crate 只定义
//! [`HostPort`] 端口 trait；生产默认接 [`ProcessLivenessHost`]（如实报告「进程
//! 存活、后端未接线」），[`InMemoryHost`] 仅用于 `tests/` 的测试替身，两者都
//! 不写入产品路径，详见 crate README。

pub mod host_capability;
pub mod info;
pub mod remote_api;
pub mod static_files;
pub mod ws;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::ws::WebSocketUpgrade;
use axum::response::IntoResponse;
use nc_code_channels::{ChannelRegistry, ClientMode};
use tokio::net::TcpListener;

use async_trait::async_trait;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{any, get};
use axum::Router;
use serde::Serialize;
use serde_json::{json, Value};
use tower_http::services::{ServeDir, ServeFile};

/// Host 健康探测结果；`ok=false` 时 `detail` 必须带面向操作的诊断信息。
#[derive(Clone, Debug, Serialize)]
pub struct HostPing {
    pub ok: bool,
    pub detail: String,
}

/// Host 端口（seam）：server 只依赖该 trait，不依赖具体 host 实现。
///
/// 接入真实 host（S3 Rust Host / S5 strangler 集成）是开放项——spec
/// `specs/rust-native-crates.md` 规定 nc-code-server 在此之前不得跨 crate 依赖。
#[async_trait]
pub trait HostPort: Send + Sync {
    /// 探测 host 存活/就绪状态。
    async fn ping(&self) -> HostPing;
}

/// 进程存活端口：**生产默认** [`HostPort`] 实现。
///
/// 真实 session/host 后端尚未接线（spec `specs/rust-native-crates.md` S5：接线是
/// 开放集成项），此时 `/healthz` 只能证实 HTTP 进程自身存活。`ok=true` 表示进程可
/// 服务；`detail` 显式声明后端未接线——绝不报一个「假健康」而掩盖真实后端。
pub struct ProcessLivenessHost;

#[async_trait]
impl HostPort for ProcessLivenessHost {
    async fn ping(&self) -> HostPing {
        HostPing {
            ok: true,
            detail: "process alive; session host not wired (S3/S5 open integration)".to_string(),
        }
    }
}

/// 内存测试替身：仅供 `tests/` 与调试用，**不进入生产路径**
/// （生产默认是 [`ProcessLivenessHost`]）。可用 [`InMemoryHost::unhealthy`] 构造降级态。
pub struct InMemoryHost {
    ping: Mutex<HostPing>,
}

impl InMemoryHost {
    /// 构造健康的测试替身（ping 恒 `ok=true`）。
    pub fn healthy() -> Self {
        Self {
            ping: Mutex::new(HostPing {
                ok: true,
                detail: "in-memory test host".to_string(),
            }),
        }
    }

    /// 构造降级的测试替身，用于验证 `/healthz` 的聚合行为。
    pub fn unhealthy(detail: &str) -> Self {
        Self {
            ping: Mutex::new(HostPing {
                ok: false,
                detail: detail.to_string(),
            }),
        }
    }

    /// 运行时切换健康态（测试/演示用）。
    pub fn set_ok(&self, ok: bool) {
        let mut guard = self.ping.lock().expect("in-memory host lock poisoned");
        guard.ok = ok;
    }
}

#[async_trait]
impl HostPort for InMemoryHost {
    async fn ping(&self) -> HostPing {
        self.ping
            .lock()
            .expect("in-memory host lock poisoned")
            .clone()
    }
}

/// Router 共享状态：持有 host 端口，供 handler 轮询。
#[derive(Clone)]
pub struct AppState {
    pub host: Arc<dyn HostPort>,
}

/// 版本号：与仓库根 `package.json` 的 `version` 强一致（tests 做偏移守护）。
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 装配默认 router：接入进程存活端口（真实 host 未接线时的如实报告）。
pub fn build_router(static_dir: Option<PathBuf>) -> Router {
    build_router_with_host(static_dir, Arc::new(ProcessLivenessHost))
}

/// 装配指定 host 端口的 router；`static_dir` 为空时仅提供 API 路由。
pub fn build_router_with_host(static_dir: Option<PathBuf>, host: Arc<dyn HostPort>) -> Router {
    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .route("/api/version", get(app_version))
        // 未知 /api 路径必须短路为 JSON 404，禁止落入静态 fallback 的 HTML。
        .route("/api", any(api_not_found))
        .route("/api/{*key}", any(api_not_found))
        .with_state(AppState { host });

    if let Some(dir) = static_dir {
        // 非 /api 的 GET 未命中文件时回退到 index.html（SPA 路由）。
        let index = dir.join("index.html");
        router = router.fallback_service(ServeDir::new(&dir).fallback(ServeFile::new(index)));
    }
    router
}

/// 存活探测：聚合 host 端口。默认进程存活端口 → `200 {"status":"ok"}`（仅代表
/// 进程可服务；未接线状态见 [`ProcessLivenessHost`] 的 `detail`）；host 降级时返回
/// 503 JSON，让 e2e/监控能区分「进程活」与「host 不可用」。
async fn healthz(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let ping = state.host.ping().await;
    if ping.ok {
        (StatusCode::OK, Json(json!({ "status": "ok" })))
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "unavailable", "detail": ping.detail })),
        )
    }
}

/// 版本查询：来自 crate 自身版本（与 package.json 同步由测试守护）。
async fn app_version() -> (StatusCode, Json<Value>) {
    (StatusCode::OK, Json(json!({ "version": APP_VERSION })))
}

/// 未知 `/api/*` 的统一 JSON 404。
async fn api_not_found() -> (StatusCode, Json<Value>) {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not_found" })))
}

/// One workspace advertised by `/api/server-info`.
#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub path: String,
    pub label: Option<String>,
    pub workspace_identity: Option<String>,
}

impl WorkspaceInfo {
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            label: None,
            workspace_identity: None,
        }
    }

    pub fn with_identity(path: impl Into<String>, identity: Option<String>) -> Self {
        Self {
            path: path.into(),
            label: None,
            workspace_identity: identity,
        }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

/// Embedded-server options used by the Tauri shell and the standalone binary.
#[derive(Clone)]
pub struct ServerOptions {
    pub static_dir: Option<PathBuf>,
    pub spa_fallback: bool,
    pub server_id: String,
    pub name: Option<String>,
    pub auth_token: Option<String>,
    pub workspaces: Vec<WorkspaceInfo>,
    /// Window remote-session registry exposed via `/api/remote/*`
    /// ([`crate::remote_api`]). `None` keeps those paths at JSON 404.
    pub remote: Option<Arc<nc_code_services_client::RemoteConnectionRegistry>>,
    /// NC-AG orchestration platform (specs/nc-ag-orchestration.md). `None` disables
    /// `/api/nc-ag/*`; the paths then answer JSON 404 like other unknown API routes.
    pub ncag: Option<Arc<nc_code_orchestrator::NcagRuntime>>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            static_dir: None,
            spa_fallback: true,
            server_id: "nc-code-server".to_string(),
            name: None,
            auth_token: None,
            workspaces: Vec::new(),
            remote: None,
            ncag: None,
        }
    }
}

impl std::fmt::Debug for ServerOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerOptions")
            .field("static_dir", &self.static_dir)
            .field("spa_fallback", &self.spa_fallback)
            .field("server_id", &self.server_id)
            .field("name", &self.name)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "<redacted>"),
            )
            .field("workspaces", &self.workspaces)
            .field("remote", &self.remote.is_some())
            .field("ncag", &self.ncag.is_some())
            .finish()
    }
}

#[derive(Clone)]
pub struct ChannelState {
    registry: Arc<ChannelRegistry>,
    options: Arc<ServerOptions>,
}

impl ChannelState {
    pub fn remote(&self) -> Option<Arc<nc_code_services_client::RemoteConnectionRegistry>> {
        self.options.remote.clone()
    }
}

/// Serve the full backend: HTTP bootstrap + `/ws` channel RPC + static UI.
pub async fn serve(
    listener: TcpListener,
    registry: Arc<ChannelRegistry>,
    options: ServerOptions,
) -> std::io::Result<()> {
    let router = channel_router(registry, options);
    axum::serve(listener, router).await
}

/// Build the full channel router (used by handler tests via oneshot).
pub fn build_channel_router(registry: Arc<ChannelRegistry>, options: ServerOptions) -> Router {
    channel_router(registry, options)
}

fn channel_router(registry: Arc<ChannelRegistry>, options: ServerOptions) -> Router {
    let static_dir = options.static_dir.clone();
    let spa = options.spa_fallback;
    let has_remote = options.remote.is_some();
    // NC-AG is mounted before the state is moved into ChannelState: its router owns its
    // own state (orchestrator/store/hub) and must not share the channel state type.
    let ncag_router = options
        .ncag
        .as_ref()
        .map(|runtime| nc_code_orchestrator::api::router(runtime.api_state()));
    let state = ChannelState {
        registry,
        options: Arc::new(options),
    };
    let mut router = Router::new()
        .route("/healthz", get(channel_healthz))
        .route("/api/version", get(channel_version))
        .route("/api/server-info", get(server_info))
        .route("/ws", get(ws_upgrade));
    if has_remote {
        router = remote_api::register(router);
    }
    // Finalize the channel state first so both routers become `Router<()>` and can be
    // merged; the NC-AG router already carries its own state.
    let router = router
        .route("/api", any(api_not_found))
        .route("/api/{*key}", any(api_not_found))
        .with_state(state);
    let router = match ncag_router {
        Some(ncag) => router.merge(ncag),
        None => router,
    };
    if let Some(dir) = static_dir {
        let index = dir.join("index.html");
        let service = ServeDir::new(&dir).fallback(ServeFile::new(index));
        return if spa {
            router.fallback_service(service)
        } else {
            router.fallback_service(ServeDir::new(dir))
        };
    }
    router
}

async fn channel_healthz() -> (StatusCode, Json<Value>) {
    (StatusCode::OK, Json(json!({ "status": "ok" })))
}

async fn channel_version() -> (StatusCode, Json<Value>) {
    (StatusCode::OK, Json(json!({ "version": APP_VERSION })))
}

async fn server_info(State(state): State<ChannelState>) -> (StatusCode, Json<Value>) {
    let auth_required = state.options.auth_token.is_some();
    (
        StatusCode::OK,
        Json(info::server_info(
            &state.options,
            APP_VERSION,
            auth_required,
        )),
    )
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<ChannelState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| {
        ws::handle_socket(
            socket,
            Arc::clone(&state.registry),
            ClientMode::DesktopContinuous,
            ws::next_connection_id("conn"),
        )
    })
}
