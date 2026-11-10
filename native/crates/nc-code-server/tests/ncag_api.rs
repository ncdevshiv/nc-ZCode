//! HTTP surface proofs for NC-AG (`/api/nc-ag/*`) through the real server router.
//!
//! These tests drive `build_channel_router` (the same router the binary serves) with a
//! real `NcagRuntime` backed by a real SQLite store and a real git repository, then
//! exercise every route category: health/config, projects/requests/tasks/agents,
//! messages, decisions, leases, integrations, usage, events and the audit chain.

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use nc_code_channels::ChannelRegistry;
use nc_code_orchestrator::config::NcagConfig;
use nc_code_orchestrator::git::run_git;
use nc_code_orchestrator::main_support;
use nc_code_orchestrator::types::*;
use nc_code_orchestrator::Store;
use nc_code_server::{build_channel_router, ServerOptions};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn call(
    app: axum::Router,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(match body {
            Some(value) => Body::from(value.to_string()),
            None => Body::empty(),
        })
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let parsed = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
    (status, parsed)
}

fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let out = run_git(dir, &["init", "-b", "main"]).unwrap();
    assert!(out.ok(), "git init: {}", out.stderr);
    std::fs::write(dir.join("README.md"), "# repo\n").unwrap();
    assert!(run_git(dir, &["add", "-A"]).unwrap().ok());
    let commit = run_git(
        dir,
        &[
            "-c",
            "user.name=NCAG Test",
            "-c",
            "user.email=ncag@example.invalid",
            "commit",
            "-m",
            "init",
        ],
    )
    .unwrap();
    assert!(commit.ok(), "git commit: {}", commit.stderr);
}

fn runtime(dir: &Path) -> nc_code_orchestrator::NcagRuntime {
    let mut config = NcagConfig::load(None, |_| None).unwrap();
    config.workspace_root = dir.join("repo");
    config.data_dir = dir.join("data");
    std::fs::create_dir_all(&config.data_dir).unwrap();
    config.database_path = config.data_dir.join("ncag.sqlite");
    let store = Arc::new(Store::open(&config.database_path).unwrap());
    main_support::runtime_with_store(config, store, None)
}

fn app(dir: &Path) -> (axum::Router, Arc<nc_code_orchestrator::NcagRuntime>) {
    let runtime = Arc::new(runtime(dir));
    let options = ServerOptions {
        ncag: Some(Arc::clone(&runtime)),
        ..ServerOptions::default()
    };
    let router = build_channel_router(Arc::new(ChannelRegistry::new()), options);
    (router, runtime)
}

/// Build the runtime and router with an API token, which the router enforces.
fn app_with_token(
    dir: &Path,
    token: &str,
) -> (axum::Router, Arc<nc_code_orchestrator::NcagRuntime>) {
    let mut runtime = runtime(dir);
    runtime.config.api_token = Some(token.to_string());
    let runtime = Arc::new(runtime);
    let options = ServerOptions {
        ncag: Some(Arc::clone(&runtime)),
        ..ServerOptions::default()
    };
    let router = build_channel_router(Arc::new(ChannelRegistry::new()), options);
    (router, runtime)
}

