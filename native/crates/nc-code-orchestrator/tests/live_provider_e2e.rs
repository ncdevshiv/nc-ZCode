//! NC-AG against a **real** model gateway (skipped unless credentials are exported).
//!
//! The offline suite (`e2e_scenarios.rs`) proves the orchestration machinery against a
//! scripted SSE server. This file proves the same machinery against a real
//! OpenAI-compatible endpoint: real planning (or honest deterministic fallback), real
//! streaming tool calls, real token accounting and real review/integration gates.
//!
//! Credentials are only read from the environment and never written to the repository:
//!
//! ```sh
//! ZCODE_NCAG_LIVE_BASE_URL=https://amr-link.open-design.ai/v1 \
//! ZCODE_NCAG_LIVE_API_KEY=… \
//! ZCODE_NCAG_LIVE_MODEL=deepseek-v4.1-flash-eco \
//! cargo test -p nc-code-orchestrator --test live_provider_e2e -- --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use nc_code_orchestrator::config::NcagConfig;
use nc_code_orchestrator::git::run_git;
use nc_code_orchestrator::main_support;
use nc_code_orchestrator::types::RequestStatus;
use nc_code_orchestrator::Store;

struct Live {
    base_url: String,
    api_key: String,
    model: String,
}

fn live() -> Option<Live> {
    let base_url = std::env::var("ZCODE_NCAG_LIVE_BASE_URL").ok()?;
    let api_key = std::env::var("ZCODE_NCAG_LIVE_API_KEY").ok()?;
    let model = std::env::var("ZCODE_NCAG_LIVE_MODEL").ok()?;
    if base_url.trim().is_empty() || api_key.trim().is_empty() || model.trim().is_empty() {
        return None;
    }
    Some(Live {
        base_url,
        api_key,
        model,
    })
}

fn init_repo(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).expect("repo dir");
    let output = run_git(dir, &["init", "-b", "main"]).expect("git init");
    assert!(output.ok(), "git init failed: {}", output.stderr);
    std::fs::write(
        dir.join("README.md"),
        "# Fixture\n\nAn existing repository for the live NC-AG run.\n",
    )
    .expect("readme");
    std::fs::create_dir_all(dir.join("src")).expect("src");
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"ncag-live-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub fn add(left: u64, right: u64) -> u64 { left + right }\n\n\
         #[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn adds() {\n        \
         assert_eq!(add(2, 2), 4);\n    }\n}\n",
    )
    .expect("lib");
    assert!(run_git(dir, &["add", "-A"]).expect("git add").ok());
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
    .expect("git commit");
    assert!(commit.ok(), "git commit failed: {}", commit.stderr);
}

/// A provider store fixture pointing at the real gateway. The API key comes from the
/// environment and is written only into the throw-away data dir, never the repository.
fn live_provider_store(
    paths: nc_code_service_core::AppPaths,
    live: &Live,
) -> Arc<nc_code_services_provider::store::ProviderStore> {
    let store = nc_code_services_provider::store::ProviderStore::new(paths);
    let file = store.app_config_path();
    let snapshot = json!({
        "version": 1,
        "revision": 2,
        "providers": [{
            "providerId": "live-gateway",
            "providerName": "Live OpenAI-compatible gateway",
            "enabled": true,
            "config": {
                "group": "standard-personal",
                "api": { "type": "openai-chat-completions", "baseUrl": live.base_url },
                "access": { "type": "api-key", "apiKey": live.api_key },
                "pricing": { "inputUsdPerMillionTokens": 0.0, "outputUsdPerMillionTokens": 0.0 }
            },
            "models": [{ "modelId": live.model, "config": {}, "enabled": true }]
        }],
        "provider_order": ["live-gateway"],
        "defaultModelSelection": { "providerId": "live-gateway", "modelId": live.model }
    });
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).expect("provider store dir");
    }
    std::fs::write(
        &file,
        serde_json::to_string_pretty(&snapshot).expect("json"),
    )
    .expect("provider fixture");
    store
}

