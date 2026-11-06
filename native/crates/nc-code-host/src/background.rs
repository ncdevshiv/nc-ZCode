//! Background Bash execution registry and read model.
//!
//! Reference implementation (aligned entry by entry, contract in `specs/rust-native-crates.md`):
//! - `adapters/src/exec/node-execution-adapter-lifecycle.ts:299-330`
//!   (the three outcomes of `readBackgroundBashOutput`, and the "freeze the state first, then read the file" order);
//! - `packages/shared/src/background-bash-output.ts`（`backgroundBashOutputSchema` /
//!   the two shapes of `…ResultSchema` and the 8192 byte cap);
//! - same file `shouldRetainExecutionAfterRootExit: () => state === "backgrounded"`
//!   (a background run is not killed by turn cancellation).
//!
//! Single owner: the registry hangs off `Session` and is reclaimed with it; this module writes no transcript and
//! projects no rows, it only holds "what state this background task is in now and which file holds its output".

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Byte cap for a single detail snapshot.
pub const BACKGROUND_BASH_OUTPUT_MAX_BYTES: usize = 8192;

/// Cadence and bounded tail read for the tool row's `outputPreview` (TS `DEFAULT_PROGRESS_*` /
/// `PROGRESS_TAIL_MAX_BYTES`): starts only after 2 s, then reads the tail 4 KiB every 1 s.
pub(crate) const PROGRESS_THRESHOLD_MS: u64 = 2_000;
pub(crate) const PROGRESS_INTERVAL_MS: u64 = 1_000;
pub const PROGRESS_TAIL_MAX_BYTES: usize = 4_096;
const SHORT_PREVIEW_LINES: usize = 5;
const FULL_PREVIEW_LINES: usize = 100;

/// Delivery point of the toolCall row's `outputPreview` (TS `projectToolActivity`). Injected by the session at construction;
/// None = no projection surface (library-level unit tests / contract probe), the progress thread still ticks at the cadence but reads no file.
pub trait ToolOutputPreviewSink: Send + Sync {
    fn output_preview(&self, turn_id: &str, tool_call_id: &str, preview: Value);
}

/// Closed set of settle states (TS `backgroundBashOutputSchema.status`).
pub const BG_RUNNING: &str = "running";
pub const BG_COMPLETED: &str = "completed";
pub const BG_FAILED: &str = "failed";
pub const BG_TIMED_OUT: &str = "timed_out";
pub const BG_CANCELLED: &str = "cancelled";
pub const BG_SPAWN_ERROR: &str = "spawn_error";

/// Kill-tree wait cap (the same value as the one-shot bash timeout watchdog).
const KILL_TREE_GRACE_MS: u64 = 2_000;

#[derive(Debug, Clone)]
pub struct BackgroundWork {
    pub work_id: String,
    pub command: String,
    pub output_path: PathBuf,
    pub status: &'static str,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    /// shell process pid. A background task outlives the round, and without it there is no way to reap it
    /// (cancellation and process exit kill the tree using only this value).
    pub pid: Option<u32>,
    /// toolCall id that handed this task to the background. The row projection writes
    /// `backgrounded: true` + `workId` on the toolCall row from it (TS `rows.ts:229-230`), so the client knows it can use the workId
    /// to query `v4/conversation/backgroundBashOutput`. The fact comes from the registry itself, never reverse-engineered from tool text.
    pub tool_call_id: Option<String>,
    /// The two local entries of TS `RuntimeTaskType`: `local_bash` / `local_agent`.
    /// TaskOutput's projection picks task_type from it, TaskStop picks "kill the tree or set the cancel flag" from it.
    pub task_type: &'static str,
    /// The `description` from the tool input (TS's notification subject prefers it, then command).
    pub description: Option<String>,
    /// Whether the completion notification was already claimed (TS `notified` claim: once TaskOutput read the terminal state it never notifies again).
    pub notified: bool,
    /// Cancel flag of a non-process background task (a subagent)—pointing straight at the child session's cancel switch,
    /// once set the child turn stops at its own checkpoint; a bash task is None (killed by pid tree).
    pub cancel: Option<Arc<AtomicBool>>,
}

/// Closed set of background task kinds (the `task_type` values TaskOutput accepts).
pub const TASK_TYPE_LOCAL_BASH: &str = "local_bash";
pub const TASK_TYPE_LOCAL_AGENT: &str = "local_agent";

/// Arguments of `spawn_bash` (more than clippy's limit of 7, passed as a group).
pub struct BashSpawn<'a> {
    /// The interpreter together with its env overlay: the argv shape and the overlay must come from the same parse result.
    pub shell: &'a crate::shell::Shell,
    pub command: &'a str,
    pub description: Option<&'a str>,
    pub cwd: &'a Path,
    pub tool_call_id: Option<&'a str>,
    pub now_ms: i64,
}

