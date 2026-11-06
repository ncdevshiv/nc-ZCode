//! Real pseudoterminal sessions on top of `portable-pty`.
//!
//! Platform backends are the native ones, selected by the crate at compile time:
//! - Windows: ConPTY (`CreatePseudoConsole`), matching the TypeScript service's
//!   `useConpty` path and its system-ConPTY fallback;
//! - macOS/Linux: a real pty (`forkpty`), matching `node-pty`'s unix terminal.
//!
//! Two OS threads share one terminal:
//! - the **reader** drains the PTY and streams decoded output (so no trailing bytes are
//!   lost when the shell exits);
//! - the **waiter** reaps the child as soon as it dies — dispose must never leave a
//!   zombie — and then joins the reader before reporting `onDynamicExit`, keeping the
//!   TypeScript order "data … then exit".

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use nc_code_rpc::WireValue;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tokio::sync::broadcast;

use crate::utf8_stream::Utf8StreamDecoder;

const READ_CHUNK_BYTES: usize = 8 * 1024;
const DATA_BUS_CAPACITY: usize = 2048;
const EXIT_BUS_CAPACITY: usize = 8;

/// Everything resolved before a PTY can start, mirroring the options handed to
/// `node-pty.spawn` (`{ name, cols, rows, cwd, env }`).
pub struct SpawnRequest<'a> {
    pub id: &'a str,
    pub cols: u16,
    pub rows: u16,
    pub shell: &'a str,
    pub cwd: &'a Path,
    pub env: &'a [(String, String)],
    pub term_name: &'a str,
}

pub struct PtyTerminal {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    process_id: Option<u32>,
    disposed: Arc<AtomicBool>,
    data_tx: broadcast::Sender<WireValue>,
    exit_tx: broadcast::Sender<WireValue>,
}

