//! `file-watcher` channel — per-path watchers with 150ms debounce.
//!
//! Events are `{dirPath, changedPath?}`; `changedPath` is present only when the
//! debounce window contains exactly one distinct path (TS semantics). Watchers are
//! process-local and released by `unwatch`/`disposeAll` or connection teardown.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nc_code_channels::{
    CancelToken, Channel, ChannelContext, ChannelFailure, EventBus, EventSubscription,
};
use nc_code_rpc::WireValue;
use nc_code_service_core::{ServiceArgs, ServiceError};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

const DEBOUNCE: Duration = Duration::from_millis(150);

struct WatcherEntry {
    bus: EventBus,
    watcher: RecommendedWatcher,
    task: JoinHandle<()>,
}

pub struct FileWatcherService {
    watchers: Mutex<std::collections::HashMap<String, WatcherEntry>>,
    next_id: AtomicU64,
}

impl FileWatcherService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            watchers: Mutex::new(std::collections::HashMap::new()),
            next_id: AtomicU64::new(0),
        })
    }

    async fn watch(&self, path: &str, recursive: bool) -> Result<Value, ServiceError> {
        if path.trim().is_empty() {
            return Err(ServiceError::invalid_params("params.path is required"));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let bus = EventBus::default();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if let Ok(event) = event {
                    for path in event.paths {
                        let _ = tx.send(path.to_string_lossy().into_owned());
                    }
                }
            })
            .map_err(|error| {
                ServiceError::new(format!("Unable to monitor directory '{path}': {error}"))
            })?;
        watcher
            .watch(
                std::path::Path::new(path),
                if recursive {
                    RecursiveMode::Recursive
                } else {
                    RecursiveMode::NonRecursive
                },
            )
            .map_err(|error| {
                ServiceError::new(format!("Unable to monitor directory '{path}': {error}"))
            })?;
        let dir_path = path.to_string();
        let bus_for_task = bus.clone();
        let task = tokio::spawn(async move {
            while let Some(first) = rx.recv().await {
                let mut paths: HashSet<String> = HashSet::from([first]);
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(DEBOUNCE) => break,
                        next = rx.recv() => match next {
                            Some(path) => {
                                paths.insert(path);
                            }
                            None => {
                                if !paths.is_empty() {
                                    fire(&bus_for_task, &dir_path, &paths);
                                }
                                return;
                            }
                        },
                    }
                }
                fire(&bus_for_task, &dir_path, &paths);
            }
        });
        self.watchers
            .lock()
            .await
            .insert(id.clone(), WatcherEntry { bus, watcher, task });
        Ok(json!({ "id": id }))
    }

    async fn unwatch(&self, id: &str) {
        let mut watchers = self.watchers.lock().await;
        if let Some(entry) = watchers.remove(id) {
            entry.task.abort();
            drop(entry.watcher);
        }
    }

    async fn dispose_all(&self) {
        let mut watchers = self.watchers.lock().await;
        for (_, entry) in watchers.drain() {
            entry.task.abort();
        }
    }

    async fn subscription(&self, id: &str) -> EventSubscription {
        let watchers = self.watchers.lock().await;
        match watchers.get(id) {
            Some(entry) => EventSubscription::from_broadcast(entry.bus.subscribe()),
            None => empty_subscription(),
        }
    }
}

fn fire(bus: &EventBus, dir_path: &str, paths: &HashSet<String>) {
    let mut event = json!({ "dirPath": dir_path });
    if paths.len() == 1 {
        if let Some(path) = paths.iter().next() {
            event["changedPath"] = Value::String(path.clone());
        }
    }
    bus.send(WireValue::Json(event));
}

fn empty_subscription() -> EventSubscription {
    let (_, receiver) = mpsc::channel(1);
    EventSubscription::from_receiver(receiver)
}

#[async_trait]
impl Channel for FileWatcherService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        match command {
            "watch" => {
                let params = args.required_json(0, "params")?;
                let path = params
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ServiceError::invalid_params("params.path is required"))?;
                let recursive = params
                    .get("recursive")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                Ok(WireValue::Json(self.watch(path, recursive).await?))
            }
            "unwatch" => {
                let params = args.required_json(0, "params")?;
                let id = params
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ServiceError::invalid_params("params.id is required"))?;
                self.unwatch(id).await;
                Ok(WireValue::Undefined)
            }
            "disposeAll" => {
                self.dispose_all().await;
                Ok(WireValue::Undefined)
            }
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }

    async fn listen(
        &self,
        _ctx: &ChannelContext,
        event: &str,
        arg: WireValue,
    ) -> Result<EventSubscription, ChannelFailure> {
        if event != "onDynamicChange" {
            return Err(ChannelFailure::event_not_found(event));
        }
        let id = match arg {
            WireValue::String(id) => id,
            WireValue::Json(Value::String(id)) => id,
            _ => return Ok(empty_subscription()),
        };
        Ok(self.subscription(&id).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::timeout;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zcode-watch-test-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[tokio::test]
    async fn emits_debounced_change_and_stops_after_unwatch() {
        let root = temp_dir("events");
        let service = FileWatcherService::new();
        let watched = service
            .call_inner_watch(&root.to_string_lossy(), false)
            .await;
        let id = watched["id"].as_str().expect("id").to_string();
        let mut subscription = service.subscription(&id).await;
        std::fs::write(root.join("a.txt"), "a").expect("write");
        let event = timeout(Duration::from_secs(5), subscription.next())
            .await
            .expect("watch event")
            .expect("event");
        let WireValue::Json(event) = event else {
            panic!("expected JSON event");
        };
        assert_eq!(event["dirPath"], json!(root.to_string_lossy()));
        service.unwatch(&id).await;
        assert!(
            service.watchers.lock().await.is_empty(),
            "unwatch removes it"
        );
        let mut subscription = service.subscription(&id).await;
        std::fs::write(root.join("b.txt"), "b").expect("write");
        if let Ok(Some(value)) = timeout(Duration::from_millis(500), subscription.next()).await {
            panic!("unwatch must stop events, got {value:?}");
        }
        std::fs::remove_dir_all(root).ok();
    }

    impl FileWatcherService {
        async fn call_inner_watch(&self, path: &str, recursive: bool) -> Value {
            self.watch(path, recursive).await.expect("watch")
        }
    }
}