async fn call(router: &axum::Router, uri: &str) -> (u16, Value) {
    let request = axum::http::Request::builder()
        .uri(uri)
        .body(axum::body::Body::empty())
        .expect("request");
    let response = router.clone().oneshot(request).await.expect("response");
    let status = response.status().as_u16();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_real_request_runs_through_plan_agents_review_and_integration() {
    let Some(live) = live() else {
        eprintln!(
            "[skip] set ZCODE_NCAG_LIVE_BASE_URL/ZCODE_NCAG_LIVE_API_KEY/ZCODE_NCAG_LIVE_MODEL \
             to run the live NC-AG e2e"
        );
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    init_repo(&repo);
    let mut config = NcagConfig::load(None, |_| None).expect("config");
    config.workspace_root = repo.clone();
    config.data_dir = dir.path().join("ncag-data");
    std::fs::create_dir_all(&config.data_dir).expect("data dir");
    config.database_path = config.data_dir.join("ncag.sqlite");
    config.budgets.project_usd = 10.0;
    config.budgets.concurrency = 2;
    config.budgets.max_review_cycles = 1;
    let paths = nc_code_service_core::AppPaths::new(
        config.data_dir.to_string_lossy().to_string(),
        config.data_dir.to_string_lossy().to_string(),
    );
    let provider_store = live_provider_store(paths, &live);
    let store = Arc::new(Store::open(&config.database_path).expect("store"));
    let runtime = main_support::runtime_with_store(config, store, Some(provider_store));

    let project = runtime
        .orchestrator
        .create_project("live", Some(&repo.to_string_lossy()), Some("main"))
        .expect("project");
    let request = runtime
        .orchestrator
        .submit_request(
            &project.id,
            "Add a `greeting()` function to src/lib.rs that returns \"hello\", with a unit test, \
             and write NCAG_CHANGES.md describing the change and how it was verified.",
        )
        .await
        .expect("request");
    assert!(
        !request_has_no_tasks(&runtime, &request.id),
        "a real request must produce a task plan"
    );

    // Drive the deterministic supervisor the same way the background loop does.
    let deadline = std::time::Instant::now() + Duration::from_secs(900);
    let mut settled = false;
    while std::time::Instant::now() < deadline {
        let _ = runtime.supervisor.sweep().await;
        let request_now = runtime.store.request(&request.id).expect("request row");
        if matches!(
            request_now.status,
            RequestStatus::Completed | RequestStatus::Failed | RequestStatus::Cancelled
        ) {
            settled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        settled,
        "the live request never settled within the deadline"
    );
    let request_now = runtime.store.request(&request.id).expect("request row");
    let tasks = runtime.store.tasks_for_request(&request.id).expect("tasks");
    let usage = runtime.store.usage_for_project(&project.id).expect("usage");
    // A real gateway was used: token accounting is non-zero and attributed to the
    // configured provider/model (never a fabricated local counter).
    assert!(
        usage.calls > 0 && usage.input_tokens > 0,
        "the live run must record real model usage: {usage:?}; request={:?}",
        request_now.status
    );
    let rows = runtime.store.usage_rows(&project.id).expect("usage rows");
    assert!(
        rows.iter()
            .all(|row| row.provider == "live-gateway" && row.model == live.model),
        "every usage row must name the configured gateway/model: {rows:?}"
    );
    assert!(
        tasks.iter().any(|task| task.status.is_terminal()),
        "at least one task must reach a terminal status: {tasks:?}"
    );

    // The HTTP surface must reflect the same durable state (dashboard, tasks, usage).
    let router = nc_code_orchestrator::api::router(runtime.api_state());
    let (status, health) = call(&router, "/api/nc-ag/health").await;
    assert_eq!(status, 200, "{health}");
    assert_eq!(health["provider"]["configured"], json!(true), "{health}");
    let (status, dashboard) = call(&router, "/api/nc-ag/dashboard").await;
    assert_eq!(status, 200, "{dashboard}");
    assert_eq!(dashboard["auditVerified"], json!(true), "{dashboard}");
    let (status, tasks_view) = call(
        &router,
        &format!("/api/nc-ag/tasks?projectId={}", project.id),
    )
    .await;
    assert_eq!(status, 200, "{tasks_view}");
    assert_eq!(
        tasks_view["tasks"].as_array().map(Vec::len),
        Some(tasks.len()),
        "{tasks_view}"
    );
    let (status, usage_view) = call(
        &router,
        &format!("/api/nc-ag/usage?projectId={}", project.id),
    )
    .await;
    assert_eq!(status, 200, "{usage_view}");
    assert_eq!(
        usage_view["usage"]["inputTokens"],
        json!(usage.input_tokens),
        "{usage_view}"
    );

    let delivery = runtime
        .orchestrator
        .delivery(&request.id)
        .expect("delivery");
    eprintln!(
        "live NC-AG: request={} tasks={} usage={:?} delivery={}",
        request_now.status.as_str(),
        tasks.len(),
        usage,
        delivery.status
    );
    // Completion is the product contract; anything else is reported, not hidden.
    assert!(
        request_now.status == RequestStatus::Completed,
        "the live request must complete; tasks={:?}",
        tasks
            .iter()
            .map(|task| (
                task.title.clone(),
                task.status.as_str(),
                task.blocker.clone()
            ))
            .collect::<Vec<_>>()
    );
    assert!(
        delivery.status.starts_with("completed"),
        "{}",
        delivery.status
    );
    for task in &tasks {
        assert!(
            !runtime
                .store
                .evidence_for_task(&task.id)
                .unwrap_or_default()
                .is_empty(),
            "task {} must have evidence rows",
            task.id
        );
    }
}

fn request_has_no_tasks(runtime: &nc_code_orchestrator::NcagRuntime, request_id: &str) -> bool {
    runtime
        .store
        .tasks_for_request(request_id)
        .map(|tasks| tasks.is_empty())
        .unwrap_or(true)
}
