//! The complete life cycle of the kernel process: spawn, NDJSON request/response association, timeout/cancel upgrade, recycling.
//!
//! Threading model (crate documentation, ENTRY 017): This crate does not introduce tokio. The kernel comes with two
//! Background thread (stdout reader / stderr drain), the caller calls it in the blocking thread pool
//! [`NodeKernel::evaluate`] and other entries; asynchrony is reflected at the child process boundary.
//!
//! Two-level semantics for timeout/cancellation (mirror TS: server.ts:216-228 outer layer AbortSignal.timeout +
//! executors.ts:59-95 vm synchronization budget and abort race; kernel_driver.mjs:732-764
//! After receiving cancel, reset and return RESET_SUFFIX error frame):
//! 1. The total time is up → send cancel(TimeoutError), wait [`KERNEL_CANCEL_GRACE_MS`]——
//!    The driver discards the context, rebuilds it, and returns an error frame (message with kernel reset prompt).
//!    This frame is the legal result of completed execution and is handed over to the caller as it is (the model must read reset semantics,
//!    Align node-repl-session.ts:424-437 and put error into the run result instead of throwing it out);
//!    If the normal result arrives first within the grace window, the same is true (the execution is indeed completed, which is more honest than false reporting of timeout).
//! 2. There is no response within the grace period (kernel cannot even handle cancel) → kill_tree upgrade kill,
//!    Returns TimedOut/Cancelled, leaving no orphan Node (the child process of user code spawn is also recycled).
//!
//! Kernel death (process.exit/crash/fatal frame/stdout EOF) replaces all waiting methods with
//! KernelDied settlement, error with stderr tail (diagnosis is not lost and bounded - the same as TS STDERR_LIMIT).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::driver;
use crate::error::NodeReplError;
use crate::protocol::{
    parse_inbound, AddModuleDirFrame, CancelFrame, CancelReason, EvaluateFrame, InboundFrame,
    KernelOutput, ReadyEvent, SimpleFrame, KERNEL_PROTOCOL,
};

/// The grace period to wait for driver settlement frame after cancel is issued (TS node-repl.ts:403
/// `timeout.cleanupGraceMs = 2_000`: Cleanup window after timeout/cancellation).
pub const KERNEL_CANCEL_GRACE_MS: u64 = 2_000;
/// stderr collection limit (TS `harness.ts:163 STDERR_LIMIT = 64 * 1024`).
const STDERR_LIMIT: usize = 64 * 1024;
/// The upper limit of reading a single frame line: exceeding the limit is settled as death according to protocol violation, and there is no unbounded buffering.
const MAX_FRAME_LINE_BYTES: usize = 16 * 1024 * 1024;
/// Polling slices when waiting for frames: allow the cancel flag to be observed in time during blocking wait.
const WAIT_SLICE: Duration = Duration::from_millis(25);
/// Response window for non-evaluate requests (status/reset/shutdown/addModuleDir).
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

/// Kernel startup parameters.
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    /// Node executable (Available.exe from capability::probe).
    pub node_exe: PathBuf,
    /// Kernel's cwd; None inherits the current process (the driver uses cwd as the require benchmark,
    /// kernel_driver.mjs:323).
    pub cwd: Option<PathBuf>,
    /// Pass `--restrict-process`: sandbox process changes to restricted facade
    /// (kernel_driver.mjs:37-75; TS host constant true, server.ts:139).
    pub restrict_process: bool,
    /// Additional module parsing directories (one for each `--module-dir`, kernel_driver.mjs:40-45).
    pub module_dirs: Vec<PathBuf>,
    /// The upper limit of waiting for ready frames.
    pub ready_timeout: Duration,
    /// driver materialized directory root; None uses the system temporary directory.
    pub driver_dir: Option<PathBuf>,
}

