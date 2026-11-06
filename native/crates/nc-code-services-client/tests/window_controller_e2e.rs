//! `window-controller` end-to-end. The Controller owns no task data — it routes into the
//! real `nc_code_services_agent::TaskService` (`nc-code-task`) and drives a real V4 projection.
//!
//! Everything here is real: a temp-dir task-index + session `meta.json` seed the task
//! facade's on-disk state, calls go through the actual `ChannelRegistry`, and controller
//! frames are delivered over the actual `onDynamicControllerFrame` broadcast subscription.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelRegistry, ClientMode};
use nc_code_rpc::WireValue;
use nc_code_service_core::AppPaths;
use nc_code_services_agent::AgentServices;
use nc_code_services_client::ClientServices;
use nc_code_services_provider::ProviderStore;
use serde_json::{json, Value};

const WORKSPACE: &str = "G:/seed-ws";
const TASK_ID: &str = "t1";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn temp_root(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "zcode-window-controller-{}-{tag}-{}",
        std::process::id(),
        now_ms()
    ));
    std::fs::create_dir_all(&root).expect("root");
    root
}

/// Seed the two files the task facade reads: a membership row and a session `meta.json`.
fn seed_state(paths: &AppPaths) {
    let config_dir = paths.app_config_dir();
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let row = json!({
        "taskId": TASK_ID,
        "workspacePath": WORKSPACE,
        "pinned": false,
        "archived": false,
        "updatedAt": 120
    });
    let index = json!({ "version": 1, "tasks": { TASK_ID: row } });
    std::fs::write(
        config_dir.join("tasks-index-rust-v1.json"),
        index.to_string(),
    )
    .expect("index");

    // data_dir_for(workspace, key) = {app_config}/agent/{hash}/sessions ; one session dir.
    let key = WORKSPACE;
    let hash = paths.workspace_hash(WORKSPACE, Some(key));
    let session_dir = config_dir
        .join("agent")
        .join(hash)
        .join("sessions")
        .join(TASK_ID);
    std::fs::create_dir_all(&session_dir).expect("session dir");
    std::fs::write(
        session_dir.join("meta.json"),
        json!({
            "sessionId": TASK_ID,
            "title": "Seeded task",
            "createdAt": 100,
            "updatedAt": 120
        })
        .to_string(),
    )
    .expect("meta");
}

struct Harness {
    registry: Arc<ChannelRegistry>,
    _client: ClientServices,
    root: std::path::PathBuf,
}

impl Harness {
    fn new(tag: &str) -> Self {
        let root = temp_root(tag);
        let paths = AppPaths::new(root.join("home"), root.join("data"));
        let agent = AgentServices::new(paths.clone(), ProviderStore::new(paths.clone()));
        seed_state(&paths);
        let remotes = nc_code_services_client::RemoteConnectionRegistry::new();
        let client = ClientServices::new(Arc::clone(&agent.tasks), Arc::clone(&remotes));
        let registry = Arc::new(ChannelRegistry::new());
        client.register(&registry).expect("register");
        Self {
            registry,
            _client: client,
            root,
        }
    }

    fn cleanup(&self) {
        std::fs::remove_dir_all(&self.root).ok();
    }

    fn channel(&self, name: &str) -> Arc<dyn Channel> {
        self.registry.get(name).expect("channel registered")
    }

    async fn call(&self, channel: &str, method: &str, params: Value) -> Result<Value, String> {
        let ctx = ChannelContext::new("test", ClientMode::DesktopContinuous);
        match self
            .channel(channel)
            .call(
                &ctx,
                method,
                vec![WireValue::Json(params)],
                CancelToken::new(),
            )
            .await
        {
            Ok(WireValue::Json(value)) => Ok(value),
            Ok(WireValue::Undefined) => Ok(Value::Null),
            Ok(other) => Ok(Value::String(
                other.as_str().unwrap_or_default().to_string(),
            )),
            Err(failure) => Err(format!("{}", failure)),
        }
    }

