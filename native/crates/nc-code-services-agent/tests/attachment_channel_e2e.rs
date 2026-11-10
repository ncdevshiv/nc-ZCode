//! The attachment command surface, end to end at the channel boundary: the same six
//! methods the UI calls (`attachmentBeginV4`…`attachmentPreviewSourceV4`) plus `sendPrompt`
//! with inline composer bytes. The assertions land on the durable artefacts — the
//! content-addressed file in the session store and the projected `userInput` row in the
//! transcript — never on internal structures.
//!
//! The provider endpoint is deliberately unreachable (`127.0.0.1:9`): the attachment
//! transaction and the command admission must work without a model, and a failed turn is
//! still recorded with the reference the user attached.

use std::sync::Arc;

use nc_code_channels::{CancelToken, Channel, ChannelContext, ClientMode};
use nc_code_rpc::WireValue;
use nc_code_service_core::AppPaths;
use nc_code_services_agent::service::AgentService;
use nc_code_services_provider::store::{
    default_model_config, ModelRecord, ProviderRecord, ProviderStore,
};
use serde_json::{json, Value};

fn temp_base(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "zcode-agent-attach-e2e-{}-{tag}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&root).expect("root");
    root
}

async fn service(tag: &str) -> (Arc<AgentService>, AppPaths, std::path::PathBuf) {
    let root = temp_base(tag);
    let paths = AppPaths::new(root.join("home"), root.join("data"));
    let store = ProviderStore::new(paths.clone());
    store
        .mutate(|snapshot| {
            snapshot.providers.push(ProviderRecord {
                provider_id: "personal:1".to_string(),
                provider_name: "Test Provider".to_string(),
                template_id: None,
                enabled: true,
                config: json!({
                    "group": "standard-personal",
                    "visibility": "visible",
                    "api": {"type": "openai-chat-completions", "baseUrl": "http://127.0.0.1:9/v1"},
                    "access": {"type": "api-key", "apiKey": "test-key"}
                }),
                models: vec![ModelRecord {
                    model_id: "test-model".to_string(),
                    config: default_model_config(),
                    enabled: true,
                    use_recommended: true,
                }],
            });
            snapshot.provider_order.push("personal:1".to_string());
            Ok(())
        })
        .await
        .expect("store");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let agent = AgentService::new(paths.clone(), store);
    (agent, paths, workspace)
}

fn ctx() -> ChannelContext {
    ChannelContext::new("conn-attachment", ClientMode::DesktopContinuous)
}

async fn call(service: &Arc<AgentService>, method: &str, params: Value) -> Value {
    let result = service
        .call(
            &ctx(),
            method,
            vec![WireValue::Json(params)],
            CancelToken::new(),
        )
        .await
        .unwrap_or_else(|error| panic!("{method} must succeed: {error:?}"));
    match result {
        WireValue::Json(value) => value,
        other => panic!("expected JSON, got {other:?}"),
    }
}

/// `<data>/agent/<workspace-hash>/sessions/<session>/attachments/<sha256>` — the host's
/// single content store for this workspace runtime.
fn attachment_file(
    paths: &AppPaths,
    workspace: &str,
    session_id: &str,
    sha: &str,
) -> std::path::PathBuf {
    paths
        .app_config_dir()
        .join("agent")
        .join(paths.workspace_hash(workspace, Some(workspace)))
        .join("sessions")
        .join(session_id)
        .join("attachments")
        .join(sha)
}