/// The lifecycle of one already-started but not yet registered foreground Bash (the `preparing/foreground` pair of auto-on-timeout).
pub struct BashLifecycle {
    pub work_id: String,
    pub output_path: PathBuf,
    pub pid: u32,
    pub command: String,
    pub description: Option<String>,
    pub tool_call_id: Option<String>,
    pub started_at: i64,
    /// The exit fact emitted once when the process settles itself (background progression is driven by it).
    pub completion: Receiver<BashLifecycleSettle>,
}

/// Exit fact: `code = None ∧ error = Some` means wait failed, not killed by a signal (which is also None).
#[derive(Clone, Debug)]
pub struct BashLifecycleSettle {
    pub code: Option<i32>,
    pub error: Option<String>,
}

impl BashLifecycleSettle {
    pub fn status(&self) -> &'static str {
        if self.code == Some(0) && self.error.is_none() {
            BG_COMPLETED
        } else {
            BG_FAILED
        }
    }
}

/// Session-level background task registry. `root` is this session's `background/` directory.
pub struct BackgroundRegistry {
    root: PathBuf,
    session_id: String,
    counter: AtomicU64,
    works: Mutex<VecDeque<BackgroundWork>>,
    /// Delivery point of completion notifications (injected by the host when the session is created; None = no notification, e.g. library-level unit tests).
    notifier: Mutex<Option<Arc<dyn crate::notifications::BackgroundNotifier>>>,
    /// Delivery point of the tool row's `outputPreview` (injected once at session construction).
    preview_sink: Mutex<Option<Arc<dyn ToolOutputPreviewSink>>>,
}

impl std::fmt::Debug for BackgroundRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BackgroundRegistry")
            .field("root", &self.root)
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

/// Retention cap: hundreds of background commands in one session is not a real usage pattern, and unbounded growth is a leak.
pub const RETAINED_WORKS: usize = 64;

