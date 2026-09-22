//! Router 层验收（tower oneshot，不起真实 socket）：
//! 健康 JSON、版本漂移守护、SPA fallback、静态资源、未知 /api 的 JSON 404、
//! 非 GET 静态请求语义，以及 HostPort 接线（降级 host → 503）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use zcode_server::{build_router, build_router_with_host, InMemoryHost, ProcessLivenessHost};

/// 每个用例独立临时静态目录，避免并发测试互相覆盖。
fn temp_static_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "zcode-server-router-{}-{tag}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp static dir");
    dir
}

async fn get(app: axum::Router, path: &str) -> (StatusCode, Option<String>, Vec<u8>) {
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap_or_default().to_string());
    let bytes = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, content_type, bytes)
}

async fn send(
    app: axum::Router,
    method: Method,
    path: &str,
) -> (StatusCode, Option<String>, Vec<u8>) {
    let response = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap_or_default().to_string());
    let bytes = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, content_type, bytes)
}

#[tokio::test]
async fn healthz_returns_ok_json() {
    let (status, content_type, body) = get(build_router(None), "/healthz").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type
        .unwrap_or_default()
        .contains("application/json"));
    let parsed: Value = serde_json::from_slice(&body).expect("healthz body must be JSON");
    assert_eq!(parsed, serde_json::json!({ "status": "ok" }));
}

#[tokio::test]
async fn version_matches_root_package_json() {
    // 漂移守护：crates 版本必须等于仓库根 package.json 的 version。
    // 路径相对本测试文件（tests/ → crate → crates → native → 仓库根）。
    let pkg: Value =
        serde_json::from_str(include_str!("../../../../package.json")).expect("parse package.json");
    let pkg_version = pkg["version"].as_str().expect("package.json version");
    assert_eq!(
        env!("CARGO_PKG_VERSION"),
        pkg_version,
        "zcode-server 版本与 package.json 漂移"
    );

    let (status, content_type, body) = get(build_router(None), "/api/version").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type
        .unwrap_or_default()
        .contains("application/json"));
    let parsed: Value = serde_json::from_slice(&body).expect("version body must be JSON");
    assert_eq!(parsed["version"], pkg_version);
}

#[tokio::test]
async fn spa_fallback_serves_index_html_for_client_route() {
    let dir = temp_static_dir("spa");
    std::fs::write(
        dir.join("index.html"),
        "<html><body>zcode-spa-marker</body></html>",
    )
    .expect("write index.html");
    let (status, content_type, body) = get(build_router(Some(dir.clone())), "/workspace/x").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.unwrap_or_default().contains("text/html"));
    let html = String::from_utf8(body).unwrap();
    assert!(
        html.contains("zcode-spa-marker"),
        "SPA fallback 必须回 index.html"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn static_asset_served_from_temp_dir() {
    let dir = temp_static_dir("asset");
    std::fs::write(dir.join("index.html"), "<html>index</html>").expect("write index.html");
    std::fs::write(dir.join("app.js"), "console.log('zcode-asset-marker');").expect("write app.js");
    let (status, content_type, body) = get(build_router(Some(dir.clone())), "/app.js").await;
    assert_eq!(status, StatusCode::OK);
    let js = String::from_utf8(body).unwrap();
    assert!(js.contains("zcode-asset-marker"));
    // tower-http 按扩展名推断 MIME。
    assert!(
        content_type.unwrap_or_default().contains("javascript"),
        "静态 JS 必须带 JavaScript content-type"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn unknown_api_route_is_json_404_not_html() {
    let dir = temp_static_dir("api404");
    // 有静态目录时也不能让 /api 未命中落到 index.html。
    std::fs::write(dir.join("index.html"), "<html>index</html>").expect("write index.html");
    let (status, content_type, body) = get(
        build_router(Some(dir.clone())),
        "/api/definitely-not-a-route",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let ct = content_type.unwrap_or_default();
    assert!(
        ct.contains("application/json"),
        "content-type 必须是 JSON，实得 {ct}"
    );
    let parsed: Value = serde_json::from_slice(&body).expect("404 body must be JSON");
    assert_eq!(parsed["error"], "not_found");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn post_to_unknown_api_route_is_json_404() {
    let (status, content_type, body) = send(
        build_router(None),
        Method::POST,
        "/api/definitely-not-a-route",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(content_type
        .unwrap_or_default()
        .contains("application/json"));
    let parsed: Value = serde_json::from_slice(&body).expect("404 body must be JSON");
    assert_eq!(parsed["error"], "not_found");
}

#[tokio::test]
async fn non_get_on_static_asset_is_method_not_allowed() {
    let dir = temp_static_dir("method");
    std::fs::write(dir.join("index.html"), "<html>index</html>").expect("write index.html");
    std::fs::write(dir.join("app.js"), "console.log(1);").expect("write app.js");
    let (status, _, body) = send(build_router(Some(dir.clone())), Method::POST, "/app.js").await;
    // ServeDir 只服务 GET/HEAD；POST 必须被拒绝且不得返回资源体。
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(body.is_empty(), "405 不应携带资源体");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn degraded_host_makes_healthz_unavailable() {
    // HostPort 接线证明：router state 持有 host，降级态必须反映到 /healthz。
    let router = build_router_with_host(None, Arc::new(InMemoryHost::unhealthy("host offline")));
    let (status, content_type, body) = get(router, "/healthz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(content_type
        .unwrap_or_default()
        .contains("application/json"));
    let parsed: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["status"], "unavailable");
    assert_eq!(parsed["detail"], "host offline");
}

#[tokio::test]
async fn in_memory_host_ping_roundtrip() {
    // 测试替身行为本身（经 HostPort 动态分发）。
    let host = InMemoryHost::healthy();
    use zcode_server::HostPort;
    assert!(host.ping().await.ok);
    host.set_ok(false);
    assert!(!host.ping().await.ok);
}

#[tokio::test]
async fn process_liveness_host_reports_unwired_backend() {
    // 生产默认端口的诚实性契约：报告「进程存活」，且必须显式声明后端未接线，
    // 不允许出现一个恒健康却什么都不代表的假 host。
    use zcode_server::HostPort;
    let ping = ProcessLivenessHost.ping().await;
    assert!(ping.ok, "HTTP 进程自身可服务");
    assert!(
        ping.detail.contains("not wired"),
        "必须显式声明后端未接线，实得 {:?}",
        ping.detail
    );
}