impl SpawnOptions {
    pub fn new(node_exe: impl Into<PathBuf>) -> Self {
        Self {
            node_exe: node_exe.into(),
            cwd: None,
            restrict_process: true,
            module_dirs: Vec::new(),
            ready_timeout: Duration::from_secs(10),
            driver_dir: None,
        }
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn module_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.module_dirs.push(dir.into());
        self
    }
}

/// The observable state of the kernel (ready handshake fact + status response).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelStatus {
    pub pid: u32,
    pub node_version: String,
    pub generation: u64,
    pub busy: bool,
    pub restrict_process: bool,
    pub module_dirs: Vec<String>,
}
/// Clone means sharing the same kernel handle. The host holds it by session key;
/// The tool layer in fresh mode is created every time and shut down after use.
pub type KernelHandle = Arc<NodeKernel>;

/// Kernel process handle. One instance = one Node child process + two background threads.
///
/// All methods are blocking (writing to stdin, waiting for frames, waiting for the process to exit); corresponding to the host side
/// `spawn_blocking`. `child` is only touched by the methods of this struct - the reader thread does not touch it
/// (EOF only sets the death flag) to avoid "EOF lock wait" and "kill_now lock kill" from being equivalent.
pub struct NodeKernel {
    inner: Arc<KernelInner>,
    child: Mutex<Child>,
    driver_dir: PathBuf,
    next_id: AtomicU64,
}

struct KernelInner {
    stdin: Mutex<ChildStdin>,
    /// id → one-time response channel for this request.
    pending: Mutex<HashMap<u64, mpsc::Sender<InboundFrame>>>,
    /// ready handshake fact (always has value after spawn is successful).
    ready: Mutex<Option<ReadyEvent>>,
    /// Cause of death (fatal frame copy / stdout EOF).
    death: Mutex<Option<String>>,
    /// stderr diagnostic tail (bounded 64 KiB).
    stderr: Mutex<Vec<u8>>,
    alive: AtomicBool,
}

