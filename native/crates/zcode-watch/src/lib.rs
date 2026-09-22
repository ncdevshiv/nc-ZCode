//! zcode-watch 可测核心：防抖窗口、notify 事件映射、emitter 线程循环。
//!
//! 镜像 TS 源 `packages/services/src/fileWatcher/fileWatcherService.ts` 的可观察契约：
//! - 防抖：每条原始事件重置窗口（TS `DEBOUNCE_MS = 150` 为默认值），窗口内同路径只出一条事件；
//! - 无忽略规则：TS 不过滤任何路径（Git 状态信号依赖 `.git` 下的事件），本 crate 同样不过滤；
//! - 底层 watcher 出错：镜像 TS `fsWatcher.on("error")` —— 发最终事件后清理。
//!
//! 分工（单写者模型）：notify 回调线程只 `send`；emitter 线程独占防抖状态并经
//! `Emitter`（与 serve 循环共用的 `Arc<Mutex<W>>`）写 `fs_event` 帧。

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::event::{AccessKind, CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode};
use serde_json::json;
use zcode_sidecar_proto::Emitter;

/// 防抖窗口默认毫秒 —— 镜像 TS `DEBOUNCE_MS`。
pub const DEFAULT_DEBOUNCE_MS: u64 = 150;
/// ready 事件中的 sidecar 名。
pub const SIDECAR_NAME: &str = "zcode-watch";
/// 文件变更事件名。
pub const EVENT_FS_EVENT: &str = "fs_event";
/// 无防抖事件挂起时 emitter 的轮询间隔（只影响唤醒频率，不产生行为）。
const IDLE_POLL_TIMEOUT: Duration = Duration::from_secs(3600);

/// `fs_event.kind` 词表：create / change / delete / rename / other。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsEventKind {
    Create,
    Change,
    Delete,
    Rename,
    Other,
}

impl FsEventKind {
    /// 线上字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            FsEventKind::Create => "create",
            FsEventKind::Change => "change",
            FsEventKind::Delete => "delete",
            FsEventKind::Rename => "rename",
            FsEventKind::Other => "other",
        }
    }
}

/// notify `EventKind` → sidecar kind 词表。
///
/// Node `fs.watch` 只有 `rename|change` 两种粗粒度事件；sidecar 借 notify 的
/// Create/Remove/Modify(名/数据) 细分出 create/delete/rename/change，其余归 other。
pub fn kind_from_notify(kind: &EventKind) -> FsEventKind {
    match kind {
        EventKind::Create(CreateKind::Other) => FsEventKind::Other,
        EventKind::Create(_) => FsEventKind::Create,
        EventKind::Modify(ModifyKind::Data(_)) | EventKind::Modify(ModifyKind::Metadata(_)) => {
            FsEventKind::Change
        }
        EventKind::Modify(ModifyKind::Any) => FsEventKind::Change,
        // notify 8 的未知修改子类：与其他 *Kind::Other 一致归 other。
        EventKind::Modify(ModifyKind::Other) => FsEventKind::Other,
        EventKind::Modify(ModifyKind::Name(RenameMode::Other)) => FsEventKind::Other,
        EventKind::Modify(ModifyKind::Name(_)) => FsEventKind::Rename,
        EventKind::Remove(RemoveKind::Other) => FsEventKind::Other,
        EventKind::Remove(_) => FsEventKind::Delete,
        EventKind::Access(AccessKind::Other) => FsEventKind::Other,
        EventKind::Access(_) => FsEventKind::Other,
        EventKind::Any | EventKind::Other => FsEventKind::Other,
    }
}

/// 窗口内待发的一条事件（path + kind）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebouncedPath {
    pub path: String,
    pub kind: FsEventKind,
}

/// 防抖/合并窗口。`now` 全部由调用方注入：逻辑可脱离真实时钟单测。
///
/// 合并规则：窗口内**同路径只保留一条**，kind 取窗口内**首条**（首条描述的是
/// “窗口起点前 → 现在”的状态跃迁起点：先 create 后 modify → create，先 create
/// 后 delete → create，客户端保守刷新即可）。TS 侧本就只发目录级单事件、不区分
/// kind；此规则是 per-path 形状下的等价物。
#[derive(Debug)]
pub struct Debouncer {
    window: Duration,
    /// 截止时刻 = 窗口内最后一条事件 + window（镜像 TS 每次事件 `clearTimeout` 后重开）。
    deadline: Option<Instant>,
    /// 首次出现顺序，保证输出顺序稳定。
    order: Vec<String>,
    kinds: HashMap<String, FsEventKind>,
}

