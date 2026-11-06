//! nc-code-scan sidecar bin：stdio 行循环 + 每扫描一个线程。
//!
//! 线程/所有权模型（单写者）：
//!
//! ```text
//! 扫描线程（每 scanId 一个）──scan_entry/scan_error/scan_done──┐
//! serve(stdin) 循环（scan/cancel/shutdown 请求与响应）──────────┼── Emitter = Arc<Mutex<Stdout>>
//! cancel/shutdown 经 ScanState.running 的取消位停止遍历        │  （唯一写者模型，行级安全交错）
//! ```
//!
//! 退出序：serve 返回 → 置位全部取消位 → drop 注册表 → join 全部扫描线程
//! （在途扫描先发出 `cancelled:true` 的 scan_done 再退出，终态不静默）→ 按
//! `Shutdown` 决定退出码。`scanId` 在 scan_done **之前**释放：客户端收到终态
//! 事件后复用 id 必然成功；线程存活判断不依赖 running 表，统一走 handles join。

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use nc_code_scan::{
    walk_storage_root, ScanEntry, ScanError, ScanSummary, WalkOptions, SIDECAR_NAME,
};
use nc_code_sidecar_proto::{
    optional_bool, optional_u32, require_object, require_string, serve, Emitter, HandlerError,
    Registry, SIDECAR_PROTOCOL_VERSION,
};
use serde_json::{json, Value};

/// 服务状态（唯一所有者：serve 线程经 `lock` 访问；扫描线程只删自己的条目）。
#[derive(Default)]
struct ScanState {
    /// 运行中任务：scanId → 取消位（cancel/shutdown 置位，遍历逐目录/条目检查）。
    running: HashMap<String, Arc<AtomicBool>>,
    /// 全部扫描线程句柄；退出前统一 join，保证 scan_done 写完再退出进程。
    handles: Vec<thread::JoinHandle<()>>,
}

type SharedState = Arc<Mutex<ScanState>>;

fn lock(state: &SharedState) -> std::sync::MutexGuard<'_, ScanState> {
    // handler panic 可能留毒锁；状态无跨 panic 不变量，取内层值继续。
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `scan_entry {scanId, entry}` —— entry 字段镜像 TS `StorageScanEntry`。
fn emit_entry<W: Write>(emitter: &Emitter<W>, scan_id: &str, entry: &ScanEntry) -> io::Result<()> {
    emitter.emit(
        "scan_entry",
        json!({
            "scanId": scan_id,
            "entry": {
                "relativePath": entry.relative_path.as_str(),
                "bytes": entry.bytes,
                "mtimeMs": entry.mtime_ms,
            },
        }),
    )
}

/// `scan_error {scanId, path, code}` —— 镜像 TS `StoragePathError`（根级失败 path 为 ""）。
fn emit_error<W: Write>(emitter: &Emitter<W>, scan_id: &str, error: &ScanError) -> io::Result<()> {
    emitter.emit(
        "scan_error",
        json!({
            "scanId": scan_id,
            "path": error.path.as_str(),
            "code": error.code.as_str(),
        }),
    )
}

/// `scan_done {scanId, directoriesScanned, filesScanned, missingRoot, cancelled}`。
fn emit_done<W: Write>(
    emitter: &Emitter<W>,
    scan_id: &str,
    summary: &ScanSummary,
) -> io::Result<()> {
    emitter.emit(
        "scan_done",
        json!({
            "scanId": scan_id,
            "directoriesScanned": summary.directories_scanned,
            "filesScanned": summary.files_scanned,
            "missingRoot": summary.missing_root,
            "cancelled": summary.cancelled,
        }),
    )
}

