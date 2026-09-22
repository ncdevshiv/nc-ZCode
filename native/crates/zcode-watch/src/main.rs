//! zcode-watch sidecar bin：stdio 行循环 + notify watcher + emitter 线程。
//!
//! 线程/所有权模型（单写者）：
//!
//! ```text
//! notify 回调线程 ──WatchMsg──▶ emitter 线程 ──┐
//! serve(stdin) 请求/响应 ─────────────────────┼── Emitter(Arc<Mutex<Stdout>>) ──▶ stdout
//! watch/unwatch handler 持有 RecommendedWatcher │（所有写出经同一把锁，行级交错安全）
//! ```
//!
//! 退出序：serve 返回 → drop 注册表与 state（watcher/回调 sender 全释放）→
//! emitter 收 Disconnected 退出 → join → 按 Shutdown 决定退出码。

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use notify::{recommended_watcher, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::json;
use zcode_sidecar_proto::{
    bad_request, optional_bool, optional_u64, require_object, require_string, require_string_array,
    serve, Emitter, HandlerError, Registry,
};
use zcode_watch::{
    kind_from_notify, resolve_path, run_emitter, WatchMsg, DEFAULT_DEBOUNCE_MS, SIDECAR_NAME,
};

/// serve 侧唯一的 watcher 注册表（owner：serve 线程；emitter 只经通道被通知）。
/// 值直接是 `RecommendedWatcher`：所有权即生命周期（remove 即停watch），无冗余字段。
#[derive(Default)]
struct WatchState {
    next_id: u64,
    entries: BTreeMap<u64, RecommendedWatcher>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // handler panic 会留毒锁；注册表状态可从内层继续（无不变量跨越 panic）。
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn main() {
    let emitter = Emitter::new(io::stdout());
    if let Err(err) = emitter.emit(
        "ready",
        json!({ "sidecar": SIDECAR_NAME, "proto": zcode_sidecar_proto::SIDECAR_PROTOCOL_VERSION }),
    ) {
        eprintln!("zcode-watch: emit ready failed: {err}");
        std::process::exit(1);
    }

    let (tx, rx) = mpsc::channel::<WatchMsg>();
    let dead_ids = Arc::new(Mutex::new(HashSet::<String>::new()));
    let emitter_thread = {
        let emitter = emitter.clone();
        let dead_ids = dead_ids.clone();
        thread::Builder::new()
            .name("zcode-watch-emitter".into())
            .spawn(move || run_emitter(rx, dead_ids, emitter))
    };
    let emitter_thread = match emitter_thread {
        Ok(handle) => handle,
        Err(err) => {
            eprintln!("zcode-watch: spawn emitter failed: {err}");
            std::process::exit(1);
        }
    };

    let state = Arc::new(Mutex::new(WatchState::default()));
    let mut registry: Registry<io::Stdout> = Registry::new();
    register_watch(&mut registry, state.clone(), tx.clone());
    register_unwatch(&mut registry, state.clone(), tx.clone());
    register_list(&mut registry, state.clone(), dead_ids.clone());

    let shutdown = serve(io::stdin().lock(), &emitter, &registry);

    // 释放全部 sender：handler 克隆随注册表释放，watcher 回调克隆随 state 释放，
    // emitter 通道断开即退出并丢弃 pending（镜像 TS cleanup 的 clearTimeout）。
    drop(registry);
    drop(state);
    drop(tx);
    let _ = emitter_thread.join();

    match shutdown {
        Ok(shutdown) => std::process::exit(shutdown.exit_code()),
        Err(err) => {
            eprintln!("zcode-watch: serve failed: {err}");
            std::process::exit(1);
        }
    }
}

/// `watch {paths, recursive?, debounceMs?} -> {watchId}`。
///
/// 失败路径（均镜像 TS 抛错语义，spec 映射为 400）：路径数组非法 → 400；
/// 任一路径无法监视 → 400 且文案保持 `无法监视目录 '<path>': …`；失败时回滚
/// emitter 侧已入队的防抖窗口。
fn register_watch(
    registry: &mut Registry<io::Stdout>,
    state: Arc<Mutex<WatchState>>,
    tx: Sender<WatchMsg>,
) {
    registry.register("watch", move |params, _emitter| {
        let object = require_object(params)?;
        let raw_paths = require_string_array(object, "paths")?;
        let recursive = optional_bool(object, "recursive", false)?;
        let debounce_ms = optional_u64(object, "debounceMs", DEFAULT_DEBOUNCE_MS)?;
        let window = Duration::from_millis(debounce_ms);

        let mut paths = Vec::with_capacity(raw_paths.len());
        for raw in &raw_paths {
            let resolved = resolve_path(Path::new(raw)).map_err(|err| {
                HandlerError::internal(format!("resolve path '{raw}' failed: {err}"))
            })?;
            paths.push(resolved.to_string_lossy().into_owned());
        }

        let mut guard = lock(&state);
        let id = guard.next_id;
        guard.next_id += 1;
        let watch_id = id.to_string();

        // 先入队 Add：通道 FIFO 保证 emitter 在该 watcher 首个事件前建好窗口。
        tx.send(WatchMsg::Add {
            watch_id: watch_id.clone(),
            window,
        })
        .map_err(|_| HandlerError::internal("emitter terminated"))?;

        match create_watcher(&watch_id, &paths, recursive, &tx) {
            Ok(watcher) => {
                guard.entries.insert(id, watcher);
                Ok(json!({ "watchId": watch_id }))
            }
            Err(err) => {
                // 回滚：emitter 侧撤掉刚入队的窗口，保持两表一致。
                let _ = tx.send(WatchMsg::Remove { watch_id });
                Err(err)
            }
        }
    });
}

fn create_watcher(
    watch_id: &str,
    paths: &[String],
    recursive: bool,
    tx: &Sender<WatchMsg>,
) -> Result<RecommendedWatcher, HandlerError> {
    let callback_tx = tx.clone();
    let callback_id = watch_id.to_string();
    let callback_paths = paths.to_vec();
    let mut watcher = recommended_watcher(move |result: notify::Result<notify::Event>| {
        match result {
            Ok(event) => {
                let kind = kind_from_notify(&event.kind);
                for path in event.paths {
                    if path.as_os_str().is_empty() {
                        continue;
                    }
                    let _ = callback_tx.send(WatchMsg::Raw {
                        watch_id: callback_id.clone(),
                        path: path.to_string_lossy().into_owned(),
                        kind,
                    });
                }
            }
            Err(_err) => {
                // TS fsWatcher.on("error")：发最终事件（无法判定具体变更 → kind other）
                // 并清理；由 emitter 标记 dead、丢弃 pending。
                let _ = callback_tx.send(WatchMsg::WatchError {
                    watch_id: callback_id.clone(),
                    paths: callback_paths.clone(),
                });
            }
        }
    })
    .map_err(|err| HandlerError::internal(format!("create watcher failed: {err}")))?;

    for path in paths {
        let mode = if recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        watcher.watch(Path::new(path), mode).map_err(|err| {
            // 保留 TS 错误文案（fileWatcherService.ts watch() catch 分支）。
            bad_request(format!("无法监视目录 '{path}': {err}"))
        })?;
    }
    Ok(watcher)
}

/// `unwatch {watchId} -> {}`；未知 id → 404（TS cleanup 对未知 id 静默成功，
/// spec 指定 sidecar 报 404 —— 差异记录于 README）。
fn register_unwatch(
    registry: &mut Registry<io::Stdout>,
    state: Arc<Mutex<WatchState>>,
    tx: Sender<WatchMsg>,
) {
    registry.register("unwatch", move |params, _emitter| {
        let object = require_object(params)?;
        let watch_id = require_string(object, "watchId")?;
        // id 是计数器的规范十进制串：拒绝 "01" 之类的等价串，避免误删真实条目。
        let numeric = watch_id
            .parse::<u64>()
            .ok()
            .filter(|id| id.to_string() == watch_id)
            .ok_or_else(|| HandlerError::not_found(format!("unknown watchId: {watch_id}")))?;
        let mut guard = lock(&state);
        if guard.entries.remove(&numeric).is_none() {
            return Err(HandlerError::not_found(format!(
                "unknown watchId: {watch_id}"
            )));
        }
        drop(guard);
        // 丢弃该 watchId 的未发事件（TS cleanup 的 clearTimeout 语义）。
        let _ = tx.send(WatchMsg::Remove { watch_id });
        Ok(json!({}))
    });
}

/// `list -> {ids}`；按 id 数值升序，emitter 标记 dead 的 id 不再返回
/// （镜像 TS error-cleanup 后从 watchers 表消失的行为）。
fn register_list(
    registry: &mut Registry<io::Stdout>,
    state: Arc<Mutex<WatchState>>,
    dead_ids: Arc<Mutex<HashSet<String>>>,
) {
    registry.register("list", move |_params, _emitter| {
        let guard = lock(&state);
        let dead = lock(&dead_ids);
        let ids: Vec<String> = guard
            .entries
            .keys()
            .map(u64::to_string)
            .filter(|id| !dead.contains(id))
            .collect();
        Ok(json!({ "ids": ids }))
    });
}
