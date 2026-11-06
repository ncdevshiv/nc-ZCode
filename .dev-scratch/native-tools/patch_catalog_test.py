import io

path = 'crates/zcode-services-provider/tests/catalog_e2e.rs'
s = io.open(path, encoding='utf-8').read()

s = s.replace('''use tokio::net::TcpListener;
use tokio_stream::wrappers::ReceiverStream;
use zcode_channels::{CancelToken, Channel, ChannelContext, ClientMode};''','''use tokio::net::TcpListener;
use zcode_channels::{CancelToken, Channel, ChannelContext, ClientMode};''')

s = s.replace('''use axum::response::sse::{Event, Sse};''','')

s = s.replace('''async fn chat(
    State(mock): State<Mock>,
    body: Value,
) -> axum::response::Response {
    let model = body.get("model").and_then(Value::as_str).unwrap_or("");
    mock.chats.fetch_add(1, Ordering::SeqCst);
    if model == "mock/broken" {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": {"message": "model is overloaded"}})),
        )
            .into_response();
    }
    let wants_tools = body.get("tools").and_then(Value::as_array).is_some();
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(16);
    tokio::spawn(async move {
        let base = json!({"id": "chatcmpl-mock", "object": "chat.completion.chunk", "model": model});
        let mut emit = |delta: Value, finish: Option<&str>, usage: Option<Value>| {
            let mut payload = base.clone();
            payload["choices"] = json!([{"index": 0, "delta": delta, "finish_reason": finish}]);
            if let Some(usage) = usage {
                payload["usage"] = usage;
            }
            let _ = senderblocking_send(&sender, chunk(payload)).await;
        };
        if wants_tools {
            emit(
                json!({"role": "assistant", "tool_calls": [{
                    "index": 0,
                    "id": "call-1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": ""}
                }]}),
                None,
                None,
            );
            emit(
                json!({"tool_calls": [{
                    "index": 0,
                    "id": "call-1",
                    "type": "function",
                    "function": {"arguments": "{\\"city\\":\\"Paris\\"}"}
                }]}),
                Some("tool_calls"),
                Some(json!({"prompt_tokens": 20, "completion_tokens": 9, "total_tokens": 29})),
            );
        } else if model == "mock/toolthink" {
            emit(json!({"role": "assistant", "reasoning_content": "thinking…"}), None, None);
            emit(json!({"content": "OK"}), Some("stop"), Some(json!({
                "prompt_tokens": 12, "completion_tokens": 30, "total_tokens": 42
            })));
        } else {
            emit(json!({"role": "assistant", "content": "OK"}), None, None);
            emit(json!({}), Some("stop"), Some(json!({
                "prompt_tokens": 12, "completion_tokens": 4, "total_tokens": 16
            })));
        }
        let _ = sender.send(Ok(Event::default().data("[DONE]")));
    });
    Sse::new(ReceiverStream::new(receiver)).into_response()
}''','''fn chunk(payload: Value) -> String {
    format!("data: {}\\n\\n", payload)
}

async fn chat(State(mock): State<Mock>, body: Value) -> axum::response::Response {
    let model = body.get("model").and_then(Value::as_str).unwrap_or("");
    mock.chats.fetch_add(1, Ordering::SeqCst);
    if model == "mock/broken" {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": {"message": "model is overloaded"}})),
        )
            .into_response();
    }
    let wants_tools = body.get("tools").and_then(Value::as_array).is_some();
    let mut frames: Vec<String> = Vec::new();
    let arguments = "{\\"city\\":\\"Paris\\"}";
    if wants_tools {
        frames.push(chunk(json!({
            "id": "chatcmpl-mock", "object": "chat.completion.chunk", "model": model,
            "choices": [{"index": 0, "finish_reason": null, "delta": {"role": "assistant", "tool_calls": [{
                "index": 0, "id": "call-1", "type": "function",
                "function": {"name": "get_weather", "arguments": ""}
            }]}}]
        })));
        frames.push(chunk(json!({
            "id": "chatcmpl-mock", "object": "chat.completion.chunk", "model": model,
            "choices": [{"index": 0, "finish_reason": "tool_calls", "delta": {"tool_calls": [{
                "index": 0, "id": "call-1", "type": "function",
                "function": {"arguments": arguments}
            }]}}],
            "usage": {"prompt_tokens": 20, "completion_tokens": 9, "total_tokens": 29}
        })));
    } else if model == "mock/toolthink" {
        frames.push(chunk(json!({
            "id": "chatcmpl-mock", "object": "chat.completion.chunk", "model": model,
            "choices": [{"index": 0, "finish_reason": null, "delta": {
                "role": "assistant", "reasoning_content": "thinking about it"
            }}]
        })));
        frames.push(chunk(json!({
            "id": "chatcmpl-mock", "object": "chat.completion.chunk", "model": model,
            "choices": [{"index": 0, "finish_reason": "stop", "delta": {"content": "OK"}}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 30, "total_tokens": 42}
        })));
    } else {
        frames.push(chunk(json!({
            "id": "chatcmpl-mock", "object": "chat.completion.chunk", "model": model,
            "choices": [{"index": 0, "finish_reason": null, "delta": {
                "role": "assistant", "content": "OK"
            }}]
        })));
        frames.push(chunk(json!({
            "id": "chatcmpl-mock", "object": "chat.completion.chunk", "model": model,
            "choices": [{"index": 0, "finish_reason": "stop", "delta": {}}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 4, "total_tokens": 16}
        })));
    }
    frames.push("data: [DONE]\\n\\n".to_string());
    let stream = futures::stream::iter(frames.map(Ok::<_, std::io::Error>));
    Sse::new(stream).into_response()
}''')

# drop leftover helper fn chunk(Event) + Infallible import + StateAccess hack
s = s.replace('''fn chunk(payload: Value) -> Result<Event, Infallible> {
    Ok(Event::default().data(payload.to_string()))
}

''','')
s = s.replace('''use std::convert::Infallible;
''','')
s = s.replace('''async fn spawn_mock() -> (String, Mock) {
    let app = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .with_state(Mock {
            chats: Arc::new(AtomicUsize::new(0)),
        });
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    let state = app.all_state();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (format!("http://{address}/v1"), state)
}

trait StateAccess {
    fn all_state(&self) -> Mock;
}

impl StateAccess for Router {
    fn all_state(&self) -> Mock {
        Mock {
            chats: Arc::new(AtomicUsize::new(0)),
        }
    }
}''','''async fn spawn_mock() -> (String, Mock) {
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
}''')
io.open(path, 'w', encoding='utf-8', newline='').write(s)
print('rewritten test')