/// 扫描线程体：遍历 → 释放 scanId → 发 scan_done。
///
/// 释放先于终态事件（见模块文档）：收到 done 后复用 id 必然成功；
/// 退出路径不依赖 running 表，统一由 `handles` join 兜底。
fn run_one<W: Write>(
    emitter: &Emitter<W>,
    state: &SharedState,
    cancel: &AtomicBool,
    root: &str,
    scan_id: &str,
    options: WalkOptions,
) {
    let summary = walk_storage_root(
        Path::new(root),
        &options,
        cancel,
        |entry| {
            if emit_entry(emitter, scan_id, entry).is_err() {
                // stdout 不可用（客户端断开）：与 TS emit 中断同语义，尽快停扫。
                cancel.store(true, Ordering::SeqCst);
            }
        },
        |error| {
            if emit_error(emitter, scan_id, error).is_err() {
                cancel.store(true, Ordering::SeqCst);
            }
        },
    );
    lock(state).running.remove(scan_id);
    let _ = emit_done(emitter, scan_id, &summary);
}

/// `scan {path, scanId, recursive?, maxDepth?, followSymlinks?} -> {}`。
///
/// 校验参数（400）→ 占位 scanId（运行中重复 → 409）→ 派生扫描线程立即返回；
/// 进度与终态经 scan_entry / scan_error / scan_done 事件异步推送（spec 规则 5：
/// 内部失败绝不静默）。
fn handle_scan<W: Write + Send + 'static>(
    state: &SharedState,
    emitter: &Emitter<W>,
    params: &Value,
) -> Result<Value, HandlerError> {
    let object = require_object(params)?;
    let path = require_string(object, "path")?;
    let scan_id = require_string(object, "scanId")?;
    let options = WalkOptions {
        recursive: optional_bool(object, "recursive", true)?,
        max_depth: optional_u32(object, "maxDepth", None)?,
        follow_symlinks: optional_bool(object, "followSymlinks", false)?,
    };
    let cancel = Arc::new(AtomicBool::new(false));

    // 先占位再派生：cancel 抢在任何遍历动作前到达也不会丢失取消信号。
    {
        let mut guard = lock(state);
        if guard.running.contains_key(&scan_id) {
            return Err(HandlerError::conflict(format!(
                "scanId already running: {scan_id}"
            )));
        }
        guard.running.insert(scan_id.clone(), Arc::clone(&cancel));
    }

    let spawned = {
        let emitter = emitter.clone();
        let state = Arc::clone(state);
        let cancel = Arc::clone(&cancel);
        let root = path.clone();
        let id = scan_id.clone();
        thread::Builder::new()
            .name("zcode-scan-worker".into())
            .spawn(move || run_one(&emitter, &state, &cancel, &root, &id, options))
    };
    match spawned {
        Ok(handle) => {
            lock(state).handles.push(handle);
            Ok(json!({}))
        }
        Err(err) => {
            // 派生失败：回滚占位，scanId 仍可复用；500 上报（spec 规则 5）。
            lock(state).running.remove(&scan_id);
            Err(HandlerError::internal(format!("spawn scan thread: {err}")))
        }
    }
}

/// `cancel {scanId} -> {}`；未知 scanId 为 no-op 成功（镜像 TS「非当前 job 为 no-op」）。
fn handle_cancel(state: &SharedState, params: &Value) -> Result<Value, HandlerError> {
    let object = require_object(params)?;
    let scan_id = require_string(object, "scanId")?;
    if let Some(cancel) = lock(state).running.get(&scan_id) {
        cancel.store(true, Ordering::SeqCst);
    }
    Ok(json!({}))
}

