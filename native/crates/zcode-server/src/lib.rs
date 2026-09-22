//! `zcode-server`：`packages/server`（Hono HTTP 层）的 axum 镜像（S5）。
//!
//! 当前交付面（spec: `specs/rust-native-crates.md` S5 验收）：
//! - `GET /healthz` — 存活探测，聚合 [`HostPort`] 的 host 健康态；
//! - `GET /api/version` — 版本来自 `env!("CARGO_PKG_VERSION")`，crates 版本与仓库根
//!   `package.json` 的 `version` 由测试做漂移守护；
//! - 静态资源（tower-http `ServeDir`）+ SPA fallback（`ServeFile(index.html)`）；
//! - 未知 `/api/*` 一律返回 JSON 404，绝不落到 HTML fallback。
//!
//! Host 接线（真实 session/host 后端）是 S3/S5 的开放集成项：本 crate 只定义
//! [`HostPort`] 端口 trait；生产默认接 [`ProcessLivenessHost`]（如实报告「仅进程
//! 存活、后端未接线」），[`InMemoryHost`] 是仅用于 `tests/` 的测试替身，两者都不
//! 冒充真实后端，详见 crate README。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{any, get};
use axum::Router;
use serde::Serialize;
use serde_json::{json, Value};
use tower_http::services::{ServeDir, ServeFile};

/// Host 健康探测结果；`ok=false` 时 `detail` 携带面向日志的诊断信息。
#[derive(Clone, Debug, Serialize)]
pub struct HostPing {
    pub ok: bool,
    pub detail: String,
}

/// Host 端口（seam）：server 只依赖该 trait，不依赖具体 host 实现。
///
/// 接入真实 host（S3 Rust Host / S5 strangler 集成）是开放项——spec
/// `specs/rust-native-crates.md` 规定 zcode-server 在此之前不得跨 crate 依赖。
#[async_trait]
pub trait HostPort: Send + Sync {
    /// 探测 host 存活/就绪状态。
    async fn ping(&self) -> HostPing;
}

/// 进程存活端口：**生产默认** [`HostPort`] 实现。
///
/// 真实 session/host 后端尚未接线（spec `specs/rust-native-crates.md` S5：接线是
/// 开放集成项），此时 `/healthz` 只能证实 HTTP 进程自身存活。`ok=true` 表示进程可
/// 服务；`detail` 显式声明后端未接线——绝不接一个「恒健康假 host」冒充真实后端。
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

/// 内存测试替身：仅供 `tests/` 与调用方自己的验收使用，**不进入生产路径**
///（生产默认是 [`ProcessLivenessHost`]）。可用 [`InMemoryHost::unhealthy`] 构造降级态。
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

    /// 运行期切换健康态（测试/演示用）。
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

/// Router 克隆态：持有 host 端口，供 handler 轮询。
#[derive(Clone)]
pub struct AppState {
    pub host: Arc<dyn HostPort>,
}

/// 版本号：与仓库根 `package.json` 的 `version` 强一致（tests 做漂移守护）。
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 组装默认 router：接进程级存活端口（真实 host 未接线时的如实报告）。
pub fn build_router(static_dir: Option<PathBuf>) -> Router {
    build_router_with_host(static_dir, Arc::new(ProcessLivenessHost))
}

/// 组装指定 host 端口的 router；`static_dir` 为空时仅提供 API 路由。
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
/// 503 JSON，让 e2e/探活能区分「进程活着」与「host 不可用」。
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

/// 版本查询：来自 crates 自身版本（与 package.json 同步由测试守护）。
async fn app_version() -> (StatusCode, Json<Value>) {
    (StatusCode::OK, Json(json!({ "version": APP_VERSION })))
}

/// 未知 `/api/*` 的统一 JSON 404。
async fn api_not_found() -> (StatusCode, Json<Value>) {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not_found" })))
}