    async fn frame_listener(&self) -> nc_code_channels::EventSubscription {
        let ctx = ChannelContext::new("test", ClientMode::DesktopContinuous);
        self.channel("window-controller")
            .listen(&ctx, "onDynamicControllerFrame", WireValue::Undefined)
            .await
            .expect("dynamic event subscription")
    }
}

async fn next_frame(subscription: &mut nc_code_channels::EventSubscription) -> Option<Value> {
    let value = tokio::time::timeout(Duration::from_secs(5), subscription.next())
        .await
        .ok()
        .flatten();
    match value {
        Some(WireValue::Json(value)) => Some(value),
        Some(other) => Some(Value::String(
            other.as_str().unwrap_or_default().to_string(),
        )),
        None => None,
    }
}

fn list_task_params() -> Value {
    json!({
        "kind": "timeline",
        "sortBy": "updated",
        "workspaceScopes": [{ "workspacePath": WORKSPACE }]
    })
}

#[tokio::test]
async fn client_services_registers_exactly_the_three_channels() {
    let harness = Harness::new("register");
    let mut names = harness.registry.names();
    names.sort();
    assert_eq!(
        names,
        vec!["client-config", "client-scenes", "window-controller"]
    );
    harness.cleanup();
}

#[tokio::test]
async fn subscribe_emits_a_real_snapshot_frame_with_the_seeded_task() {
    let harness = Harness::new("subscribe");
    // Register the local scope so the projection has rows before subscribing.
    let listed = harness
        .call("window-controller", "listTaskList", list_task_params())
        .await
        .expect("list");
    let items = listed["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "{listed}");
    assert_eq!(items[0]["taskId"], json!(TASK_ID));
    assert_eq!(items[0]["sourceAvailability"], json!("online"));
    assert_eq!(items[0]["liveStatus"], json!("idle"));
    assert_eq!(listed["total"], json!(1));

    let mut frames = harness.frame_listener().await;
    let ack = harness
        .call(
            "window-controller",
            "subscribeControllerV4",
            json!({ "topic": "controller/tasks-index" }),
        )
        .await
        .expect("subscribe");
    assert_eq!(ack["ack"]["mode"], json!("snapshot"));
    assert!(
        ack["ack"]["logEpoch"]
            .as_str()
            .is_some_and(|epoch| !epoch.is_empty()),
        "epoch must be a non-empty string: {ack}"
    );
    assert!(ack["ack"]["subscriptionId"]
        .as_str()
        .is_some_and(|id| !id.is_empty()));

    let frame = next_frame(&mut frames).await.expect("snapshot frame");
    assert_eq!(frame["topic"], json!("controller/tasks-index"));
    assert_eq!(frame["subscriptionId"], ack["ack"]["subscriptionId"]);
    assert_eq!(frame["fromSeq"], json!(0));
    assert_eq!(frame["payload"]["kind"], json!("snapshot"));
    assert_eq!(frame["payload"]["snapshot"]["protocolVersion"], json!(1));
    let tasks = frame["payload"]["snapshot"]["tasks"]
        .as_array()
        .expect("tasks");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["membership"]["pinned"], json!(false));
    assert_eq!(tasks[0]["sourceAvailability"], json!("online"));
    // Local addresses never carry remoteSessionId.
    assert!(tasks[0]["address"].get("remoteSessionId").is_none());

    harness.cleanup();
}