impl NodeKernel {
    /// Start kernel: materialized driver → spawn Node with parameter array (no shell) → start reader/stderr
    /// Both threads → wait for the ready frame and verify the protocol id verbatim. If any step fails, the process will be recycled and an error will be reported.
    pub fn spawn(options: &SpawnOptions) -> Result<Self, NodeReplError> {
        let driver_root = options
            .driver_dir
            .clone()
            .unwrap_or_else(std::env::temp_dir);
        let script = driver::materialize_driver(&driver_root).map_err(NodeReplError::from)?;
        let script_dir = script
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(driver_root);

        let mut command = Command::new(&options.node_exe);
        command
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if options.restrict_process {
            command.arg("--restrict-process");
        }
        for dir in &options.module_dirs {
            command.arg("--module-dir").arg(dir);
        }
        if let Some(cwd) = &options.cwd {
            command.current_dir(cwd);
        }
        #[cfg(windows)]
        {
            // Kernel is a background process and the console window must not pop up (same reason as capability.rs detection).
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }

        let mut child = command.spawn().map_err(|error| {
            let _ = std::fs::remove_dir_all(&script_dir);
            NodeReplError::Io(format!("failed to spawn kernel via node: {error}"))
        })?;

        // Any one of stdin/stdout/stderr is missing: recycling process + error after clearing directory (piped guarantees not
        // It will happen, but hanging silently when there is a lack of pipes is worse than reporting an error).
        let (stdin, stdout, stderr) =
            match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
                (Some(stdin), Some(stdout), Some(stderr)) => (stdin, stdout, stderr),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_dir_all(&script_dir);
                    return Err(NodeReplError::Io(
                        "kernel process pipes are incomplete (stdin/stdout/stderr)".to_string(),
                    ));
                }
            };

        let inner = Arc::new(KernelInner {
            stdin: Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            ready: Mutex::new(None),
            death: Mutex::new(None),
            stderr: Mutex::new(Vec::new()),
            alive: AtomicBool::new(true),
        });

        let (ready_tx, ready_rx) = mpsc::channel::<ReadyEvent>();
        spawn_stdio_threads(Arc::clone(&inner), stdout, stderr, ready_tx);

        let kernel = Self {
            inner,
            child: Mutex::new(child),
            driver_dir: script_dir,
            next_id: AtomicU64::new(1),
        };

        // ready handshake: verify protocol id verbatim, does not match fail-closed (frame semantics of wrong generation driver
        // Don't assume, rather kill and report again than guess).
        match ready_rx.recv_timeout(options.ready_timeout) {
            Ok(ready) => {
                if ready.protocol != KERNEL_PROTOCOL {
                    kernel.kill_now();
                    return Err(NodeReplError::Protocol(format!(
                        "kernel protocol mismatch: driver speaks {:?}, Rust side expects {KERNEL_PROTOCOL:?}",
                        ready.protocol,
                    )));
                }
                *kernel.inner.ready.lock().expect("kernel ready mutex") = Some(ready);
            }
            Err(_) => {
                kernel.kill_now();
                return Err(NodeReplError::TimedOut(format!(
                    "kernel did not send ready within {:?}",
                    options.ready_timeout
                )));
            }
        }
        Ok(kernel)
    }

    /// The pid of ready handshake announcement (used for diagnostics/logging).
    pub fn pid(&self) -> u32 {
        self.inner
            .ready
            .lock()
            .expect("kernel ready mutex")
            .as_ref()
            .map(|ready| ready.pid)
            .unwrap_or(0)
    }

    /// Whether the kernel is still alive (false after fatal/EOF/explicit kill).
    pub fn is_alive(&self) -> bool {
        self.inner.alive.load(Ordering::SeqCst)
    }

    /// Execute a piece of code (see module documentation for two-level timeout/cancellation semantics).
    pub fn evaluate(
        &self,
        code: &str,
        sync_timeout_ms: u64,
        overall_timeout: Duration,
        request_meta: Value,
        cancel: Option<&AtomicBool>,
    ) -> Result<KernelOutput, NodeReplError> {
        self.ensure_alive()?;
        let id = self.next_id();
        let (tx, rx) = mpsc::channel();
        self.inner
            .pending
            .lock()
            .expect("kernel pending mutex")
            .insert(id, tx);
        let frame = EvaluateFrame::new(id, code, sync_timeout_ms, request_meta);
        if let Err(error) = self.write_frame(&frame) {
            self.finish_request(id);
            return Err(error);
        }

        let deadline = Instant::now() + overall_timeout;
        let mut canceled = false;
        loop {
            // User cancellation takes precedence over timeout (active cancellation takes effect immediately, without waiting for the window to be exhausted).
            if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
                canceled = true;
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let slice = WAIT_SLICE.min(deadline - now);
            match rx.recv_timeout(slice) {
                Ok(frame) => return self.settle_eval_frame(frame),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(self.death_error("kernel dropped the evaluation channel"));
                }
            }
        }

        //Two-level semantics level 1: send cancel frame, the driver will reset and send back one frame during the grace period
        // (normal result or error with RESET_SUFFIX).
        let reason = if canceled {
            CancelReason::AbortError
        } else {
            CancelReason::TimeoutError
        };
        let message = if canceled {
            "JavaScript execution was canceled"
        } else {
            "JavaScript execution timed out"
        };
        if self
            .write_frame(&CancelFrame::new(id, reason, message))
            .is_err()
        {
            self.finish_request(id);
            return Err(self.death_error("kernel died while cancelling"));
        }
        // Within the grace window, the driver will first return the ack frame of cancel (kernel_driver.mjs:815, which only means// "cancel has been queued"), followed by the aborted settlement frame (error with RESET_SUFFIX).
        // ack is not a result - you must skip and continue waiting. Treating ack as a result will lose the reset semantics.
        let grace_deadline = Instant::now() + Duration::from_millis(KERNEL_CANCEL_GRACE_MS);
        loop {
            let now = Instant::now();
            if now >= grace_deadline {
                break;
            }
            let slice = WAIT_SLICE.min(grace_deadline - now);
            match rx.recv_timeout(slice) {
                Ok(InboundFrame::CancelAck { .. }) => continue,
                Ok(frame) => return self.settle_eval_frame(frame),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(self.death_error("kernel dropped the evaluation channel"));
                }
            }
        }
        //Level 2: After the grace period expires, the kernel still has no settlement frame → it cannot even handle cancel, so upgrade the kill tree.
        self.kill_now();
        let tail = format!(
            "{message}; kernel did not settle within {KERNEL_CANCEL_GRACE_MS} ms and was killed"
        );
        Err(if canceled {
            NodeReplError::Cancelled(tail)
        } else {
            NodeReplError::TimedOut(tail)
        })
    }

    /// The result frame returned by the driver (including the error field) is a completed execution:
    /// Delivered as-is. Only Error kind (protocol-level errors without result wrapping) are kernel faults.
    fn settle_eval_frame(&self, frame: InboundFrame) -> Result<KernelOutput, NodeReplError> {
        match frame {
            InboundFrame::Result { output, .. } => Ok(output),
            InboundFrame::Error { error, .. } => Err(NodeReplError::Protocol(format!(
                "kernel rejected the operation: {}: {}",
                error.name, error.message
            ))),
            other => Err(NodeReplError::Protocol(format!(
                "unexpected kernel frame during evaluation: {other:?}"
            ))),
        }
    }

    /// Query kernel status (generation/busy/moduleDirs, kernel_driver.mjs:823-831).
    pub fn status(&self) -> Result<KernelStatus, NodeReplError> {
        self.ensure_alive()?;
        let (id, rx) = self.begin_request()?;
        let result = self.write_frame(&SimpleFrame::status(id)).and_then(|()| {
            let frame = self.await_frame(id, &rx, CONTROL_TIMEOUT)?;
            match frame {
                InboundFrame::Status { status, .. } => Ok(status),
                other => Err(NodeReplError::Protocol(format!(
                    "unexpected frame for status: {other:?}"
                ))),
            }
        });
        self.finish_request(id);
        let reported = result?;
        let ready = self
            .inner
            .ready
            .lock()
            .expect("kernel ready mutex")
            .clone()
            .ok_or_else(|| NodeReplError::Protocol("kernel ready fact missing".to_string()))?;
        Ok(KernelStatus {
            pid: ready.pid,
            node_version: ready.node,
            generation: reported.generation,
            busy: reported.busy,
            restrict_process: reported.restrict_process,
            module_dirs: reported.module_dirs,
        })
    }

    /// Explicit reset (driver rebuild context + generation+1, kernel_driver.mjs:789-793).
    /// The normal path is not required: the driver has reset itself after cancel/timeout; here is the host
    /// Used for "User actively clears session". Return the new generation.
    pub fn reset(&self) -> Result<u64, NodeReplError> {
        self.ensure_alive()?;
        let (id, rx) = self.begin_request()?;
        let result = self.write_frame(&SimpleFrame::reset(id)).and_then(|()| {
            let frame = self.await_frame(id, &rx, CONTROL_TIMEOUT)?;
            match frame {
                InboundFrame::ResetAck { generation, .. } => Ok(generation),
                other => Err(NodeReplError::Protocol(format!(
                    "unexpected frame for reset: {other:?}"
                ))),
            }
        });
        self.finish_request(id);
        result
    }

    /// Inject module parsing directory (host-side capability; this tool will not be exposed to the model starting from 0.6.0, see the reason for this)
    /// tool-contract.ts:13-18: The model cannot know which node_modules to pass, the host knows).
    pub fn add_module_dir(&self, dir: &Path) -> Result<Vec<String>, NodeReplError> {
        self.ensure_alive()?;
        let (id, rx) = self.begin_request()?;
        let result = self
            .write_frame(&AddModuleDirFrame::new(
                id,
                dir.to_string_lossy().into_owned(),
            ))
            .and_then(|()| {
                let frame = self.await_frame(id, &rx, CONTROL_TIMEOUT)?;
                match frame {
                    InboundFrame::ModuleDir { frame, .. } if frame.ok => Ok(frame.dirs),
                    InboundFrame::ModuleDir { frame, .. } => {
                        let detail = frame
                            .error
                            .map(|error| format!("{}: {}", error.name, error.message))
                            .unwrap_or_else(|| "kernel refused the module dir".to_string());
                        Err(NodeReplError::Protocol(detail))
                    }
                    other => Err(NodeReplError::Protocol(format!(
                        "unexpected frame for addModuleDir: {other:?}"
                    ))),
                }
            });
        self.finish_request(id);
        result
    }

    /// Graceful shutdown: shutdown frame (driver exit(0) by itself, kernel_driver.mjs:859-862)
    /// + Short window, etc. ack + Tell the truth kill_tree + Clean up the materialized directory. Idempotent; Drop also adjusts it.
    pub fn shutdown(&self) {
        if self.inner.alive.load(Ordering::SeqCst) {
            if let Ok((id, rx)) = self.begin_request() {
                let _ = self.write_frame(&SimpleFrame::shutdown(id));
                // The driver receives process.exit(0): only a short window is given, and it does not rely on ack to arrive.
                let _ = rx.recv_timeout(Duration::from_millis(500));
                self.finish_request(id);
            }
        }
        self.kill_now();
    }

    /// Immediate recycling: kill_tree (overwrite the descendant process of user code spawn) + set to death + settlement
    /// pending + harvest exit code (anti-zombie) + clean up the materialized directory. Idempotent.
    pub fn kill_now(&self) {
        if self.inner.alive.swap(false, Ordering::SeqCst) {
            let mut child = self.child.lock().expect("kernel child mutex");
            let pid = child.id();
            // nc-code-proc::kill_tree: leaf priority + process group last hit + survivor force,
            // No-op success for dead roots (README contract).
            let _ = nc_code_proc::kill_tree(pid, KERNEL_CANCEL_GRACE_MS);
            // It also needs to harvest exit codes to prevent zombies.
            let _ = child.kill();
            let _ = child.wait();
        }
        self.settle_all_pending();
        let _ = std::fs::remove_dir_all(&self.driver_dir);
    }

    fn ensure_alive(&self) -> Result<(), NodeReplError> {
        if self.inner.alive.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(self.death_error("kernel is not running"))
        }
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn begin_request(&self) -> Result<(u64, mpsc::Receiver<InboundFrame>), NodeReplError> {
        self.ensure_alive()?;
        let id = self.next_id();
        let (tx, rx) = mpsc::channel();
        self.inner
            .pending
            .lock()
            .expect("kernel pending mutex")
            .insert(id, tx);
        Ok((id, rx))
    }

    fn await_frame(
        &self,
        id: u64,
        rx: &mpsc::Receiver<InboundFrame>,
        timeout: Duration,
    ) -> Result<InboundFrame, NodeReplError> {
        match rx.recv_timeout(timeout) {
            Ok(frame) => Ok(frame),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(NodeReplError::TimedOut(format!(
                "kernel did not answer request {id} within {timeout:?}"
            ))),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(self.death_error("kernel dropped the request channel"))
            }
        }
    }

    fn finish_request(&self, id: u64) {
        self.inner
            .pending
            .lock()
            .expect("kernel pending mutex")
            .remove(&id);
    }

    fn write_frame<T: Serialize>(&self, frame: &T) -> Result<(), NodeReplError> {
        let mut line = serde_json::to_string(frame)
            .map_err(|error| NodeReplError::Protocol(format!("frame serialize: {error}")))?;
        line.push('\n');
        let mut stdin = self.inner.stdin.lock().expect("kernel stdin mutex");
        stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|error| NodeReplError::Io(format!("kernel stdin write failed: {error}")))
    }

    /// Death error with diagnosis: attached with cause of death and stderr tail (stderr only contains kernel itself
    /// Diagnostic output - user console and NDJSON logs, do not enter here, do not leak the evaluation content).
    fn death_error(&self, context: &str) -> NodeReplError {
        let reason = self.inner.death.lock().expect("kernel death mutex").clone();
        let stderr = self.stderr_tail();
        let mut message = match reason {
            Some(reason) => format!("{context}: {reason}"),
            None => context.to_string(),
        };
        if !stderr.is_empty() {
            message.push_str(&format!("; kernel stderr: {stderr}"));
        }
        NodeReplError::KernelDied(message)
    }

    fn stderr_tail(&self) -> String {
        let bytes = self.inner.stderr.lock().expect("kernel stderr mutex");
        String::from_utf8_lossy(&bytes).trim().to_string()
    }

    fn settle_all_pending(&self) {
        // sender is dropped → all recv ends get Disconnected → each receives death_error.
        let drained: Vec<mpsc::Sender<InboundFrame>> = self
            .inner
            .pending
            .lock()
            .expect("kernel pending mutex")
            .drain()
            .map(|(_, tx)| tx)
            .collect();
        drop(drained);
    }
}