impl PtyTerminal {
    /// Opens a PTY, spawns `shell` inside it and starts the reader/waiter threads.
    ///
    /// `on_reap` runs on the waiter thread once the child has been waited for; the
    /// service uses it to drop the terminal, mirroring `terminals.delete(id)` inside
    /// the TypeScript `onExit` handler.
    pub fn spawn(
        request: SpawnRequest<'_>,
        on_reap: impl FnOnce(&str) + Send + 'static,
    ) -> Result<Self, String> {
        let size = PtySize {
            rows: request.rows,
            cols: request.cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(size)
            .map_err(|error| error.to_string())?;

        let mut command = CommandBuilder::new(request.shell);
        // node-pty replaces the child environment with the object it is given, so the
        // inherited environment is cleared before the resolved one is applied.
        command.env_clear();
        for (key, value) in request.env {
            command.env(key, value);
        }
        command.env("TERM", request.term_name);
        command.cwd(request.cwd);

        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| error.to_string())?;
        let process_id = child.process_id();
        let killer = child.clone_killer();
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| error.to_string())?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| error.to_string())?;

        let (data_tx, _) = broadcast::channel(DATA_BUS_CAPACITY);
        let (exit_tx, _) = broadcast::channel(EXIT_BUS_CAPACITY);
        let disposed = Arc::new(AtomicBool::new(false));
        // On Windows a ConPTY read returns 0 bytes instead of blocking when there is no data, so the pump thread must be able to keep waiting;
        // once the child exits the waiter sets stop, letting the pump thread exit so the join does not hang.
        let stop = Arc::new(AtomicBool::new(false));
        let id = request.id.to_string();

        // The pump reports when it has stopped reading, but the waiter never blocks on it:
        // on Windows a ConPTY read can stay blocked after the child exits, and joining there
        // would withhold `onDynamicExit` forever.
        let (pump_done_tx, pump_done_rx) = std::sync::mpsc::channel::<()>();
        let reader_data = data_tx.clone();
        let reader_disposed = Arc::clone(&disposed);
        let reader_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name(format!("zcode-pty-read-{id}"))
            .spawn(move || {
                pump_output(reader, reader_data, reader_disposed, reader_stop);
                let _ = pump_done_tx.send(());
            })
            .map_err(|error| format!("failed to start the terminal reader: {error}"))?;

        let waiter_exit = exit_tx.clone();
        let waiter_disposed = Arc::clone(&disposed);
        let waiter_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name(format!("zcode-pty-wait-{id}"))
            .spawn(move || {
                let status = child.wait();
                // The child has exited: tell the pump thread it may stop (on Windows it polls 0-byte reads).
                waiter_stop.store(true, Ordering::Release);
                let killed = waiter_disposed.load(Ordering::Acquire);
                if !killed {
                    // Give the pump a short window to drain the last bytes the shell printed,
                    // then report the real exit status exactly once. A blocked ConPTY read must
                    // not delay this event: `on_reap` closes the console, which unblocks it.
                    let _ = pump_done_rx.recv_timeout(std::time::Duration::from_millis(500));
                    if let Ok(status) = status {
                        let _ = waiter_exit.send(WireValue::Int(status.exit_code()));
                    }
                }
                on_reap(&id);
                // Dropping the waiter's senders closes the exit subscription; the
                // reader's copy goes with it, so no subscription outlives the terminal.
            })
            .map_err(|error| format!("failed to start the terminal waiter: {error}"))?;

        Ok(Self {
            master: Arc::new(Mutex::new(pair.master)),
            writer: Arc::new(Mutex::new(writer)),
            killer: Arc::new(Mutex::new(killer)),
            process_id,
            disposed,
            data_tx,
            exit_tx,
        })
    }

    pub fn process_id(&self) -> Option<u32> {
        self.process_id
    }

    pub fn subscribe_data(&self) -> broadcast::Receiver<WireValue> {
        self.data_tx.subscribe()
    }

    pub fn subscribe_exit(&self) -> broadcast::Receiver<WireValue> {
        self.exit_tx.subscribe()
    }

    /// `pty.write(data)` — node-pty encodes the string as UTF-8 bytes.
    pub fn write(&self, data: &str) -> Result<(), String> {
        let mut writer = lock(&self.writer, "terminal input");
        writer
            .write_all(data.as_bytes())
            .map_err(|error| format!("failed to write to the terminal: {error}"))?;
        writer
            .flush()
            .map_err(|error| format!("failed to write to the terminal: {error}"))
    }

    /// `pty.resize(cols, rows)`.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
        let master = lock(&self.master, "terminal master");
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| format!("failed to resize the terminal: {error}"))
    }

    /// Window size as the kernel/ConPTY sees it — used to prove a resize reached the PTY.
    pub fn size(&self) -> Option<(u32, u32)> {
        let master = lock(&self.master, "terminal master");
        master
            .get_size()
            .ok()
            .map(|size| (u32::from(size.cols), u32::from(size.rows)))
    }

    /// `pty.kill()` plus emitter disposal: the child is terminated and no further
    /// events are delivered for this terminal.
    pub fn kill(&self) {
        self.disposed.store(true, Ordering::Release);
        let mut killer = lock(&self.killer, "terminal child killer");
        // A process that already exited reports an error here; the TypeScript
        // `pty.kill()` only needs the process gone.
        let _ = killer.kill();
    }
}

/// Drains the PTY and streams decoded output until the slave end closes.
fn pump_output(
    mut reader: Box<dyn Read + Send>,
    data_tx: broadcast::Sender<WireValue>,
    disposed: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    let mut decoder = Utf8StreamDecoder::default();
    let mut buffer = [0u8; READ_CHUNK_BYTES];
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        match reader.read(&mut buffer) {
            Ok(0) => {
                if cfg!(windows) {
                    // ConPTY: 0 bytes only means "no data right now" (NO_DATA), not EOF. Treating it as EOF
                    // would kill the pump thread after the first idle period, losing all echo from later writes.
                    // Poll, waiting for the child to exit (the waiter sets stop) or for the next data chunk.
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                } else {
                    break;
                }
            }
            Ok(count) => {
                if disposed.load(Ordering::Acquire) {
                    // The TypeScript emitters are disposed together with the terminal,
                    // so output produced after `dispose` is dropped.
                    continue;
                }
                let text = decoder.push(&buffer[..count]);
                if !text.is_empty() {
                    let _ = data_tx.send(WireValue::String(text));
                }
            }
            // A closed/errored master means the terminal is gone; the waiter thread
            // still reaps the child, so nothing leaks.
            Err(_) => break,
        }
    }
}

fn lock<'a, T>(value: &'a Mutex<T>, what: &str) -> std::sync::MutexGuard<'a, T> {
    value.lock().unwrap_or_else(|poisoned| {
        eprintln!("zcode-services-terminal: {what} lock was poisoned");
        poisoned.into_inner()
    })
}