#[tokio::test]
async fn mutate_task_pin_writes_through_and_emits_an_upsert_delta() {
    let harness = Harness::new("mutate");
    harness
        .call("window-controller", "listTaskList", list_task_params())
        .await
        .expect("prime list");
    let mut frames = harness.frame_listener().await;
    let ack = harness
        .call(
            "window-controller",
            "subscribeControllerV4",
            json!({ "topic": "controller/tasks-index" }),
        )
        .await
        .expect("subscribe");
    next_frame(&mut frames).await.expect("snapshot frame");

    let meta = harness
        .call(
            "window-controller",
            "mutateTask",
            json!({
                "address": { "workspacePath": WORKSPACE, "taskId": TASK_ID },
                "mutation": { "kind": "pin", "pinned": true }
            }),
        )
        .await
        .expect("mutate");
    // mutateTask returns the task meta (ZCodeTaskMeta), which has no `pinned` field —
    // pinning is membership, observed on the delta frame below.
    assert_eq!(meta["taskId"], json!(TASK_ID));

    let delta = next_frame(&mut frames).await.expect("delta frame");
    assert_eq!(delta["payload"]["kind"], json!("deltas"));
    let deltas = delta["payload"]["deltas"].as_array().expect("deltas");
    let upsert = deltas
        .iter()
        .find(|d| d["op"] == json!("task.upserted"))
        .expect("task.upserted");
    assert_eq!(upsert["task"]["membership"]["pinned"], json!(true));

    // The write is real: a fresh list now shows the task under `pinned` membership.
    let pinned = harness
        .call(
            "window-controller",
            "listTaskList",
            json!({
                "kind": "pinned",
                "sortBy": "updated",
                "workspaceScopes": [{ "workspacePath": WORKSPACE }]
            }),
        )
        .await
        .expect("pinned list");
    assert_eq!(pinned["items"].as_array().expect("items").len(), 1);

    assert_eq!(
        ack["ack"]["mode"],
        json!("snapshot"),
        "subscribe mode is always snapshot (TS parity)"
    );
    harness.cleanup();
}

#[tokio::test]
async fn delete_returns_null_and_remove_delta() {
    let harness = Harness::new("delete");
    harness
        .call("window-controller", "listTaskList", list_task_params())
        .await
        .expect("prime");
    let mut frames = harness.frame_listener().await;
    harness
        .call(
            "window-controller",
            "subscribeControllerV4",
            json!({ "topic": "controller/tasks-index" }),
        )
        .await
        .expect("subscribe");
    next_frame(&mut frames).await.expect("snapshot");

    let result = harness
        .call(
            "window-controller",
            "mutateTask",
            json!({
                "address": { "workspacePath": WORKSPACE, "taskId": TASK_ID },
                "mutation": { "kind": "delete" }
            }),
        )
        .await
        .expect("delete");
    // A deleted task leaves the projection → null, matching `?.meta ?? null`.
    assert_eq!(result, Value::Null);

    let delta = next_frame(&mut frames).await.expect("delta");
    let deltas = delta["payload"]["deltas"].as_array().expect("deltas");
    assert!(deltas.iter().any(|d| d["op"] == json!("task.removed")));
    harness.cleanup();
}

#[tokio::test]
async fn delete_archived_helpers_route_through_the_task_facade() {
    let harness = Harness::new("archived");
    harness
        .call("window-controller", "listTaskList", list_task_params())
        .await
        .expect("prime");
    let deleted = harness
        .call(
            "window-controller",
            "deleteArchivedTask",
            json!({ "address": { "workspacePath": WORKSPACE, "taskId": TASK_ID } }),
        )
        .await
        .expect("deleteArchivedTask");
    // Not archived → the facade reports nothing deleted, and the channel surfaces false.
    assert_eq!(deleted, json!(false));

    let batch = harness
        .call(
            "window-controller",
            "deleteArchivedTasks",
            json!({
                "address": { "workspacePath": WORKSPACE, "taskId": TASK_ID },
                "taskIds": []
            }),
        )
        .await
        .expect("deleteArchivedTasks");
    assert_eq!(
        batch,
        json!({ "deletedTaskIds": [], "skippedTaskIds": [], "failedTaskIds": [] })
    );
    harness.cleanup();
}