impl Drop for NodeKernel {
    fn drop(&mut self) {
        // Backpack recycling: The caller forgets to leave the orphan Node and temporary driver directory when shutting down().
        self.shutdown();
    }
}
/// Start stdout reader / stderr drain two background threads.
fn spawn_stdio_threads(
    inner: Arc<KernelInner>,
    stdout: std::process::ChildStdout,
    stderr: std::process::ChildStderr,
    ready_tx: mpsc::Sender<ReadyEvent>,
) {
    let reader_inner = Arc::clone(&inner);
    let stderr_inner = Arc::clone(&inner);
    let reader = std::thread::Builder::new()
        .name("zcode-node-repl-stdout".into())
        .spawn(move || reader_thread(stdout, reader_inner, ready_tx));
    let drainer = std::thread::Builder::new()
        .name("zcode-node-repl-stderr".into())
        .spawn(move || stderr_thread(stderr, stderr_inner));
    // Thread spawn fails (resources exhausted): Death is resolved immediately, allowing all waiting parties to get KernelDied
    // Instead of waiting indefinitely for a response that never comes.
    if let Err(error) = reader {
        mark_dead(&inner, format!("spawn stdout reader thread: {error}"));
    }
    if let Err(error) = drainer {
        mark_dead(&inner, format!("spawn stderr thread: {error}"));
    }
}
fn mark_dead(inner: &KernelInner, reason: String) {
    *inner.death.lock().expect("kernel death mutex") = Some(reason);
    inner.alive.store(false, Ordering::SeqCst);
    let drained: Vec<mpsc::Sender<InboundFrame>> = inner
        .pending
        .lock()
        .expect("kernel pending mutex")
        .drain()
        .map(|(_, tx)| tx)
        .collect();
    drop(drained);
}