impl Debouncer {
    /// `window` 为防抖窗口长度。
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            deadline: None,
            order: Vec::new(),
            kinds: HashMap::new(),
        }
    }

    /// 记录一条原始事件。若上一窗口已到期，先返回其到期事件（本事件进入新窗口）。
    pub fn push(&mut self, now: Instant, path: String, kind: FsEventKind) -> Vec<DebouncedPath> {
        let mut due = Vec::new();
        if let Some(deadline) = self.deadline {
            if now >= deadline {
                due = self.take_all();
            }
        }
        // 窗口重置：TS 每条事件都重开 setTimeout。极端超大 window 会造成 Instant
        // 溢出，回退为 deadline=now（立即到期），绝不 panic。
        self.deadline = Some(now.checked_add(self.window).unwrap_or(now));
        if !self.kinds.contains_key(&path) {
            self.order.push(path.clone());
            self.kinds.insert(path, kind);
        }
        // 同路径首条已占位：后续 kind 在本窗口内丢弃（first-wins）。
        due
    }

    /// 窗口到期则取出全部待发事件；未到期返回空。
    pub fn flush_due(&mut self, now: Instant) -> Vec<DebouncedPath> {
        match self.deadline {
            Some(deadline) if now >= deadline => self.take_all(),
            _ => Vec::new(),
        }
    }

    /// 下一次到期时刻（emitter 用它计算 `recv_timeout`）。
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    fn take_all(&mut self) -> Vec<DebouncedPath> {
        self.deadline = None;
        let kinds = std::mem::take(&mut self.kinds);
        std::mem::take(&mut self.order)
            .into_iter()
            .filter_map(|path| {
                kinds
                    .get(&path)
                    .map(|kind| DebouncedPath { path, kind: *kind })
            })
            .collect()
    }
}

/// emitter 线程的消息。同一通道 FIFO：`Add` 在 watcher 创建前入队，保证防抖
/// 窗口先于该 watcher 的首个 `Raw` 事件建立。
#[derive(Debug)]
pub enum WatchMsg {
    /// 注册 watchId 的防抖窗口。
    Add { watch_id: String, window: Duration },
    /// 移除 watchId 并丢弃其未发事件（unwatch / watch 失败回滚；镜像 TS clearTimeout）。
    Remove { watch_id: String },
    /// notify 原始事件（已按 path 拆开、kind 已映射）。
    Raw {
        watch_id: String,
        path: String,
        kind: FsEventKind,
    },
    /// 底层 watcher 出错：镜像 TS error → 最终事件 + 清理。
    WatchError {
        watch_id: String,
        paths: Vec<String>,
    },
}