#[tokio::test]
async fn resync_and_unsubscribe_and_decode_failures() {
    let harness = Harness::new("lifecycle");
    let mut frames = harness.frame_listener().await;
    let ack = harness
        .call(
            "window-controller",
            "subscribeControllerV4",
            json!({ "topic": "controller/workspaces" }),
        )
        .await
        .expect("subscribe");
    let subscription_id = ack["ack"]["subscriptionId"]
        .as_str()
        .expect("id")
        .to_string();
    next_frame(&mut frames).await.expect("snapshot frame");

    let resync = harness
        .call(
            "window-controller",
            "resyncControllerV4",
            json!({ "subscriptionId": subscription_id }),
        )
        .await
        .expect("resync");
    assert_eq!(resync["ack"]["mode"], json!("snapshot"));
    assert_eq!(resync["ack"]["subscriptionId"], json!(subscription_id));
    // resync re-emits a snapshot frame on the same subscription.
    let refired = next_frame(&mut frames).await.expect("resync frame");
    assert_eq!(refired["payload"]["kind"], json!("snapshot"));
    assert_eq!(refired["topic"], json!("controller/workspaces"));

    let gone = harness
        .call(
            "window-controller",
            "resyncControllerV4",
            json!({ "subscriptionId": "does-not-exist" }),
        )
        .await
        .expect_err("unknown subscription must fail, no fabricated ack");
    assert!(gone.contains("subscription"), "{gone}");

    let unsubscribed = harness
        .call(
            "window-controller",
            "unsubscribeControllerV4",
            json!({ "subscriptionId": subscription_id }),
        )
        .await
        .expect("unsubscribe");
    assert_eq!(unsubscribed, Value::Null, "unsubscribe returns void");
    // After unsubscribe, resync of the same id no longer resolves.
    harness
        .call(
            "window-controller",
            "resyncControllerV4",
            json!({ "subscriptionId": subscription_id }),
        )
        .await
        .expect_err("unsubscribed id gone");
    harness.cleanup();
}

#[tokio::test]
async fn remote_addresses_and_bad_topics_fail_closed() {
    let harness = Harness::new("failclosed");
    // Unknown remote session (with identity) fails via the registry's named error.
    let remote = harness
        .call(
            "window-controller",
            "mutateTask",
            json!({
                "address": { "workspacePath": WORKSPACE, "workspaceIdentity": "id-unknown", "taskId": TASK_ID, "remoteSessionId": "r-unknown" },
                "mutation": { "kind": "pin", "pinned": true }
            }),
        )
        .await
        .expect_err("remote rejected");
    assert!(remote.contains("RemoteSourceUnavailable"), "{remote}");
    // Remote without workspaceIdentity is a schema validation failure.
    let missing_identity = harness
        .call(
            "window-controller",
            "mutateTask",
            json!({
                "address": { "workspacePath": WORKSPACE, "taskId": TASK_ID, "remoteSessionId": "r" },
                "mutation": { "kind": "pin", "pinned": true }
            }),
        )
        .await
        .expect_err("remote without identity");
    assert!(
        missing_identity.contains("workspaceIdentity") || missing_identity.contains("ZodError"),
        "{missing_identity}"
    );

    let bad_topic = harness
        .call(
            "window-controller",
            "subscribeControllerV4",
            json!({ "topic": "controller/nope" }),
        )
        .await
        .expect_err("bad topic");
    assert!(
        bad_topic.contains("topic") || bad_topic.contains("ZodError"),
        "{bad_topic}"
    );

    let no_mutation = harness
        .call(
            "window-controller",
            "mutateTask",
            json!({ "address": { "workspacePath": WORKSPACE, "taskId": TASK_ID } }),
        )
        .await
        .expect_err("mutation required");
    assert!(no_mutation.contains("mutation"), "{no_mutation}");

    let unknown_method = harness
        .call("window-controller", "bogusMethod", json!({}))
        .await
        .expect_err("unknown method");
    assert!(
        unknown_method.contains("Method not found"),
        "{unknown_method}"
    );

    // listen() rejects events other than the declared dynamic frame event.
    let ctx = ChannelContext::new("test", ClientMode::DesktopContinuous);
    assert!(harness
        .channel("window-controller")
        .listen(&ctx, "onOther", WireValue::Undefined)
        .await
        .is_err());
    harness.cleanup();
}

#[test]
fn seed_helpers_do_not_escape_temp_dir() {
    // Guard the path math used by every e2e above.
    let root = temp_root("path");
    let paths = AppPaths::new(root.join("home"), root.join("data"));
    let config_dir: &Path = &paths.app_config_dir();
    assert!(config_dir.starts_with(&root), "{config_dir:?}");
    std::fs::remove_dir_all(root).ok();
}