/// stdout reader: Single thread parses frames and routes by id. EOF/fatal marks the kernel as dead
/// And settle all pending (recv end gets Disconnected → death_error with diagnosis).
/// Note: This thread **does not touch the child** - wait/kill is only performed with locks in the NodeKernel method.
/// Avoid equality with kill_now.
fn reader_thread(
    stdout: std::process::ChildStdout,
    inner: Arc<KernelInner>,
    ready_tx: mpsc::Sender<ReadyEvent>,
) {
    let mut reader = BufReader::new(stdout);
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer) {
            Ok(0) => break, // EOF
            Ok(n) if n > MAX_FRAME_LINE_BYTES => {
                mark_dead(
                    &inner,
                    "kernel emitted an oversized protocol frame".to_string(),
                );
                return;
            }
            Ok(_) => {}
            Err(error) => {
                mark_dead(&inner, format!("kernel stdout read failed: {error}"));
                return;
            }
        }
        let line = String::from_utf8_lossy(&buffer);
        let Some(frame) = parse_inbound(line.trim()) else {
            continue; // Bad row/unknown kind: It cannot be any waiting response, discard it without hanging.
        };
        match frame {
            InboundFrame::Ready(ready) => {
                // ready is only sent once during the spawn handshake; repeated frames are ignored (existing facts are not overwritten).
                let _ = ready_tx.send(ready);
            }
            InboundFrame::Fatal(error) => {
                mark_dead(
                    &inner,
                    format!("kernel fatal: {}: {}", error.name, error.message),
                );
                return;
            }
            InboundFrame::Error { id: None, error } => {
                // Error frame without id: kernel response to bad input parameters, no waiting parties; record diagnosis.
                eprintln!(
                    "zcode-node-repl: kernel reported {}: {}",
                    error.name, error.message
                );
            }
            InboundFrame::Result { id, output } => {
                deliver(&inner, id, InboundFrame::Result { id, output });
            }
            InboundFrame::CancelAck { id, acknowledged } => {
                // ack is not the final frame: the aborted evaluate will also return the settlement frame with the same id.
                // Here we only forward, not consume pending - consumption occurs in the final frame (Result/Error).
                peek_deliver(&inner, id, InboundFrame::CancelAck { id, acknowledged });
            }
            InboundFrame::ResetAck { id, generation } => {
                deliver(&inner, id, InboundFrame::ResetAck { id, generation });
            }
            InboundFrame::Status { id, status } => {
                deliver(&inner, id, InboundFrame::Status { id, status });
            }
            InboundFrame::ModuleDir { id, frame } => {
                deliver(&inner, id, InboundFrame::ModuleDir { id, frame });
            }
            InboundFrame::ShutdownAck { id } => {
                deliver(&inner, id, InboundFrame::ShutdownAck { id });
            }
            InboundFrame::Error {
                id: Some(id),
                error,
            } => {
                deliver(
                    &inner,
                    id,
                    InboundFrame::Error {
                        id: Some(id),
                        error,
                    },
                );
            }
        }
    }
    // EOF: stdout is closed but the reason is unknown (the process may have exited, or the pipe may just be closed).
    // Set the common cause of death; stderr is appended by death_error at the end.
    mark_dead(&inner, "kernel stdout closed".to_string());
}

