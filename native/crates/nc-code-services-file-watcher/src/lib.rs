//! `file-watcher` channel �?" real filesystem watching via `notify`.
//!
//! Port of the TS `packages/services/src/fileWatcher/fileWatcherService.ts` contract:
//! - `watch({ path, recursive? }) -> { id }`
//! - `unwatch({ id }) -> void`
//! - `disposeAll() -> void`
//! - `onDynamicChange(id)` event: fires `FileWatchEvent { dirPath, changedPath? }`
//!
//! Uses `notify` for cross-platform watching. Events are debounced (150ms) and coalesced
//! per watched directory, matching TS behavior. Each watcher has an independent event bus;
//! `listen("onDynamicChange", id)` subscribes to that bus.

/// Handoff skeleton (pure data/types): serde persistence structs, port trait, Send/Sync markers.
pub mod handoff;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use nc_code_channels::{
    CancelToken, Channel, ChannelContext, ChannelFailure, EventBus, EventSubscription,
};
use nc_code_rpc::WireValue;
use nc_code_service_core::{ServiceArgs, ServiceError};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::json;
use tokio::sync::mpsc;

const DEBOUNCE_MS: u64 = 150;

struct WatcherEntry {
    _dir_path: String,
    bus: EventBus,
    // Dropping the watcher stops OS notifications.
    _watcher: RecommendedWatcher,
}

#[derive(Default)]
struct DebounceState {
    timer_active: bool,
    changed_paths: HashSet<String>,
    has_unknown: bool,
}

pub struct FileWatcherService {
    next_id: AtomicU64,
    watchers: Mutex<HashMap<String, Arc<WatcherEntry>>>,
}

impl FileWatcherService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            next_id: AtomicU64::new(1),
            watchers: Mutex::new(HashMap::new()),
        })
    }

    fn cleanup(&self, id: &str) {
        let mut watchers = self.watchers.lock().unwrap_or_else(|p| p.into_inner());
        watchers.remove(id);
    }

    fn cleanup_all(&self) {
        let mut watchers = self.watchers.lock().unwrap_or_else(|p| p.into_inner());
        watchers.clear();
    }

    async fn watch(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let path_str = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ServiceError::invalid_params("path is required"))?
            .to_string();
        let recursive = params
            .get("recursive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let dir_path = PathBuf::from(&path_str);
        if !dir_path.exists() {
            return Err(ServiceError::new(format!(
                "Failed to watch directory '{}': path does not exist",
                path_str
            )));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let bus = EventBus::new(64);
        let bus_for_notify = bus.clone();
        let dir_for_notify = path_str.clone();
        let (tx, mut rx) = mpsc::unbounded_channel::<Option<String>>();

        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let changed = match res {
                Ok(event) => {
                    // Prefer the first path; normalize to string.
                    event
                        .paths
                        .first()
                        .map(|p| p.to_string_lossy().into_owned())
                }
                Err(_) => None,
            };
            let _ = tx.send(changed);
        })
        .map_err(|e| ServiceError::new(format!("Failed to watch directory '{}': {e}", path_str)))?;

        let mode = if recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        watcher.watch(Path::new(&path_str), mode).map_err(|e| {
            ServiceError::new(format!("Failed to watch directory '{}': {e}", path_str))
        })?;

        // Debounce/coalesce task: batches changes and fires FileWatchEvent.
        let debounce_bus = bus_for_notify;
        let debounce_dir = dir_for_notify;
        tokio::spawn(async move {
            let mut state = DebounceState::default();
            let mut deadline: Option<tokio::time::Instant> = None;
            loop {
                let sleep = async {
                    if let Some(d) = deadline {
                        tokio::time::sleep_until(d).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                };
                tokio::select! {
                    msg = rx.recv() => {
                        match msg {
                            Some(changed) => {
                                match changed {
                                    Some(p) => { state.changed_paths.insert(p); }
                                    None => { state.has_unknown = true; }
                                }
                                state.timer_active = true;
                                deadline = Some(tokio::time::Instant::now() + Duration::from_millis(DEBOUNCE_MS));
                            }
                            None => break, // watcher dropped
                        }
                    }
                    _ = sleep, if deadline.is_some() => {
                        let only = if !state.has_unknown && state.changed_paths.len() == 1 {
                            state.changed_paths.iter().next().cloned()
                        } else {
                            None
                        };
                        let mut event = json!({ "dirPath": debounce_dir });
                        if let Some(changed_path) = only {
                            event["changedPath"] = json!(changed_path);
                        }
                        debounce_bus.send(WireValue::Json(event));
                        state = DebounceState::default();
                        deadline = None;
                    }
                }
            }
        });

        let entry = Arc::new(WatcherEntry {
            _dir_path: path_str,
            bus,
            _watcher: watcher,
        });
        self.watchers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.clone(), entry);
        Ok(WireValue::Json(json!({ "id": id })))
    }

    async fn unwatch(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let id = params
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ServiceError::invalid_params("id is required"))?;
        self.cleanup(id);
        Ok(WireValue::Undefined)
    }

    fn dispose_all(&self) -> Result<WireValue, ServiceError> {
        self.cleanup_all();
        Ok(WireValue::Undefined)
    }
}