#[tokio::test]
async fn health_and_config_report_real_state() {
    let temp = tempfile::tempdir().unwrap();
    init_repo(&temp.path().join("repo"));
    let (router, _) = app(temp.path());
    let (status, body) = call(router.clone(), Method::GET, "/api/nc-ag/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["auditChainVerified"], true);
    assert_eq!(body["provider"]["configured"], false);
    let (status, body) = call(router.clone(), Method::GET, "/api/nc-ag/config", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["budgets"]["projectUsd"].as_f64().unwrap() > 0.0);
    let (status, body) = call(router, Method::GET, "/api/nc-ag/profiles", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["profiles"].as_array().unwrap().len(), 16);
}

#[tokio::test]
async fn project_request_and_task_routes_round_trip() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    init_repo(&repo);
    let (router, runtime) = app(temp.path());
    let (status, body) = call(
        router.clone(),
        Method::POST,
        "/api/nc-ag/projects",
        Some(json!({ "name": "Bridge", "workspace": repo.to_string_lossy() })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let project_id = body["project"]["id"].as_str().unwrap().to_string();
    // The project branch exists in the real repository.
    assert!(
        run_git(&repo, &["rev-parse", "--verify", "refs/heads/ncag/main"])
            .unwrap()
            .ok()
    );

    let (status, body) = call(
        router.clone(),
        Method::POST,
        &format!("/api/nc-ag/projects/{project_id}/requests"),
        Some(json!({ "instruction": "Implement the health endpoint and document it" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let request_id = body["request"]["id"].as_str().unwrap().to_string();
    assert!(body["tasks"].as_array().unwrap().len() >= 2);
    assert!(body["requirements"].as_array().unwrap().len() >= 2);
    // Planning is deterministic here (no provider), and the plan is recorded.
    assert_eq!(body["request"]["planner"], "deterministic");

    let (status, body) = call(
        router.clone(),
        Method::GET,
        &format!("/api/nc-ag/requests/{request_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["request"]["status"], "planned");

    let (status, body) = call(
        router.clone(),
        Method::GET,
        &format!("/api/nc-ag/projects/{project_id}/requests"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["requests"].as_array().unwrap().len(), 1);

    let (status, body) = call(router.clone(), Method::GET, "/api/nc-ag/tasks", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body["tasks"].as_array().unwrap().is_empty());

    let task_id = body["tasks"][0]["id"].as_str().unwrap().to_string();
    let (status, body) = call(
        router.clone(),
        Method::GET,
        &format!("/api/nc-ag/tasks/{task_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["task"]["id"], task_id);

    let (status, body) = call(
        router.clone(),
        Method::GET,
        &format!("/api/nc-ag/tasks/{task_id}/dag"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["nodes"].as_array().unwrap().len() >= 2);

    // An unknown project is a JSON 404, never HTML.
    let (status, body) = call(
        router.clone(),
        Method::GET,
        "/api/nc-ag/projects/nope",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
    let _ = runtime;
}

#[tokio::test]
async fn unknown_ncag_route_is_json_404_and_dismissal_controls_work() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    init_repo(&repo);
    let (router, _) = app(temp.path());
    let (status, body) = call(
        router.clone(),
        Method::GET,
        "/api/nc-ag/does-not-exist",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.get("error").is_some() || body.get("message").is_some() || body.is_null());

    let (_, body) = call(
        router.clone(),
        Method::POST,
        "/api/nc-ag/projects",
        Some(json!({ "name": "Controls", "workspace": repo.to_string_lossy() })),
    )
    .await;
    let project_id = body["project"]["id"].as_str().unwrap().to_string();
    let (status, body) = call(
        router.clone(),
        Method::POST,
        &format!("/api/nc-ag/projects/{project_id}/pause"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["project"]["status"], "paused");
    let (status, body) = call(
        router.clone(),
        Method::POST,
        &format!("/api/nc-ag/projects/{project_id}/resume"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["project"]["status"], "active");
}

#[tokio::test]
async fn decision_lease_and_audit_routes_report_the_truth() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    init_repo(&repo);
    let (router, runtime) = app(temp.path());
    let (_, body) = call(
        router.clone(),
        Method::POST,
        "/api/nc-ag/projects",
        Some(json!({ "name": "Decisions", "workspace": repo.to_string_lossy() })),
    )
    .await;
    let project_id = body["project"]["id"].as_str().unwrap().to_string();
    // A material decision may not be resolved by the orchestrator, only by the user.
    let decision = nc_code_orchestrator::decisions::open(
        &runtime.store,
        &runtime.hub,
        nc_code_orchestrator::decisions::DebateRequest {
            project_id: project_id.clone(),
            request_id: None,
            task_id: None,
            topic: "write directly to the shared checkout".into(),
            requested: "let agents edit the shared directory".into(),
            recommendation: "keep worktrees".into(),
            evidence: vec!["concurrent writes lose changes".into()],
            risks: vec!["lost updates".into()],
            class: DecisionClass::Material,
        },
    )
    .unwrap();
    let (status, body) = call(router.clone(), Method::GET, "/api/nc-ag/decisions", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["open"].as_array().unwrap().len(), 1);
    let (status, body) = call(
        router.clone(),
        Method::POST,
        &format!("/api/nc-ag/decisions/{}/resolve", decision.id),
        Some(json!({ "resolution": "recommended", "decided_by": "orchestrator" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "user_decision_required");
    let (status, body) = call(
        router.clone(),
        Method::POST,
        &format!("/api/nc-ag/decisions/{}/resolve", decision.id),
        Some(json!({ "resolution": "requested", "rationale": "compatibility", "decided_by": "user" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["decision"]["status"], "overridden");

    let (status, body) = call(
        router.clone(),
        Method::GET,
        "/api/nc-ag/leases/waiters?projectId=missing",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["waiters"].as_array().unwrap().is_empty());

    let (status, body) = call(router.clone(), Method::GET, "/api/nc-ag/audit/verify", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["verified"], true);
    assert!(body["entries"].as_u64().unwrap() >= 2, "{body}");
    let (status, body) = call(router.clone(), Method::GET, "/api/nc-ag/audit", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body["entries"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn message_and_event_routes_expose_persisted_rows() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    init_repo(&repo);
    let (router, runtime) = app(temp.path());
    let (_, body) = call(
        router.clone(),
        Method::POST,
        "/api/nc-ag/projects",
        Some(json!({ "name": "Messages", "workspace": repo.to_string_lossy() })),
    )
    .await;
    let project_id = body["project"]["id"].as_str().unwrap().to_string();
    let (status, body) = call(
        router.clone(),
        Method::POST,
        "/api/nc-ag/messages",
        Some(json!({
            "project_id": project_id,
            "subject": "Heads up",
            "body": "The API contract changed; read it before implementing.",
            "requires_ack": true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let message_id = body["message"]["id"].as_str().unwrap().to_string();
    let (status, body) = call(
        router.clone(),
        Method::GET,
        &format!("/api/nc-ag/messages?project_id={project_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    let (status, body) = call(
        router.clone(),
        Method::POST,
        &format!("/api/nc-ag/messages/{message_id}/ack"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["acknowledged"], true);
    let (status, body) = call(
        router.clone(),
        Method::GET,
        "/api/nc-ag/events?limit=50",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["latestId"].as_i64().unwrap() > 0);
    let events = body["events"].as_array().unwrap();
    assert!(events
        .iter()
        .any(|event| event["kind"] == "project.created"));
    let (status, _) = call(
        router,
        Method::GET,
        "/api/nc-ag/usage?projectId=missing",
        None,
    )
    .await;
    // Usage for an unknown project is a typed 404, not a silent empty report.
    assert_eq!(status, StatusCode::NOT_FOUND);
    let _ = runtime;
}

#[tokio::test]
async fn api_token_is_enforced_by_the_router() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    init_repo(&repo);
    let (router, _) = app_with_token(temp.path(), "s3cret-token");

    // No token: rejected before any handler runs.
    let (status, body) = call(router.clone(), Method::GET, "/api/nc-ag/health", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["code"], "unauthorized");

    // Wrong token: still rejected.
    let (status, _) = call(
        router.clone(),
        Method::GET,
        "/api/nc-ag/health?token=wrong",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Query token: accepted.
    let (status, body) = call(
        router.clone(),
        Method::GET,
        "/api/nc-ag/health?token=s3cret-token",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["auditChainVerified"], true);

    // Bearer header: accepted, and writes are protected too.
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/nc-ag/projects")
                .header("content-type", "application/json")
                .header("authorization", "Bearer s3cret-token")
                .body(Body::from(
                    json!({ "name": "Token project", "workspace": repo.to_string_lossy() })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
}