fn main() {
    let emitter = Emitter::new(io::stdout());
    // 就绪事件必须是 stdout 首帧（spec §Sidecar stdio protocol）。
    if let Err(err) = emitter.emit(
        "ready",
        json!({ "sidecar": SIDECAR_NAME, "proto": SIDECAR_PROTOCOL_VERSION }),
    ) {
        eprintln!("zcode-scan: emit ready failed: {err}");
        std::process::exit(1);
    }

    let state: SharedState = Arc::new(Mutex::new(ScanState::default()));
    let mut registry: Registry<io::Stdout> = Registry::new();
    {
        let state = Arc::clone(&state);
        registry.register("scan", move |params, emitter| {
            handle_scan(&state, emitter, params)
        });
    }
    {
        let state = Arc::clone(&state);
        registry.register("cancel", move |params, _| handle_cancel(&state, params));
    }

    let shutdown = serve(io::stdin().lock(), &emitter, &registry);

    // 退出序：置位取消 → 取走句柄与表（锁随即释放）→ drop 注册表 → join。
    // 在途扫描被取消后仍会发出 scan_done（终态事件不静默，spec 规则 5）。
    let handles = {
        let mut guard = lock(&state);
        for cancel in guard.running.values() {
            cancel.store(true, Ordering::SeqCst);
        }
        guard.running.clear();
        std::mem::take(&mut guard.handles)
    };
    drop(registry);
    drop(state);
    for handle in handles {
        let _ = handle.join();
    }

    match shutdown {
        Ok(shutdown) => std::process::exit(shutdown.exit_code()),
        Err(err) => {
            eprintln!("zcode-scan: serve failed: {err}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn shared() -> SharedState {
        Arc::new(Mutex::new(ScanState::default()))
    }

    fn scan_params(path: &Path, scan_id: &str) -> Value {
        json!({ "path": path.to_string_lossy(), "scanId": scan_id })
    }

    /// join 全部扫描线程（等价于退出序的收尾，保证事件已全部写出）。
    fn join_all(state: &SharedState) {
        let handles = std::mem::take(&mut lock(state).handles);
        for handle in handles {
            handle.join().expect("scan thread panicked");
        }
    }

    fn frames_of(output: &[u8]) -> Vec<Value> {
        String::from_utf8_lossy(output)
            .lines()
            .map(|line| serde_json::from_str(line).expect("frame is json"))
            .collect()
    }

    /// 从一帧集合中筛出指定事件名的帧。
    fn events<'b>(frames: &'b [Value], event: &str) -> Vec<&'b Value> {
        frames
            .iter()
            .filter(|frame| frame.get("event").is_some_and(|name| *name == json!(event)))
            .collect()
    }

    #[test]
    fn scan_streams_entries_and_done_then_releases_id() {
        let tmp = tempdir().expect("tempdir");
        std::fs::create_dir(tmp.path().join("sub")).expect("mkdir");
        std::fs::write(tmp.path().join("a.txt"), b"abc").expect("write");
        std::fs::write(tmp.path().join("sub").join("b.txt"), b"de").expect("write");

        let state = shared();
        let emitter = Emitter::new(Vec::new());
        let result = handle_scan(&state, &emitter, &scan_params(tmp.path(), "s1"))
            .expect("scan must be accepted");
        assert_eq!(result, json!({}));

        join_all(&state);
        assert!(
            lock(&state).running.is_empty(),
            "扫描完成后必须释放 scanId 占位"
        );

        let frames = frames_of(&emitter.contents());
        let errors = events(&frames, "scan_error");
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");

        let entries = events(&frames, "scan_entry");
        assert_eq!(entries.len(), 2, "每个文件恰好一条 scan_entry");
        let mut paths: Vec<&str> = entries
            .iter()
            .map(|frame| {
                frame["data"]["entry"]["relativePath"]
                    .as_str()
                    .expect("relativePath")
            })
            .collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            vec!["a.txt", "sub/b.txt"],
            "路径以 / 分隔的根相对路径"
        );

        let mut sizes: Vec<(&str, u64)> = entries
            .iter()
            .map(|frame| {
                (
                    frame["data"]["entry"]["relativePath"]
                        .as_str()
                        .expect("rel"),
                    frame["data"]["entry"]["bytes"].as_u64().expect("bytes"),
                )
            })
            .collect();
        sizes.sort_unstable();
        assert_eq!(
            sizes,
            vec![("a.txt", 3), ("sub/b.txt", 2)],
            "bytes = 实际字节数"
        );
        for entry in &entries {
            assert!(
                entry["data"]["entry"]["mtimeMs"].as_u64().expect("mtimeMs") > 1_600_000_000_000,
                "mtimeMs 必须是像样的 epoch 毫秒"
            );
        }

        let done = events(&frames, "scan_done");
        assert_eq!(done.len(), 1, "终态事件恰好一条");
        let data = &done[0]["data"];
        assert_eq!(data["scanId"], json!("s1"));
        assert_eq!(data["filesScanned"], json!(2));
        assert_eq!(data["directoriesScanned"], json!(2), "root + sub");
        assert_eq!(data["missingRoot"], json!(false));
        assert_eq!(data["cancelled"], json!(false));
    }

    #[test]
    fn duplicate_running_scan_id_is_409_and_param_errors_are_400() {
        let tmp = tempdir().expect("tempdir");
        let state = shared();
        let emitter = Emitter::new(Vec::new());

        // 模拟运行中的扫描：同 id 占位未释放 → 409，且不得派生新线程。
        lock(&state)
            .running
            .insert("busy".into(), Arc::new(AtomicBool::new(false)));
        let err = handle_scan(&state, &emitter, &scan_params(tmp.path(), "busy"))
            .expect_err("duplicate scanId must conflict");
        assert_eq!(err.body.code, 409);
        assert_eq!(lock(&state).handles.len(), 0, "冲突不得派生线程");

        // 参数问题一律 400（缺字段 / 类型不符），且不产生任何占位或线程。
        let path = tmp.path().to_string_lossy().into_owned();
        let bad_cases = [
            json!({}),
            json!({"scanId": "x"}),
            json!({"path": path.as_str()}),
            json!({"path": path.as_str(), "scanId": "z", "maxDepth": -1}),
            json!({"path": path.as_str(), "scanId": "z2", "recursive": "yes"}),
            json!({"path": path.as_str(), "scanId": "z3", "followSymlinks": "no"}),
        ];
        for case in bad_cases {
            let err = handle_scan(&state, &emitter, &case).expect_err("invalid params must be 400");
            assert_eq!(err.body.code, 400, "case: {case}");
        }
        let guard = lock(&state);
        assert_eq!(guard.running.len(), 1, "失败的请求不得留下占位");
        assert!(guard.running.contains_key("busy"));
        assert!(guard.handles.is_empty());
    }

    #[test]
    fn cancel_sets_running_flag_and_unknown_id_is_noop() {
        let state = shared();
        let flag = Arc::new(AtomicBool::new(false));
        lock(&state).running.insert("r1".into(), Arc::clone(&flag));

        let out = handle_cancel(&state, &json!({"scanId": "r1"})).expect("cancel");
        assert_eq!(out, json!({}));
        assert!(flag.load(Ordering::SeqCst), "cancel 必须置位运行中标记");

        // 未知 scanId：no-op 成功（镜像 TS「非当前 job 为 no-op」），状态不变。
        let out = handle_cancel(&state, &json!({"scanId": "ghost"})).expect("noop");
        assert_eq!(out, json!({}));
        assert_eq!(lock(&state).running.len(), 1, "no-op 不得改动状态");

        // 缺 scanId → 400。
        let err = handle_cancel(&state, &json!({})).expect_err("missing scanId");
        assert_eq!(err.body.code, 400);
    }

    #[test]
    fn non_recursive_param_scans_root_only() {
        let tmp = tempdir().expect("tempdir");
        std::fs::create_dir(tmp.path().join("a")).expect("mkdir");
        std::fs::write(tmp.path().join("root.txt"), b"r").expect("write");
        std::fs::write(tmp.path().join("a").join("f.txt"), b"f").expect("write");

        let state = shared();
        let emitter = Emitter::new(Vec::new());
        let params = json!({
            "path": tmp.path().to_string_lossy(),
            "scanId": "flat",
            "recursive": false,
        });
        handle_scan(&state, &emitter, &params).expect("scan");
        join_all(&state);

        let frames = frames_of(&emitter.contents());
        let entries = events(&frames, "scan_entry");
        assert_eq!(entries.len(), 1, "recursive=false 只扫根目录");
        assert_eq!(
            entries[0]["data"]["entry"]["relativePath"],
            json!("root.txt")
        );
        let done = events(&frames, "scan_done");
        assert_eq!(done[0]["data"]["filesScanned"], json!(1));
        assert_eq!(
            done[0]["data"]["directoriesScanned"],
            json!(1),
            "只打开根目录"
        );
    }
}