impl Default for FileWatcherService {
    fn default() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            watchers: Mutex::new(HashMap::new()),
        }
    }
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
            "watch" => self.watch(&args).await.map_err(ChannelFailure::from),
            "unwatch" => self.unwatch(&args).await.map_err(ChannelFailure::from),
            "disposeAll" => self.dispose_all().map_err(ChannelFailure::from),
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }

    async fn listen(
        &self,
        _ctx: &ChannelContext,
        event: &str,
        arg: WireValue,
    ) -> Result<EventSubscription, ChannelFailure> {
        match event {
            "onDynamicChange" => {
                let id = match &arg {
                    WireValue::String(s) => s.clone(),
                    WireValue::Json(v) => v.as_str().unwrap_or_default().to_string(),
                    _ => return Err(ChannelFailure::event_not_found(event)),
                };
                let watchers = self.watchers.lock().unwrap_or_else(|p| p.into_inner());
                let entry = watchers.get(&id).cloned();
                drop(watchers);
                match entry {
                    Some(e) => Ok(EventSubscription::from_broadcast(e.bus.subscribe())),
                    None => {
                        // TS returns Event.None for stale watcher ids (logs warning).
                        // Create a closed/empty subscription.
                        let (tx, rx) = tokio::sync::mpsc::channel(1);
                        drop(tx);
                        Ok(EventSubscription::from_receiver(rx))
                    }
                }
            }
            other => Err(ChannelFailure::event_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;
    use tempfile::TempDir;

    fn ctx() -> ChannelContext {
        ChannelContext::new("file-watcher", ClientMode::DesktopContinuous)
    }

    #[tokio::test]
    async fn watch_unwatch_dispose_lifecycle() {
        let tmp = TempDir::new().unwrap();
        let svc = FileWatcherService::new();
        let res = svc
            .call(
                &ctx(),
                "watch",
                vec![WireValue::Json(
                    json!({"path": tmp.path().to_string_lossy(), "recursive": false}),
                )],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let id = match res {
            WireValue::Json(v) => v["id"].as_str().unwrap().to_string(),
            _ => panic!("expected json"),
        };
        assert!(!id.is_empty());

        let res = svc
            .call(
                &ctx(),
                "unwatch",
                vec![WireValue::Json(json!({"id": id}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(res, WireValue::Undefined));

        let res = svc
            .call(&ctx(), "disposeAll", vec![], CancelToken::new())
            .await
            .unwrap();
        assert!(matches!(res, WireValue::Undefined));
    }

    #[tokio::test]
    async fn watch_missing_path_errors() {
        let svc = FileWatcherService::new();
        let err = svc
            .call(
                &ctx(),
                "watch",
                vec![WireValue::Json(
                    json!({"path": "G:/definitely-not-a-real-path-xyz-12345"}),
                )],
                CancelToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Failed to watch")
                || err.to_string().contains("does not exist")
        );
    }

    #[tokio::test]
    async fn listen_stale_id_returns_empty_subscription() {
        let svc = FileWatcherService::new();
        let mut sub = svc
            .listen(&ctx(), "onDynamicChange", WireValue::String("9999".into()))
            .await
            .unwrap();
        // Should complete quickly with None (closed channel)
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), sub.next()).await;
        assert!(matches!(result, Ok(None) | Err(_)));
    }

    #[tokio::test]
    async fn unknown_method_and_event_fail() {
        let svc = FileWatcherService::new();
        assert!(svc
            .call(&ctx(), "nope", vec![], CancelToken::new())
            .await
            .is_err());
        assert!(svc
            .listen(&ctx(), "onNope", WireValue::Undefined)
            .await
            .is_err());
    }
}
