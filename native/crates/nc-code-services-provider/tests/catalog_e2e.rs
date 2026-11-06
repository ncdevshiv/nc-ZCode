//! Catalog + probe integration against a local mock OpenAI-compatible endpoint.
//!
//! The probe path runs the product's own harness (`nc-code-provider::executor`), so these
//! tests pin what the Auto fetch panel shows as `measured` facts: tool-call output, a
//! reasoning channel, throughput, and honest failure for an unavailable model.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ClientMode};
use nc_code_rpc::WireValue;
use nc_code_service_core::AppPaths;
use nc_code_services_provider::catalog::{self, Endpoint};
use nc_code_services_provider::{ProviderSettingsService, ProviderStore};
use serde_json::{json, Value};
use tokio::net::TcpListener;

#[derive(Clone)]
struct Mock {
    chats: Arc<AtomicUsize>,
}

async fn models() -> Json<Value> {
    Json(json!({
        "object": "list",
        "data": [
            {
                "id": "mock/toolthink",
                "object": "model",
                "owned_by": "mock",
                "display_name": "Tool Think",
                "context_length": 4096,
                "supports_tools": true,
                "reasoning_levels": ["low", "high"],
                "supported_modalities": ["text", "image"]
            },
            {"id": "mock/plain-instruct", "object": "model", "owned_by": "mock"},
            {"id": "mock/broken", "object": "model", "owned_by": "mock"}
        ]
    }))
}

fn frame(payload: Value) -> Event {
    Event::default().data(payload.to_string())
}

async fn chat(State(mock): State<Mock>, Json(body): Json<Value>) -> axum::response::Response {
    let model = body.get("model").and_then(Value::as_str).unwrap_or("");
    mock.chats.fetch_add(1, Ordering::SeqCst);
    if model == "mock/broken" {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": {"message": "model is overloaded"}})),
        )
            .into_response();
    }
    // Only models that support tools return tool_calls; other models return ordinary text to verify that the detection is true and return false.
    let wants_tools =
        body.get("tools").and_then(Value::as_array).is_some() && model == "mock/toolthink";
    let mut frames: Vec<Event> = Vec::new();
    let push = |frames: &mut Vec<Event>, delta: Value, finish: Value, usage: Option<Value>| {
        let mut payload = json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion.chunk",
            "model": model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
        });
        if let Some(usage) = usage {
            payload["usage"] = usage;
        }
        frames.push(frame(payload));
    };
    if wants_tools {
        push(
            &mut frames,
            json!({"role": "assistant", "tool_calls": [{
                "index": 0,
                "id": "call-1",
                "type": "function",
                "function": {"name": "get_weather", "arguments": ""}
            }]}),
            Value::Null,
            None,
        );
        push(
            &mut frames,
            json!({"tool_calls": [{
                "index": 0,
                "id": "call-1",
                "type": "function",
                "function": {"arguments": "{\"city\":\"Paris\"}"}
            }]}),
            json!("tool_calls"),
            Some(json!({"prompt_tokens": 20, "completion_tokens": 9, "total_tokens": 29})),
        );
    } else if model == "mock/toolthink" {
        push(
            &mut frames,
            json!({"role": "assistant", "reasoning_content": "thinking about it"}),
            Value::Null,
            None,
        );
        push(
            &mut frames,
            json!({"content": "OK"}),
            json!("stop"),
            Some(json!({"prompt_tokens": 12, "completion_tokens": 30, "total_tokens": 42})),
        );
    } else {
        push(
            &mut frames,
            json!({"role": "assistant", "content": "OK"}),
            Value::Null,
            None,
        );
        push(
            &mut frames,
            json!({}),
            json!("stop"),
            Some(json!({"prompt_tokens": 12, "completion_tokens": 4, "total_tokens": 16})),
        );
    }
    frames.push(Event::default().data("[DONE]"));
    Sse::new(stream::iter(
        frames.into_iter().map(Ok::<_, std::io::Error>),
    ))
    .into_response()
}

async fn spawn_mock() -> (String, Mock) {
    let state = Mock {
        chats: Arc::new(AtomicUsize::new(0)),
    };
    let app = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (format!("http://{address}/v1"), state)
}

fn endpoint(base_url: &str) -> Endpoint {
    Endpoint {
        base_url: base_url.to_string(),
        api_type: "openai-chat-completions".to_string(),
        api_key: "mock-key".to_string(),
    }
}

fn temp_root(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "zcode-catalog-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0)
    ))
}

async fn call_service(
    service: &Arc<ProviderSettingsService>,
    command: &str,
    params: Value,
) -> Value {
    let ctx = ChannelContext::new("test-connection", ClientMode::DesktopContinuous);
    let value = service
        .call(
            &ctx,
            command,
            vec![WireValue::Json(params)],
            CancelToken::new(),
        )
        .await
        .unwrap_or_else(|error| panic!("{command} failed: {:?}", error.payload()));
    match value {
        WireValue::Json(value) => value,
        other => panic!("{command} returned {other:?}"),
    }
}