fn transcript_file(paths: &AppPaths, workspace: &str, session_id: &str) -> std::path::PathBuf {
    paths
        .app_config_dir()
        .join("agent")
        .join(paths.workspace_hash(workspace, Some(workspace)))
        .join("sessions")
        .join(session_id)
        .join("transcript.jsonl")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_upload_transaction_round_trips_through_the_agent_channel() {
    let (service, paths, workspace) = service("upload").await;
    let workspace = workspace.to_string_lossy().to_string();
    let created = call(
        &service,
        "createSession",
        json!({"workspacePath": workspace}),
    )
    .await;
    let session_id = created["session"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    let bytes = b"ATTACHMENT-CHANNEL-E2E";
    let sha = nc_code_host::content_hash::sha256_hex(bytes);
    let checksum = format!("sha256:{sha}");
    let base64 = nc_code_host::tools::read_image::base64_encode(bytes);

    let begun = call(
        &service,
        "attachmentBeginV4",
        json!({
            "workspacePath": workspace,
            "sessionId": session_id,
            "uploadId": "upload-e2e-1",
            "fileName": "note.txt",
            "mime": "text/plain",
            "totalBytes": bytes.len(),
            "totalChunks": 1,
            "checksum": checksum,
        }),
    )
    .await;
    assert_eq!(begun["state"], json!("staging"), "{begun}");

    let chunked = call(
        &service,
        "attachmentChunkV4",
        json!({
            "workspacePath": workspace,
            "sessionId": session_id,
            "uploadId": "upload-e2e-1",
            "chunkIndex": 0,
            "dataBase64": base64,
        }),
    )
    .await;
    assert_eq!(chunked["nextChunkIndex"], json!(1), "{chunked}");

    let committed = call(
        &service,
        "attachmentCommitV4",
        json!({
            "workspacePath": workspace,
            "sessionId": session_id,
            "uploadId": "upload-e2e-1",
        }),
    )
    .await;
    let ref_value = committed["ref"].as_str().expect("committed ref");
    assert_eq!(
        ref_value,
        format!("zcode-artifact://{sha}"),
        "the ref is content-addressed: {committed}"
    );
    // The bytes are really on disk behind the ref — not an in-memory claim.
    let stored = attachment_file(&paths, &workspace, &session_id, &sha);
    assert_eq!(
        std::fs::read(&stored).expect("stored attachment bytes"),
        bytes.to_vec(),
        "{}",
        stored.display()
    );

    // A commit without a staged upload still fails closed.
    let aborted = service
        .call(
            &ctx(),
            "attachmentCommitV4",
            vec![WireValue::Json(json!({
                "workspacePath": workspace,
                "sessionId": session_id,
                "uploadId": "upload-never-begun",
            }))],
            CancelToken::new(),
        )
        .await;
    assert!(aborted.is_err(), "an unbegun upload must fail closed");
}

/// `sendPrompt` with inline composer bytes uploads through the real transaction and
/// forwards a content-addressed `AttachmentRef` with `sendText`; the durable `userInput`
/// row carries that ref (the same object the model resolver reads).
/// A local-path attachment is forwarded as-is (zero upload — the desktop zero-copy path),
/// and the model resolver reads the real file behind that absolute path.
#[tokio::test(flavor = "multi_thread")]
async fn local_path_attachments_pass_through_without_upload() {
    let (service, paths, workspace) = service("local-path").await;
    let workspace = workspace.to_string_lossy().to_string();
    let created = call(
        &service,
        "createSession",
        json!({"workspacePath": workspace}),
    )
    .await;
    let session_id = created["session"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    let file = std::env::temp_dir().join(format!(
        "zcode-local-attachment-{}-{}.png",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&file, b"LOCAL-PATH-BYTES").expect("write local attachment");
    let routed = service
        .call(
            &ctx(),
            "sendPrompt",
            vec![WireValue::Json(json!({
                "workspacePath": workspace,
                "sessionId": session_id,
                "content": "look at the screenshot",
                "inputId": "input-local-path-1",
                "attachments": [{
                    "kind": "image",
                    "filename": "pasted-screenshot.png",
                    "mimeType": "image/png",
                    "sizeBytes": 16,
                    "localPath": file.to_string_lossy(),
                }],
            }))],
            CancelToken::new(),
        )
        .await;
    let routed = match routed {
        Ok(WireValue::Json(value)) => value,
        other => panic!("sendPrompt with a local path must be routed, got {other:?}"),
    };
    assert_eq!(routed["accepted"], json!(true), "{routed}");
    let transcript = transcript_file(&paths, &workspace, &session_id);
    let mut row: Option<Value> = None;
    for _ in 0..200 {
        if let Ok(raw) = std::fs::read_to_string(&transcript) {
            for line in raw.lines() {
                let Ok(value) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if value.get("t").and_then(Value::as_str) == Some("row") {
                    let candidate = value.get("row").cloned().unwrap_or(Value::Null);
                    if candidate["kind"] == json!("userInput") {
                        row = Some(candidate);
                    }
                }
            }
        }
        if row.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let row = row.expect("a durable userInput row");
    let attachments = row["attachments"].as_array().cloned().unwrap_or_default();
    assert_eq!(attachments.len(), 1, "{row}");
    assert_eq!(
        attachments[0]["ref"],
        json!(file.to_string_lossy().to_string()),
        "the desktop local path travels unchanged (no upload): {row}"
    );
    // Zero upload: no content-addressed copy exists for this attachment.
    let sha = nc_code_host::content_hash::sha256_hex(b"LOCAL-PATH-BYTES");
    assert!(
        !attachment_file(&paths, &workspace, &session_id, &sha).exists(),
        "a local-path attachment must not be copied into the content store"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn send_prompt_maps_composer_attachments_to_references() {
    let (service, paths, workspace) = service("send").await;
    let workspace = workspace.to_string_lossy().to_string();
    let created = call(
        &service,
        "createSession",
        json!({"workspacePath": workspace}),
    )
    .await;
    let session_id = created["session"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();

    let routed = service
        .call(
            &ctx(),
            "sendPrompt",
            vec![WireValue::Json(json!({
                "workspacePath": workspace,
                "sessionId": session_id,
                "content": "describe this image",
                "inputId": "input-attachment-1",
                "attachments": [{
                    "kind": "image",
                    "filename": "tiny.png",
                    "mimeType": "image/png",
                    "sizeBytes": 3,
                    "dataBase64": nc_code_host::tools::read_image::base64_encode(b"abc"),
                }],
            }))],
            CancelToken::new(),
        )
        .await;
    let routed = match routed {
        Ok(WireValue::Json(value)) => value,
        other => panic!("sendPrompt with an attachment must be routed, got {other:?}"),
    };
    assert_eq!(routed["accepted"], json!(true), "{routed}");

    let sha = nc_code_host::content_hash::sha256_hex(b"abc");
    let stored = attachment_file(&paths, &workspace, &session_id, &sha);
    assert_eq!(
        std::fs::read(&stored).expect("uploaded inline bytes"),
        b"abc".to_vec(),
        "inline composer bytes are uploaded before the command: {}",
        stored.display()
    );

    let transcript = transcript_file(&paths, &workspace, &session_id);
    // The turn runs in the background right after admission, so the projected row lands
    // slightly after the ack: poll the durable file instead of assuming the ordering.
    let mut user_row: Option<Value> = None;
    for _ in 0..200 {
        if let Ok(raw) = std::fs::read_to_string(&transcript) {
            for line in raw.lines() {
                let Ok(value) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if value.get("t").and_then(Value::as_str) == Some("row") {
                    let row = value.get("row").cloned().unwrap_or(Value::Null);
                    if row["kind"] == json!("userInput") {
                        user_row = Some(row);
                    }
                }
            }
        }
        if user_row.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let user_row = user_row.unwrap_or_else(|| {
        let listing = std::fs::read_dir(transcript.parent().unwrap())
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let raw = std::fs::read_to_string(&transcript).unwrap_or_default();
        panic!(
            "a durable userInput row; dir={:?} entries={listing:?} transcript head={}",
            transcript.display(),
            raw.chars().take(600).collect::<String>()
        )
    });
    let attachments = user_row["attachments"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(attachments.len(), 1, "{user_row}");
    assert_eq!(
        attachments[0]["ref"],
        json!(format!("zcode-artifact://{sha}")),
        "the row carries the content-addressed ref: {user_row}"
    );
    assert_eq!(attachments[0]["mime"], json!("image/png"), "{user_row}");
}