impl BackgroundRegistry {
    pub fn new(session_id: String, root: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            root,
            session_id,
            counter: AtomicU64::new(0),
            works: Mutex::new(VecDeque::new()),
            notifier: Mutex::new(None),
            preview_sink: Mutex::new(None),
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Output directory (`sessions/<id>/background`). Registration and the read side share one source instead of each rebuilding the path.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn next_work_id(&self) -> String {
        let sequence = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        format!("bash-{}-{sequence}", self.session_id)
    }

    /// Start a command in background mode: stdout/stderr are merged into the task's own file and the process survives outside this
    /// round (turn cancellation does not kill it, same semantics as TS's `shouldRetainExecutionAfterRootExit`).
    ///
    /// Returns `(work_id, output_path)`. A spawn failure still leaves a `spawn_error` record—the detail panel
    /// must be able to answer "why is it empty" instead of offering silence.
    pub fn spawn_bash(self: &Arc<Self>, spawn: BashSpawn<'_>) -> (String, PathBuf) {
        let BashSpawn {
            shell,
            command,
            description,
            cwd,
            tool_call_id,
            now_ms,
        } = spawn;
        let work_id = self.next_work_id();
        let output_path = self.root.join(format!("{work_id}.log"));
        let args = shell.argv(command);
        let started = if std::fs::create_dir_all(&self.root).is_err() {
            Err("create output dir failed".to_string())
        } else {
            Self::spawn_into(shell, &args, cwd, &output_path)
        };
        match started {
            Err(error) => self.record(BackgroundWork {
                work_id: work_id.clone(),
                command: command.to_string(),
                output_path: output_path.clone(),
                status: BG_SPAWN_ERROR,
                started_at: now_ms,
                ended_at: Some(now_ms),
                exit_code: None,
                error: Some(error),
                pid: None,
                tool_call_id: tool_call_id.map(str::to_string),
                task_type: TASK_TYPE_LOCAL_BASH,
                description: description.map(str::to_string),
                notified: false,
                cancel: None,
            }),
            Ok(mut child) => {
                let pid = child.id();
                self.record(BackgroundWork {
                    work_id: work_id.clone(),
                    command: command.to_string(),
                    output_path: output_path.clone(),
                    status: BG_RUNNING,
                    started_at: now_ms,
                    ended_at: None,
                    exit_code: None,
                    error: None,
                    pid: Some(pid),
                    tool_call_id: tool_call_id.map(str::to_string),
                    task_type: TASK_TYPE_LOCAL_BASH,
                    description: description.map(str::to_string),
                    notified: false,
                    cancel: None,
                });
                // The watchdog thread waits for exit and settles. TS uses a completion promise in the event loop; here it is
                // a thread doing a blocking wait. Common ground: the process drives its own state, it is not "updated when someone queries".
                let registry = Arc::clone(self);
                let id = work_id.clone();
                std::thread::spawn(move || {
                    let code = child.wait().ok().and_then(|status| status.code());
                    let settled = if code == Some(0) {
                        BG_COMPLETED
                    } else {
                        BG_FAILED
                    };
                    registry.settle(&id, settled, code);
                    // Completion notification: the claim guarantees one task notifies at most once; if TaskOutput read the terminal state first it
                    // already claimed, so this naturally noops.
                    registry.notify_settled(&id);
                });
            }
        }
        (work_id, output_path)
    }

    /// Start a **not yet registered** foreground Bash lifecycle (TS
    /// `runBashWithBackgroundLifecycle`'s `preparing/foreground` pair). From the first millisecond the process
    /// writes merged output into the task file and detaches from the calling thread; past the foreground deadline
    /// [`BackgroundRegistry::promote_to_background`] turns it into a background task atomically. Before that it is not part of
    /// the registry—`TaskOutput` cannot see it and cancellation cannot reach it, matching TS's "no background task before commit".
    pub fn spawn_bash_lifecycle(
        self: &Arc<Self>,
        spawn: BashSpawn<'_>,
        turn_id: Option<&str>,
    ) -> Result<BashLifecycle, String> {
        let BashSpawn {
            shell,
            command,
            description,
            cwd,
            tool_call_id,
            now_ms,
        } = spawn;
        let work_id = self.next_work_id();
        let output_path = self.root.join(format!("{work_id}.log"));
        std::fs::create_dir_all(&self.root)
            .map_err(|error| format!("create output dir failed: {error}"))?;
        let args = shell.argv(command);
        let mut child = Self::spawn_into(shell, &args, cwd, &output_path)?;
        let pid = child.id();
        let (settle_tx, completion) = mpsc::channel();
        // Progress and exit share one watchdog thread: check for exit every 50 ms; after 2 s read the task file tail every 1 s and
        // hand a TS `buildBashOutputPreview`-shaped preview to the injected delivery point.
        let sink = self.preview_sink();
        let progress_turn = turn_id.map(str::to_string);
        let progress_tool = tool_call_id.map(str::to_string);
        let progress_path = output_path.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let mut next_progress = started + Duration::from_millis(PROGRESS_THRESHOLD_MS);
            let mut previous_lines = 0usize;
            let settle = loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        break BashLifecycleSettle {
                            code: status.code(),
                            error: None,
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        break BashLifecycleSettle {
                            code: None,
                            error: Some(format!("wait: {error}")),
                        }
                    }
                }
                let now = Instant::now();
                if now >= next_progress {
                    next_progress = now + Duration::from_millis(PROGRESS_INTERVAL_MS);
                    if let (Some(sink), Some(turn), Some(tool)) =
                        (&sink, &progress_turn, &progress_tool)
                    {
                        if let Ok((text, bytes_read, total_bytes)) =
                            read_tail_with_bytes(&progress_path, PROGRESS_TAIL_MAX_BYTES)
                        {
                            let preview = build_output_preview(
                                &text,
                                bytes_read,
                                total_bytes,
                                previous_lines,
                            );
                            previous_lines = preview["totalLines"].as_u64().unwrap_or(0) as usize;
                            sink.output_preview(turn, tool, preview);
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            };
            let _ = settle_tx.send(settle);
        });
        Ok(BashLifecycle {
            work_id,
            output_path,
            pid,
            command: command.to_string(),
            description: description.map(str::to_string),
            tool_call_id: tool_call_id.map(str::to_string),
            started_at: now_ms,
            completion,
        })
    }

    /// Foreground deadline reached: hand the lifecycle to the background **atomically**—registration happens at that moment (`TaskOutput`/cancellation become
    /// visible from then on), and the completion notification is sent at settle time (TS `commitBackground`'s registration + detaching from the parent abort).
    pub fn promote_to_background(self: &Arc<Self>, lifecycle: BashLifecycle) {
        let BashLifecycle {
            work_id,
            output_path,
            pid,
            command,
            description,
            tool_call_id,
            started_at,
            completion,
        } = lifecycle;
        self.record(BackgroundWork {
            work_id: work_id.clone(),
            command,
            output_path,
            status: BG_RUNNING,
            started_at,
            ended_at: None,
            exit_code: None,
            error: None,
            pid: Some(pid),
            tool_call_id,
            task_type: TASK_TYPE_LOCAL_BASH,
            description,
            notified: false,
            cancel: None,
        });
        let registry = Arc::clone(self);
        std::thread::spawn(move || {
            match completion.recv() {
                Ok(settle) => registry.settle(&work_id, settle.status(), settle.code),
                Err(_) => registry.settle(&work_id, BG_FAILED, None),
            }
            // The settle notification is chained after settle: once claimed (TaskOutput already read the terminal state) the model is not bothered again.
            registry.notify_settled(&work_id);
        });
    }