/// Final frame delivery: consumption pending (one request is only settled once).
fn deliver(inner: &KernelInner, id: u64, frame: InboundFrame) {
    let tx = inner
        .pending
        .lock()
        .expect("kernel pending mutex")
        .remove(&id);
    if let Some(tx) = tx {
        let _ = tx.send(frame);
    }
    // Unowned response (request has timed out and been removed): silently discarded - late frames do not constitute an error.
}

/// Non-final frame delivery (CancelAck): forward but keep pending, waiting for the real settlement frame.
fn peek_deliver(inner: &KernelInner, id: u64, frame: InboundFrame) {
    let tx = inner
        .pending
        .lock()
        .expect("kernel pending mutex")
        .get(&id)
        .cloned();
    if let Some(tx) = tx {
        let _ = tx.send(frame);
    }
}

/// stderr drain: Bounded collection of kernel's own diagnostic output (user console goes to NDJSON
/// Logs do not enter here; stderr only has Node's warning/stack/startup failure information).
fn stderr_thread(mut stderr: std::process::ChildStderr, inner: Arc<KernelInner>) {
    let mut chunk = [0_u8; 4096];
    loop {
        match stderr.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut locked = inner.stderr.lock().expect("kernel stderr mutex");
                if locked.len() < STDERR_LIMIT {
                    let room = STDERR_LIMIT - locked.len();
                    locked.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_options_default_mirrors_ts_host() {
        // server.ts:139 restrictProcess is always true; ready 10s is handshake insurance.
        let options = SpawnOptions::new("node");
        assert!(options.restrict_process);
        assert!(options.module_dirs.is_empty());
        assert_eq!(options.ready_timeout, Duration::from_secs(10));
        assert!(options.cwd.is_none());
    }

    #[test]
    fn spawn_options_builder_sets_cwd_and_module_dirs() {
        let options = SpawnOptions::new("node")
            .cwd("C:/work")
            .module_dir("C:/work/node_modules");
        assert_eq!(options.cwd, Some(PathBuf::from("C:/work")));
        assert_eq!(
            options.module_dirs,
            vec![PathBuf::from("C:/work/node_modules")]
        );
    }
}

/// Manage kernel life cycle by session key (mirror TS server.ts:178-192
/// serialized queue + session key routing: calls in the same session are serialized and across sessions
/// Parallel; calls without session_id share [`NODE_REPL_UNSCOPED_KEY`]).
///
/// Differences from TS (intentional, documented): TS's Worker-per-call natural isolation module cache;
/// When in-process kernel reuse, "binding is not persistent across calls" is guaranteed by the host reset after each call.
/// (the `fresh` parameter of `get_or_spawn`), or guaranteed by the per-shutdown semantics of 0.6.0.
#[derive(Default)]
pub struct KernelRegistry {
    kernels: Mutex<HashMap<String, KernelHandle>>,
}

/// The key shared by calls without session context (TS server.ts:40 UNTRUSTED_SESSION_KEY).
pub const NODE_REPL_UNSCOPED_KEY: &str = "__unscoped__";

impl KernelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take out the kernel corresponding to the session; use factory to create it if it does not exist. factory failed
    /// Throw up as is (fail-closed: do not cache the semi-initialized kernel).
    pub fn get_or_spawn(
        &self,
        key: &str,
        factory: impl FnOnce() -> Result<KernelHandle, NodeReplError>,
    ) -> Result<KernelHandle, NodeReplError> {
        // Check quickly first to avoid spawning while holding the lock (spawn is a 100-millisecond operation).
        if let Some(handle) = self.kernels.lock().expect("registry mutex").get(key) {
            if handle.is_alive() {
                return Ok(Arc::clone(handle));
            }
        }
        let handle = factory()?;
        let mut kernels = self.kernels.lock().expect("registry mutex"); //Double check: The second creator with the same key concurrently gives way (turn off the one with multiple spawns).
                                                                        // Arc clones first and then releases the lock: the borrow cannot survive the guard.
        let winner = kernels
            .get(key)
            .filter(|existing| existing.is_alive())
            .map(Arc::clone);
        if let Some(existing) = winner {
            drop(kernels);
            handle.shutdown();
            return Ok(existing);
        }
        kernels.insert(key.to_string(), Arc::clone(&handle));
        Ok(handle)
    }

    /// The currently registered session key (snapshot).
    pub fn keys(&self) -> Vec<String> {
        self.kernels
            .lock()
            .expect("registry mutex")
            .keys()
            .cloned()
            .collect()
    }

    /// Remove and close the kernel of a session (closeSession semantics).
    pub fn remove_and_shutdown(&self, key: &str) -> bool {
        let removed = self.kernels.lock().expect("registry mutex").remove(key);
        match removed {
            Some(handle) => {
                handle.shutdown();
                true
            }
            None => false,
        }
    }

    /// Close all kernels (host disposeAll semantics).
    pub fn shutdown_all(&self) {
        let drained: Vec<KernelHandle> = self
            .kernels
            .lock()
            .expect("registry mutex")
            .drain()
            .map(|(_, handle)| handle)
            .collect();
        for handle in drained {
            handle.shutdown();
        }
    }
}