#[tokio::test]
async fn catalog_fetch_normalizes_catalog_and_heuristic_entries() {
    let (base, _mock) = spawn_mock().await;
    let models = catalog::fetch_catalog(&endpoint(&base))
        .await
        .expect("catalog");
    assert_eq!(models.len(), 3);
    let toolthink = models
        .iter()
        .find(|model| model["modelId"] == json!("mock/toolthink"))
        .expect("entry");
    assert_eq!(toolthink["confidence"], json!("catalog"));
    assert_eq!(toolthink["contextWindow"], json!(4096));
    assert_eq!(toolthink["input"]["image"], json!(true));
    assert_eq!(toolthink["reasoningLevels"], json!(["low", "high"]));
    assert_eq!(toolthink["displayName"], json!("Tool Think"));
    let plain = models
        .iter()
        .find(|model| model["modelId"] == json!("mock/plain-instruct"))
        .expect("entry");
    assert_eq!(plain["confidence"], json!("heuristic"));
    assert_eq!(plain["supportsToolCall"], json!(true));
}

#[tokio::test]
async fn probe_reports_measured_capability_and_honest_failure() {
    let (base, mock) = spawn_mock().await;
    let toolthink = catalog::probe_model(
        &endpoint(&base),
        "mock/toolthink",
        &[
            "toolCall".to_string(),
            "reasoning".to_string(),
            "throughput".to_string(),
        ],
    )
    .await
    .expect("probe");
    assert_eq!(toolthink["connectivity"]["ok"], json!(true));
    assert_eq!(toolthink["toolCall"]["ok"], json!(true));
    assert_eq!(toolthink["toolCall"]["toolName"], json!("get_weather"));
    assert_eq!(toolthink["reasoning"]["ok"], json!(true));
    assert_eq!(toolthink["throughput"]["ok"], json!(true));
    assert_eq!(toolthink["throughput"]["completionTokens"], json!(30));
    assert_eq!(toolthink["throughput"]["estimated"], json!(false));
    assert!(
        toolthink["throughput"]["tokensPerSecond"]
            .as_f64()
            .unwrap_or(0.0)
            > 0.0,
        "measured throughput must be positive: {toolthink}"
    );

    let plain = catalog::probe_model(
        &endpoint(&base),
        "mock/plain-instruct",
        &["toolCall".to_string(), "reasoning".to_string()],
    )
    .await
    .expect("probe plain");
    assert_eq!(plain["toolCall"]["ok"], json!(false));
    assert_eq!(plain["reasoning"]["ok"], json!(false));

    let broken = catalog::probe_model(&endpoint(&base), "mock/broken", &["throughput".to_string()])
        .await
        .expect("probe broken");
    assert_eq!(broken["connectivity"]["ok"], json!(false));
    assert_eq!(broken["skippedChecks"], json!(["throughput"]));
    assert!(mock.chats.load(Ordering::SeqCst) >= 4);
}

#[tokio::test]
async fn settings_service_lists_adds_and_removes_catalog_models() {
    let (base, _mock) = spawn_mock().await;
    let root = temp_root("service");
    let paths = AppPaths::new(root.join("home"), root.join("data"));
    let store = ProviderStore::new(paths.clone());
    let service = ProviderSettingsService::new(store, nc_code_channels::EventBus::default());

    let created = call_service(
        &service,
        "createPersonalProvider",
        json!({
            "providerName": "Mock",
            "initialConfig": {
                "group": "standard-personal",
                "visibility": "visible",
                "api": {"type": "openai-chat-completions", "baseUrl": base},
                "access": {"type": "api-key", "apiKey": "mock-key"}
            }
        }),
    )
    .await;
    let provider_id = created["providerId"]
        .as_str()
        .expect("providerId")
        .to_string();

    let listed = call_service(
        &service,
        "listRemoteModels",
        json!({"providerId": provider_id}),
    )
    .await;
    assert_eq!(listed["total"], json!(3));
    assert!(listed["models"]
        .as_array()
        .expect("models")
        .iter()
        .all(|model| model["alreadyAdded"] == json!(false)));

    let added = call_service(
        &service,
        "addRemoteModels",
        json!({
            "providerId": provider_id,
            "modelIds": ["mock/toolthink", "mock/plain-instruct"]
        }),
    )
    .await;
    let provider = added["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .find(|provider| provider["providerId"] == json!(provider_id))
        .expect("provider");
    let models = provider["models"].as_array().expect("models");
    assert_eq!(models.len(), 2);
    let toolthink = models
        .iter()
        .find(|model| model["modelId"] == json!("mock/toolthink"))
        .expect("model");
    assert_eq!(
        toolthink["effectiveConfig"]["properties"]["contextWindow"],
        json!(4096),
        "catalog context length must land in the stored model config"
    );
    assert_eq!(
        toolthink["effectiveConfig"]["optionSpecs"]["reasoningLevel"]["values"],
        json!(["low", "high"])
    );

    let relisted = call_service(
        &service,
        "listRemoteModels",
        json!({"providerId": provider_id}),
    )
    .await;
    let entry = relisted["models"]
        .as_array()
        .expect("models")
        .iter()
        .find(|model| model["modelId"] == json!("mock/toolthink"))
        .expect("entry");
    assert_eq!(entry["alreadyAdded"], json!(true));
    assert_eq!(entry["enabled"], json!(true));

    let removed = call_service(
        &service,
        "deletePersonalModels",
        json!({"providerId": provider_id, "modelIds": ["mock/plain-instruct"]}),
    )
    .await;
    let provider = removed["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .find(|provider| provider["providerId"] == json!(provider_id))
        .expect("provider");
    assert_eq!(provider["models"].as_array().expect("models").len(), 1);

    let probed = call_service(
        &service,
        "probeRemoteModel",
        json!({"providerId": provider_id, "modelId": "mock/toolthink", "checks": ["reasoning"]}),
    )
    .await;
    assert_eq!(probed["reasoning"]["ok"], json!(true));

    std::fs::remove_dir_all(root).ok();
}