    fn spawn_into(
        shell: &crate::shell::Shell,
        args: &[String],
        cwd: &Path,
        output_path: &Path,
    ) -> Result<std::process::Child, String> {
        // The output file must not be opened in append mode: when MSYS2/Git Bash gets a stdout handle that only permits FILE_APPEND_DATA it
        // exits immediately with code 1 writing nothing (the same command works with a write-mode handle).
        // Each work_id writes its own new file, and stdout/stderr are duplicated handles of the same File
        // (sharing the file pointer), so dropping append does not change the "both sides append into one log" semantics.
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(output_path)
            .map_err(|error| format!("open output file: {error}"))?;
        let stderr = file
            .try_clone()
            .map_err(|error| format!("clone output file: {error}"))?;
        let mut command = Command::new(shell.program());
        shell.apply(&mut command);
        command
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(file))
            .stderr(Stdio::from(stderr))
            .spawn()
            .map_err(|error| format!("spawn: {error}"))
    }

    pub fn record(&self, work: BackgroundWork) {
        if let Ok(mut works) = self.works.lock() {
            works.push_back(work);
            while works.len() > RETAINED_WORKS {
                works.pop_front();
            }
        }
    }

    /// Called by the async task after a subagent completes (the settle entry for non-process tasks).
    pub fn settle_agent(&self, work_id: &str, status: &'static str) {
        self.settle(work_id, status, None);
    }

    /// Register a background subagent (work_id = agentId = child session id; TS's
    /// `backgroundTaskId`/`agentId` are the same value in this host, used by TaskOutput/TaskStop to locate it).
    pub fn record_agent(
        &self,
        work_id: &str,
        description: &str,
        output_path: PathBuf,
        tool_call_id: Option<&str>,
        cancel: Arc<AtomicBool>,
        started_at: i64,
    ) {
        self.record(BackgroundWork {
            work_id: work_id.to_string(),
            command: description.to_string(),
            output_path,
            status: BG_RUNNING,
            started_at,
            ended_at: None,
            exit_code: None,
            error: None,
            pid: None,
            tool_call_id: tool_call_id.map(str::to_string),
            task_type: TASK_TYPE_LOCAL_AGENT,
            description: None,
            notified: false,
            cancel: Some(cancel),
        });
    }

    fn settle(&self, work_id: &str, status: &'static str, exit_code: Option<i32>) {
        if let Ok(mut works) = self.works.lock() {
            if let Some(work) = works.iter_mut().find(|work| work.work_id == work_id) {
                // Advance only from running: a cancellation/timeout that landed first must not be overwritten by a later exit code.
                if work.status != BG_RUNNING {
                    return;
                }
                work.status = status;
                work.exit_code = exit_code;
                work.ended_at = Some(crate::store::now_ms());
            }
        }
    }

    /// Mark as cancelled and kill the process tree. Repeated calls noop (a late stop should not refresh the terminal timestamp).
    pub fn mark_cancelled(&self, work_id: &str) -> bool {
        let (pid, cancel) = match self.take_cancel(work_id) {
            Some(value) => value,
            None => return false,
        };
        // Set the state first, then kill the process/set the flag: the other way round, an observer mid-kill would read "still running"
        // and wait forever for a settle.
        if let Some(cancel) = cancel {
            cancel.store(true, Ordering::SeqCst);
        }
        if let Some(pid) = pid {
            let _ = nc_code_proc::kill_tree(pid, KILL_TREE_GRACE_MS);
        }
        true
    }

    /// Advance only the state and hand the pid to the caller (no slow operation like killing a process inside the lock).
    /// `None` = unknown task or already terminal → cancellation is a noop.
    fn take_cancel(&self, work_id: &str) -> Option<(Option<u32>, Option<Arc<AtomicBool>>)> {
        let mut works = self.works.lock().ok()?;
        let work = works.iter_mut().find(|work| work.work_id == work_id)?;
        if work.status != BG_RUNNING {
            return None;
        }
        work.status = BG_CANCELLED;
        work.ended_at = Some(crate::store::now_ms());
        Some((work.pid, work.cancel.clone()))
    }

    /// Reap before process exit: every still-running background command is tree-killed and recorded as cancelled.
    /// Otherwise, once the host process leaves, a `cmd /C` derived tree on Windows keeps running inside the session (orphan processes).
    pub fn stop_all(&self) -> usize {
        let running: Vec<String> = self
            .works
            .lock()
            .map(|works| {
                works
                    .iter()
                    .filter(|work| work.status == BG_RUNNING)
                    .map(|work| work.work_id.clone())
                    .collect()
            })
            .unwrap_or_default();
        running
            .iter()
            .filter(|work_id| self.mark_cancelled(work_id))
            .count()
    }

    /// Whether this toolCall handed the command to the background, and to which workId.
    /// Present = the row is `backgrounded` (the `backgrounded`/`workId` pair on TS's toolCall row).
    pub fn work_id_for_tool_call(&self, tool_call_id: &str) -> Option<String> {
        self.works.lock().ok().and_then(|works| {
            works
                .iter()
                .find(|work| work.tool_call_id.as_deref() == Some(tool_call_id))
                .map(|work| work.work_id.clone())
        })
    }

    /// Inject the delivery point of completion notifications (set once by the host when the session is created).
    pub fn set_notifier(&self, notifier: Arc<dyn crate::notifications::BackgroundNotifier>) {
        if let Ok(mut guard) = self.notifier.lock() {
            *guard = Some(notifier);
        }
    }

    /// Emit one running-row preview from a foreground Bash's live buffer. The
    /// background watchdog calls the same sink through `spawn_bash_lifecycle`;
    /// foreground commands read their pipes and call this at the same cadence.
    pub fn emit_output_preview(&self, turn_id: &str, tool_call_id: &str, preview: Value) {
        if let Some(sink) = self.preview_sink() {
            sink.output_preview(turn_id, tool_call_id, preview);
        }
    }

    /// Inject the delivery point of the tool row's `outputPreview` (set once at session construction; weak reference, no cycle).
    pub fn set_preview_sink(&self, sink: Arc<dyn ToolOutputPreviewSink>) {
        if let Ok(mut guard) = self.preview_sink.lock() {
            *guard = Some(sink);
        }
    }

    fn preview_sink(&self) -> Option<Arc<dyn ToolOutputPreviewSink>> {
        self.preview_sink
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    fn notifier(&self) -> Option<Arc<dyn crate::notifications::BackgroundNotifier>> {
        self.notifier.lock().ok().and_then(|guard| guard.clone())
    }

    /// Claim the completion notification: one task notifies at most once (TS `notified` claim).
    /// `TaskOutput` reading the terminal state also claims, to keep "tool result + completion notification" from reaching the model twice.
    pub fn claim_notification(&self, work_id: &str) -> Option<BackgroundWork> {
        let mut works = self.works.lock().ok()?;
        let work = works.iter_mut().find(|work| work.work_id == work_id)?;
        if work.notified || work.status == BG_RUNNING {
            return None;
        }
        work.notified = true;
        Some(work.clone())
    }

    /// After settling, try to deliver one completion notification (no delivery point / already claimed → noop).
    pub fn notify_settled(&self, work_id: &str) {
        let Some(notifier) = self.notifier() else {
            return;
        };
        if let Some(work) = self.claim_notification(work_id) {
            notifier.notify(&work);
        }
    }

    /// Remove one registration (a subagent resume deletes the old terminal entry before re-registering, so `get` does not read the stale one).
    pub fn remove(&self, work_id: &str) {
        if let Ok(mut works) = self.works.lock() {
            works.retain(|work| work.work_id != work_id);
        }
    }

    pub fn get(&self, work_id: &str) -> Option<BackgroundWork> {
        self.works
            .lock()
            .ok()
            .and_then(|works| works.iter().find(|work| work.work_id == work_id).cloned())
    }

    pub fn list(&self) -> Vec<BackgroundWork> {
        self.works
            .lock()
            .map(|works| works.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Output of `v4/conversation/backgroundBashOutput`.
    ///
    /// Three criteria TS states explicitly:
    /// - **Freeze the state before reading the file**: if the process exits while the file is being read, the next query returns the terminal state together with
    ///   its final tail window; reading first would pair a `completed` with an incomplete output.
    /// - **Not settled means still running**: even when cancellation was requested, the detail must keep reading to the tail.
    /// - Unknown workId → `unavailable`, an observation query does **not** revive a cold session.
    pub fn output_snapshot(&self, work_id: &str) -> Value {
        let Some(work) = self.get(work_id) else {
            return json!({"kind": "unavailable", "workId": work_id});
        };
        match read_tail(&work.output_path, BACKGROUND_BASH_OUTPUT_MAX_BYTES) {
            Ok((text, truncated)) => json!({
                "kind": "output",
                "workId": work_id,
                "status": work.status,
                "output": text,
                "truncated": truncated,
                "outputPath": work.output_path.to_string_lossy(),
            }),
            Err(code) => json!({"kind": "read_failed", "workId": work_id, "code": code}),
        }
    }

    /// Tail read used by `TaskOutput`: the budget differs from the wire detail surface (a tool result has its own
    /// `TASK_OUTPUT_DEFAULT_LENGTH`), but the truncation rule is the same (byte cap + UTF-8 boundary).
    /// An unreadable file (missing/permission) returns an empty string—the task's other facts (status, exit code) are still answered.
    pub fn read_output(&self, work_id: &str, max_bytes: usize) -> (String, bool) {
        let Some(work) = self.get(work_id) else {
            return (String::new(), false);
        };
        read_tail(&work.output_path, max_bytes).unwrap_or_default()
    }
}

/// Read the tail `max_bytes` of a file; the cut point walks back to a UTF-8 character boundary.
fn read_tail(path: &Path, max_bytes: usize) -> Result<(String, bool), String> {
    let (text, bytes_read, total) = read_tail_with_bytes(path, max_bytes)?;
    Ok((text, bytes_read < total))
}

/// A tail read reports both "bytes actually read" and the file's total byte count — the progress preview's line-count estimate needs these two facts.
fn read_tail_with_bytes(path: &Path, max_bytes: usize) -> Result<(String, usize, usize), String> {
    let bytes = std::fs::read(path).map_err(|error| io_code(&error))?;
    let total = bytes.len();
    if total <= max_bytes {
        return Ok((String::from_utf8_lossy(&bytes).to_string(), total, total));
    }
    let mut start = total - max_bytes;
    while start < total && is_utf8_continuation(&bytes, start) {
        start += 1;
    }
    Ok((
        String::from_utf8_lossy(&bytes[start..]).to_string(),
        total - start,
        total,
    ))
}

/// TS `buildBashOutputPreview`: the last 5 lines are the short preview and the last 100 lines the full preview; when only
/// the file tail was read, the total line count is estimated as "bytes read : total bytes" and marked `linesEstimated`.
pub fn build_output_preview(
    text: &str,
    bytes_read: usize,
    total_bytes: usize,
    previous_lines: usize,
) -> Value {
    let mut cursor = text.len();
    let mut short_start = 0;
    let mut full_start = 0;
    let mut lines = 0usize;
    while cursor > 0 {
        cursor = text[..cursor].rfind('\n').unwrap_or(0);
        lines += 1;
        if lines == SHORT_PREVIEW_LINES {
            short_start = if cursor == 0 { 0 } else { cursor + 1 };
        }
        if lines == FULL_PREVIEW_LINES {
            full_start = if cursor == 0 { 0 } else { cursor + 1 };
        }
    }
    let lines_estimated = bytes_read > 0 && bytes_read < total_bytes;
    let total_lines = if bytes_read == 0 {
        previous_lines
    } else if lines_estimated {
        let estimate = ((total_bytes as f64 / bytes_read as f64) * lines as f64).round() as usize;
        estimate.max(previous_lines)
    } else {
        lines
    };
    json!({
        "text": &text[short_start..],
        "fullText": &text[full_start..],
        "totalLines": total_lines,
        "totalBytes": total_bytes,
        "linesEstimated": lines_estimated,
    })
}

/// The cut point may only land on the leading byte of a UTF-8 sequence, otherwise `from_utf8_lossy` injects U+FFFD.
fn is_utf8_continuation(bytes: &[u8], index: usize) -> bool {
    bytes
        .get(index)
        .is_some_and(|byte| (byte & 0b1100_0000) == 0b1000_0000)
}

fn io_code(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => "ENOENT".to_string(),
        std::io::ErrorKind::PermissionDenied => "EACCES".to_string(),
        _ => format!("{:?}", error.kind()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zcode-bg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn registry(dir: &Path) -> Arc<BackgroundRegistry> {
        BackgroundRegistry::new("ses-1".to_string(), dir.join("background"))
    }

    #[test]
    fn output_preview_keeps_the_last_five_lines_and_counts_them_like_ts() {
        let text = "1\n2\n3\n4\n5\n6\n7\n";
        let preview = build_output_preview(text, text.len(), text.len(), 0);
        // TS counts newlines backwards from the end: 7 digits + the trailing newline = 8 "lines", and the short preview takes the last 5.
        assert_eq!(preview["text"], json!("4\n5\n6\n7\n"));
        assert_eq!(preview["fullText"], json!(text));
        assert_eq!(preview["totalLines"], json!(8));
        assert_eq!(preview["totalBytes"], json!(text.len()));
        assert_eq!(preview["linesEstimated"], json!(false));
    }

    #[test]
    fn output_preview_caps_the_full_text_at_one_hundred_lines() {
        let text: String = (1..=120).map(|line| format!("{line}\n")).collect();
        let preview = build_output_preview(&text, text.len(), text.len(), 0);
        let full = preview["fullText"].as_str().expect("fullText");
        assert!(
            full.starts_with("22\n"),
            "last 100 lines start here: {full}"
        );
        assert_eq!(preview["totalLines"], json!(121));
    }

    #[test]
    fn output_preview_estimates_total_lines_from_a_bounded_tail_read() {
        // The tail read only got 2 bytes out of 100 total: the line count is estimated by the byte ratio and flagged linesEstimated.
        let preview = build_output_preview("a\n", 2, 100, 5);
        assert_eq!(preview["linesEstimated"], json!(true));
        assert_eq!(preview["totalLines"], json!(100));
        assert_eq!(preview["totalBytes"], json!(100));
        // Not a single byte read: reuse the previous total line count instead of resetting the line count to zero.
        let empty = build_output_preview("", 0, 100, 7);
        assert_eq!(empty["totalLines"], json!(7));
        assert_eq!(empty["linesEstimated"], json!(false));
    }

    #[test]
    fn lifecycle_progress_reaches_the_sink_and_stops_after_settle() {
        let dir = temp_dir("preview-sink");
        let registry = registry(&dir);
        let previews: Arc<Mutex<Vec<(String, String, Value)>>> = Arc::default();
        struct Collect(Arc<Mutex<Vec<(String, String, Value)>>>);
        impl ToolOutputPreviewSink for Collect {
            fn output_preview(&self, turn_id: &str, tool_call_id: &str, preview: Value) {
                self.0.lock().expect("previews").push((
                    turn_id.to_string(),
                    tool_call_id.to_string(),
                    preview,
                ));
            }
        }
        registry.set_preview_sink(Arc::new(Collect(Arc::clone(&previews))));
        let started = std::time::Instant::now();
        let lifecycle = registry
            .spawn_bash_lifecycle(
                BashSpawn {
                    shell: crate::shell::shell(),
                    command: "node -e \"console.log('preview-probe'); setTimeout(() => {}, 2600)\"",
                    description: None,
                    cwd: &dir,
                    tool_call_id: Some("call-preview"),
                    now_ms: crate::store::now_ms(),
                },
                Some("turn-preview"),
            )
            .expect("lifecycle spawn");
        let settle = lifecycle
            .completion
            .recv_timeout(Duration::from_secs(30))
            .expect("settle");
        assert_eq!(settle.code, Some(0));
        assert!(started.elapsed() >= Duration::from_millis(2_400));
        let collected = previews.lock().expect("previews");
        assert!(!collected.is_empty(), "2 s threshold must have fired");
        assert!(collected
            .iter()
            .all(|(turn, tool, _)| turn == "turn-preview" && tool == "call-preview"));
        assert!(
            collected
                .last()
                .and_then(|(_, _, preview)| preview["totalBytes"].as_u64())
                .is_some_and(|bytes| bytes > 0),
            "a preview must carry real output bytes: {collected:?}"
        );
        let count = collected.len();
        drop(collected);
        std::thread::sleep(Duration::from_millis(1_300));
        assert_eq!(
            previews.lock().expect("previews").len(),
            count,
            "no progress after the process settled"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The very same shell selection rule as `tools/bash.rs` (Windows `cmd /D /C`, everywhere else `sh -c`).
    fn spawn(registry: &Arc<BackgroundRegistry>, command: &str, dir: &Path) -> String {
        // Use the production parser instead of rebuilding a cmd/sh branch inside the test: these cases are also
        // evidence that the parsed shell can really be spawned.
        let (work_id, _) = registry.spawn_bash(BashSpawn {
            shell: crate::shell::shell(),
            command,
            description: None,
            cwd: dir,
            tool_call_id: None,
            now_ms: 1,
        });
        work_id
    }

    fn wait_settled(registry: &Arc<BackgroundRegistry>, work_id: &str) -> BackgroundWork {
        for _ in 0..200 {
            let work = registry.get(work_id).expect("registered");
            if work.status != BG_RUNNING {
                return work;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("background work {work_id} never settled");
    }

    #[test]
    fn an_unknown_work_is_unavailable_and_observation_creates_nothing() {
        let dir = temp_dir("unavailable");
        let registry = registry(&dir);
        assert_eq!(
            registry.output_snapshot("bash-ses-1-9"),
            json!({"kind": "unavailable", "workId": "bash-ses-1-9"})
        );
        // A single observation query must not create the output directory: the observation surface revives nothing.
        assert!(!dir.join("background").exists());
    }

    #[test]
    fn a_real_background_command_settles_and_its_output_is_readable() {
        let dir = temp_dir("run");
        let registry = registry(&dir);
        let work_id = spawn(&registry, "echo bg-probe-output", &dir);
        assert_eq!(registry.get(&work_id).unwrap().status, BG_RUNNING);
        let work = wait_settled(&registry, &work_id);
        let snapshot = registry.output_snapshot(&work_id);
        assert_eq!(snapshot["kind"], json!("output"));
        assert_eq!(snapshot["status"], json!("completed"), "{snapshot}");
        assert_eq!(snapshot["workId"], json!(work_id));
        assert_eq!(snapshot["truncated"], json!(false));
        assert!(
            snapshot["output"]
                .as_str()
                .unwrap_or("")
                .contains("bg-probe-output"),
            "the detail pane must read what the command printed: {snapshot}"
        );
        assert_eq!(
            snapshot["outputPath"].as_str().unwrap(),
            work.output_path.to_string_lossy(),
            "outputPath is how the model can fetch more with Read"
        );
    }

    #[test]
    fn a_failing_command_settles_as_failed_with_its_exit_code() {
        let dir = temp_dir("fail");
        let registry = registry(&dir);
        let work_id = spawn(&registry, "exit 3", &dir);
        let work = wait_settled(&registry, &work_id);
        assert_eq!(work.status, BG_FAILED);
        assert_eq!(work.exit_code, Some(3));
        assert!(work.ended_at.is_some());
    }

    #[test]
    fn cancel_wins_and_is_not_overwritten_by_the_exit_or_a_second_cancel() {
        let dir = temp_dir("cancel");
        let registry = registry(&dir);
        let work_id = spawn(&registry, "echo x", &dir);
        assert!(registry.mark_cancelled(&work_id));
        assert!(
            !registry.mark_cancelled(&work_id),
            "repeat cancel is a noop"
        );
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert_eq!(registry.get(&work_id).unwrap().status, BG_CANCELLED);
    }

    #[test]
    fn the_tail_is_bounded_and_never_splits_a_multibyte_character() {
        let dir = temp_dir("tail");
        std::fs::create_dir_all(dir.join("background")).expect("dir");
        let path = dir.join("background").join("bash-ses-1-1.log");
        // Each 'あ' is 3 bytes: pushing the cut point inside a sequence would fall out as U+FFFD.
        std::fs::write(&path, "あ".repeat(4_000).as_bytes()).expect("write log");
        let registry = registry(&dir);
        registry.record(BackgroundWork {
            work_id: "bash-ses-1-1".to_string(),
            command: "cat".to_string(),
            output_path: path,
            status: BG_COMPLETED,
            started_at: 1,
            ended_at: Some(2),
            exit_code: Some(0),
            error: None,
            pid: None,
            tool_call_id: None,
            task_type: TASK_TYPE_LOCAL_BASH,
            description: None,
            notified: false,
            cancel: None,
        });
        let snapshot = registry.output_snapshot("bash-ses-1-1");
        assert_eq!(snapshot["truncated"], json!(true));
        let text = snapshot["output"].as_str().unwrap();
        assert!(
            !text.contains(char::REPLACEMENT_CHARACTER),
            "a split multibyte character must never reach the client"
        );
        assert!(
            text.chars().all(|c| c == 'あ'),
            "the tail must be whole characters"
        );
        assert!(text.len() <= BACKGROUND_BASH_OUTPUT_MAX_BYTES);
    }

    #[test]
    fn a_missing_output_file_reports_read_failed_with_a_code() {
        let dir = temp_dir("readfail");
        let registry = registry(&dir);
        registry.record(BackgroundWork {
            work_id: "bash-ses-1-7".to_string(),
            command: "echo x".to_string(),
            output_path: dir.join("background").join("missing.log"),
            status: BG_RUNNING,
            started_at: 1,
            ended_at: None,
            exit_code: None,
            error: None,
            pid: None,
            tool_call_id: None,
            task_type: TASK_TYPE_LOCAL_BASH,
            description: None,
            notified: false,
            cancel: None,
        });
        assert_eq!(
            registry.output_snapshot("bash-ses-1-7"),
            json!({"kind": "read_failed", "workId": "bash-ses-1-7", "code": "ENOENT"})
        );
    }

    #[test]
    fn retention_bounds_the_registry_without_dropping_the_newest() {
        let dir = temp_dir("retention");
        let registry = registry(&dir);
        for index in 0..(RETAINED_WORKS + 10) {
            registry.record(BackgroundWork {
                work_id: format!("bash-ses-1-{index}"),
                command: "x".to_string(),
                output_path: dir.join("background").join("x.log"),
                status: BG_COMPLETED,
                started_at: index as i64,
                ended_at: None,
                exit_code: None,
                error: None,
                pid: None,
                tool_call_id: None,
                task_type: TASK_TYPE_LOCAL_BASH,
                description: None,
                notified: false,
                cancel: None,
            });
        }
        let works = registry.list();
        assert_eq!(works.len(), RETAINED_WORKS);
        assert_eq!(
            works.last().unwrap().work_id,
            format!("bash-ses-1-{}", RETAINED_WORKS + 9)
        );
    }

    #[test]
    fn stop_all_cancels_every_still_running_work_exactly_once() {
        let dir = temp_dir("stopall");
        let registry = registry(&dir);
        // A command that outlives the whole assertion window (Windows times with ping; a container does not necessarily have sleep).
        let long = if cfg!(windows) {
            "ping -n 20 127.0.0.1"
        } else {
            "sleep 20"
        };
        let work_id = spawn(&registry, long, &dir);
        assert_eq!(registry.get(&work_id).unwrap().status, BG_RUNNING);
        assert_eq!(registry.stop_all(), 1);
        assert_eq!(registry.get(&work_id).unwrap().status, BG_CANCELLED);
        assert_eq!(
            registry.stop_all(),
            0,
            "already terminal work is not counted twice"
        );
    }
}