/// emitter 线程主循环：收原始事件 → 防抖合并 → 写 `fs_event` 帧。
///
/// 所有 sender（serve 侧 handler + notify 回调）都释放后通道断开：立即退出并
/// **丢弃** pending —— 镜像 TS `cleanup()` 里 `clearTimeout` 的语义（shutdown 后
/// 进程即将退出，未到期事件不补发）。
pub fn run_emitter<W: Write>(
    receiver: Receiver<WatchMsg>,
    dead_ids: Arc<Mutex<HashSet<String>>>,
    emitter: Emitter<W>,
) {
    let mut debouncers: HashMap<String, Debouncer> = HashMap::new();
    loop {
        let now = Instant::now();
        let timeout = debouncers
            .values()
            .filter_map(Debouncer::deadline)
            .map(|deadline| deadline.saturating_duration_since(now))
            .min()
            .unwrap_or(IDLE_POLL_TIMEOUT);
        match receiver.recv_timeout(timeout) {
            Ok(WatchMsg::Add { watch_id, window }) => {
                debouncers.insert(watch_id, Debouncer::new(window));
            }
            Ok(WatchMsg::Remove { watch_id }) => {
                debouncers.remove(&watch_id);
            }
            Ok(WatchMsg::Raw {
                watch_id,
                path,
                kind,
            }) => {
                if let Some(debouncer) = debouncers.get_mut(&watch_id) {
                    let due = debouncer.push(Instant::now(), path, kind);
                    emit_paths(&emitter, &watch_id, &due);
                }
            }
            Ok(WatchMsg::WatchError { watch_id, paths }) => {
                // TS：error → 先发最终变更事件，再 cleanup（丢 pending、出列表）。
                debouncers.remove(&watch_id);
                if let Ok(mut dead) = dead_ids.lock() {
                    dead.insert(watch_id.clone());
                }
                for path in paths {
                    let _ = emitter.emit(
                        EVENT_FS_EVENT,
                        json!({
                            "watchId": watch_id,
                            "path": path,
                            "kind": FsEventKind::Other.as_str(),
                        }),
                    );
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                let now = Instant::now();
                for (watch_id, debouncer) in &mut debouncers {
                    let due = debouncer.flush_due(now);
                    emit_paths(&emitter, watch_id, &due);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn emit_paths<W: Write>(emitter: &Emitter<W>, watch_id: &str, paths: &[DebouncedPath]) {
    for entry in paths {
        // 写失败（stdout 断开）由 serve 侧统一收口；这里停止本批继续写即可。
        if emitter
            .emit(
                EVENT_FS_EVENT,
                json!({
                    "watchId": watch_id,
                    "path": entry.path,
                    "kind": entry.kind.as_str(),
                }),
            )
            .is_err()
        {
            break;
        }
    }
}

/// 相对路径基于进程 cwd 绝对化，并词法清理 `.`/`..` —— 对齐 JS `path.resolve`
/// 的纯字符串语义（不访问文件系统；保留 Windows 盘符 / UNC 前缀；越过根保根）。
pub fn resolve_path(input: &Path) -> std::io::Result<PathBuf> {
    let absolute = if input.is_absolute() {
        input.to_path_buf()
    } else {
        std::env::current_dir()?.join(input)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other.as_os_str()),
        }
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    fn pairs(paths: &[DebouncedPath]) -> Vec<(&str, FsEventKind)> {
        paths
            .iter()
            .map(|entry| (entry.path.as_str(), entry.kind))
            .collect()
    }

    #[test]
    fn debounce_coalesces_window_and_keeps_first_kind_in_first_seen_order() {
        let mut debouncer = Debouncer::new(Duration::from_millis(150));
        let t0 = Instant::now();
        let ms = Duration::from_millis;

        assert!(debouncer
            .push(t0, "a".into(), FsEventKind::Create)
            .is_empty());
        // 同路径后续事件在窗口内被吞掉，kind 保留首条（first-wins）。
        assert!(debouncer
            .push(t0 + ms(10), "a".into(), FsEventKind::Change)
            .is_empty());
        assert!(debouncer
            .push(t0 + ms(20), "b".into(), FsEventKind::Delete)
            .is_empty());

        // 窗口随每条事件重置（TS clearTimeout 语义）：最后一条在 t0+20 → 截止 t0+170。
        assert!(debouncer.flush_due(t0 + ms(169)).is_empty(), "未到期不发");
        assert_eq!(
            pairs(&debouncer.flush_due(t0 + ms(170))),
            vec![("a", FsEventKind::Create), ("b", FsEventKind::Delete)],
            "输出按路径首见顺序、kind 取首条"
        );
        assert!(debouncer.flush_due(t0 + ms(180)).is_empty(), "取空后不再发");
        assert_eq!(debouncer.deadline(), None);
    }

    #[test]
    fn push_after_expiry_flushes_previous_window_first() {
        let mut debouncer = Debouncer::new(Duration::from_millis(150));
        let t0 = Instant::now();
        let ms = Duration::from_millis;

        assert!(debouncer
            .push(t0, "old".into(), FsEventKind::Create)
            .is_empty());
        // 上一窗口已过期：先交旧窗口事件，本事件进入新窗口。
        let due = debouncer.push(t0 + ms(400), "new".into(), FsEventKind::Change);
        assert_eq!(pairs(&due), vec![("old", FsEventKind::Create)]);
        assert_eq!(
            pairs(&debouncer.flush_due(t0 + ms(560))),
            vec![("new", FsEventKind::Change)]
        );
    }

    #[test]
    fn deadline_is_reset_by_every_event_like_ts_settimeout() {
        let mut debouncer = Debouncer::new(Duration::from_millis(150));
        let t0 = Instant::now();
        let ms = Duration::from_millis;

        debouncer.push(t0, "a".into(), FsEventKind::Change);
        assert_eq!(debouncer.deadline(), Some(t0 + ms(150)));
        // TS：每条事件 clearTimeout 后重开 —— 截止时刻跟随最后一条事件。
        debouncer.push(t0 + ms(100), "a".into(), FsEventKind::Change);
        assert_eq!(debouncer.deadline(), Some(t0 + ms(250)));
        assert!(
            debouncer.flush_due(t0 + ms(200)).is_empty(),
            "被重置的窗口不提前发"
        );
    }

    #[test]
    fn zero_window_flushes_each_event_and_absurd_window_never_panics() {
        let mut zero = Debouncer::new(Duration::ZERO);
        let t0 = Instant::now();
        assert!(zero.push(t0, "a".into(), FsEventKind::Create).is_empty());
        assert_eq!(
            pairs(&zero.flush_due(t0)),
            vec![("a", FsEventKind::Create)],
            "窗口为 0 立即到期"
        );

        // Duration::MAX 使任何平台的 Instant + window 都溢出：回退 deadline=now
        // （立即到期）且绝不 panic。（from_millis(u64::MAX) 在部分平台可表示，
        // 不能用作跨平台的溢出用例。）
        let mut huge = Debouncer::new(Duration::MAX);
        assert!(huge.push(t0, "a".into(), FsEventKind::Create).is_empty());
        assert_eq!(
            pairs(&huge.flush_due(t0)),
            vec![("a", FsEventKind::Create)],
            "溢出回退后立即到期"
        );
    }

    #[test]
    fn kind_mapping_covers_notify_vocabulary() {
        use notify::event::{AccessKind, DataChange, MetadataKind};
        let cases = [
            (EventKind::Create(CreateKind::File), FsEventKind::Create),
            (EventKind::Create(CreateKind::Folder), FsEventKind::Create),
            (EventKind::Create(CreateKind::Other), FsEventKind::Other),
            (
                EventKind::Modify(ModifyKind::Data(DataChange::Content)),
                FsEventKind::Change,
            ),
            (
                EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
                FsEventKind::Change,
            ),
            (EventKind::Modify(ModifyKind::Any), FsEventKind::Change),
            (
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                FsEventKind::Rename,
            ),
            (
                EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                FsEventKind::Rename,
            ),
            (
                EventKind::Modify(ModifyKind::Name(RenameMode::Other)),
                FsEventKind::Other,
            ),
            (EventKind::Remove(RemoveKind::File), FsEventKind::Delete),
            (EventKind::Remove(RemoveKind::Folder), FsEventKind::Delete),
            (EventKind::Remove(RemoveKind::Any), FsEventKind::Delete),
            (EventKind::Remove(RemoveKind::Other), FsEventKind::Other),
            (EventKind::Access(AccessKind::Read), FsEventKind::Other),
            (EventKind::Any, FsEventKind::Other),
            (EventKind::Other, FsEventKind::Other),
        ];
        for (kind, expected) in cases {
            assert_eq!(kind_from_notify(&kind), expected, "kind: {kind:?}");
        }
        assert_eq!(FsEventKind::Create.as_str(), "create");
        assert_eq!(FsEventKind::Change.as_str(), "change");
        assert_eq!(FsEventKind::Delete.as_str(), "delete");
        assert_eq!(FsEventKind::Rename.as_str(), "rename");
        assert_eq!(FsEventKind::Other.as_str(), "other");
    }

    #[test]
    fn resolve_path_matches_js_path_resolve_lexical_semantics() {
        let cwd = std::env::current_dir().expect("cwd");
        assert_eq!(
            resolve_path(Path::new("a/./b/../c")).expect("resolve"),
            cwd.join("a").join("c"),
            "相对路径基于 cwd 绝对化并词法清理"
        );
        let absolute = cwd.join("x").join("y");
        assert_eq!(
            resolve_path(&absolute).expect("resolve"),
            absolute,
            "绝对路径只做词法清理"
        );
        // 越过根保根：`..` 弹过卷根后 pop 无效（JS path.resolve("/..") === 根）。
        // 弹出次数按 cwd 深度计算：硬编码次数在深/浅 cwd 下会差一两级。
        let mut over = cwd.clone();
        for _ in 0..(cwd.components().count() + 4) {
            over.push("..");
        }
        let cleaned = resolve_path(&over).expect("resolve");
        assert!(
            cleaned.parent().is_none(),
            "过度的 .. 必须停在卷根: {cleaned:?}"
        );
    }

    #[test]
    fn run_emitter_flushes_dues_and_discards_pending_on_close() {
        let (tx, rx) = mpsc::channel::<WatchMsg>();
        let dead_ids = Arc::new(Mutex::new(HashSet::new()));
        let emitter = Emitter::new(Vec::new());
        let join = {
            let dead_ids = dead_ids.clone();
            let emitter = emitter.clone();
            std::thread::spawn(move || run_emitter(rx, dead_ids, emitter))
        };

        // 窗口 0：Raw2 的 push 交出 Raw1 窗口的到期事件，Raw3 交出 Raw2 的。
        tx.send(WatchMsg::Add {
            watch_id: "0".into(),
            window: Duration::ZERO,
        })
        .expect("send add");
        tx.send(WatchMsg::Raw {
            watch_id: "0".into(),
            path: "a".into(),
            kind: FsEventKind::Create,
        })
        .expect("send raw1");
        tx.send(WatchMsg::Raw {
            watch_id: "0".into(),
            path: "a".into(),
            kind: FsEventKind::Change,
        })
        .expect("send raw2");
        tx.send(WatchMsg::Raw {
            watch_id: "0".into(),
            path: "a".into(),
            kind: FsEventKind::Delete,
        })
        .expect("send raw3");
        drop(tx); // 断开 → 丢弃最后窗口的 pending（镜像 TS clearTimeout）
        join.join().expect("emitter thread");

        let out = String::from_utf8_lossy(&emitter.contents()).into_owned();
        let lines: Vec<&str> = out.trim_end_matches('\n').split('\n').collect();
        assert_eq!(
            lines,
            vec![
                r#"{"v":1,"event":"fs_event","data":{"watchId":"0","path":"a","kind":"create"}}"#,
                r#"{"v":1,"event":"fs_event","data":{"watchId":"0","path":"a","kind":"change"}}"#,
            ],
            "窗口到期事件照发，断开时未到期的 delete 必须被丢弃",
        );
    }

    #[test]
    fn run_emitter_remove_drops_pending_and_ignores_late_raws() {
        let (tx, rx) = mpsc::channel::<WatchMsg>();
        let dead_ids = Arc::new(Mutex::new(HashSet::new()));
        let emitter = Emitter::new(Vec::new());
        let join = {
            let dead_ids = dead_ids.clone();
            let emitter = emitter.clone();
            std::thread::spawn(move || run_emitter(rx, dead_ids, emitter))
        };

        tx.send(WatchMsg::Add {
            watch_id: "7".into(),
            window: Duration::from_secs(3600),
        })
        .expect("add");
        tx.send(WatchMsg::Raw {
            watch_id: "7".into(),
            path: "pending".into(),
            kind: FsEventKind::Create,
        })
        .expect("raw");
        // unwatch：pending 丢弃（TS cleanup 清定时器），迟到原始事件被忽略。
        tx.send(WatchMsg::Remove {
            watch_id: "7".into(),
        })
        .expect("remove");
        tx.send(WatchMsg::Raw {
            watch_id: "7".into(),
            path: "late".into(),
            kind: FsEventKind::Delete,
        })
        .expect("late raw");
        drop(tx);
        join.join().expect("emitter thread");

        assert_eq!(
            emitter.contents(),
            Vec::<u8>::new(),
            "Remove 之后不得再产生任何 fs_event"
        );
    }

    #[test]
    fn run_emitter_watch_error_emits_final_events_marks_dead_and_drops_pending() {
        let (tx, rx) = mpsc::channel::<WatchMsg>();
        let dead_ids = Arc::new(Mutex::new(HashSet::new()));
        let emitter = Emitter::new(Vec::new());
        let join = {
            let dead_ids = dead_ids.clone();
            let emitter = emitter.clone();
            std::thread::spawn(move || run_emitter(rx, dead_ids, emitter))
        };

        tx.send(WatchMsg::Add {
            watch_id: "3".into(),
            window: Duration::from_secs(3600),
        })
        .expect("add");
        tx.send(WatchMsg::Raw {
            watch_id: "3".into(),
            path: "pending".into(),
            kind: FsEventKind::Create,
        })
        .expect("raw");
        tx.send(WatchMsg::WatchError {
            watch_id: "3".into(),
            paths: vec!["/root/a".into(), "/root/b".into()],
        })
        .expect("watch error");
        tx.send(WatchMsg::Raw {
            watch_id: "3".into(),
            path: "after".into(),
            kind: FsEventKind::Change,
        })
        .expect("raw after");
        drop(tx);
        join.join().expect("emitter thread");

        let out = String::from_utf8_lossy(&emitter.contents()).into_owned();
        let lines: Vec<&str> = out.trim_end_matches('\n').split('\n').collect();
        assert_eq!(
            lines,
            vec![
                r#"{"v":1,"event":"fs_event","data":{"watchId":"3","path":"/root/a","kind":"other"}}"#,
                r#"{"v":1,"event":"fs_event","data":{"watchId":"3","path":"/root/b","kind":"other"}}"#,
            ],
            "TS error 语义：最终事件（kind other）发出；pending 与后续事件全部丢弃",
        );
        assert!(dead_ids.lock().expect("dead lock").contains("3"));
    }
}
