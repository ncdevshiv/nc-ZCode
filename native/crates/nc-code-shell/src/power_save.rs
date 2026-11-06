//! Keep-awake assertion for `AppSettings.keepAwakeWhileRunning`.
//!
//! The Electron main process used `powerSaveBlocker` to prevent idle system
//! sleep while background work runs. This module implements the same runtime
//! effect:
//!
//! - **Windows**: a dedicated thread holds `SetThreadExecutionState(ES_CONTINUOUS
//!   | ES_SYSTEM_REQUIRED)` for as long as the assertion is enabled.
//! - **macOS**: a `caffeinate -i` child process (prevent idle sleep).
//! - **Linux**: a `systemd-inhibit --what=idle ...` child process.
//!
//! Disabling the setting releases the assertion (thread state reset / child
//! killed). Enabling twice is a no-op; disabling twice is a no-op.

use std::sync::Mutex;

/// Owns the platform assertion; dropping it releases the effect.
pub struct PowerSaveManager {
    assertion: Mutex<Option<Assertion>>,
}

impl Default for PowerSaveManager {
    fn default() -> Self {
        Self {
            assertion: Mutex::new(None),
        }
    }
}

impl PowerSaveManager {
    /// Apply the desired state. Idempotent; returns an explicit error when the
    /// platform assertion cannot be acquired.
    pub fn set_enabled(&self, enabled: bool) -> Result<(), String> {
        let mut guard = match self.assertion.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match (enabled, guard.is_some()) {
            (true, false) => {
                *guard = Some(Assertion::acquire()?);
                Ok(())
            }
            (false, true) => {
                guard.take();
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub fn is_enabled(&self) -> bool {
        match self.assertion.lock() {
            Ok(guard) => guard.is_some(),
            Err(poisoned) => poisoned.into_inner().is_some(),
        }
    }
}

struct Assertion {
    #[cfg(windows)]
    thread: ThreadAssertion,
    #[cfg(unix)]
    child: std::process::Child,
}

impl Assertion {
    fn acquire() -> Result<Self, String> {
        #[cfg(windows)]
        {
            ThreadAssertion::acquire().map(|thread| Self { thread })
        }
        #[cfg(unix)]
        {
            return spawn_unix_assertion().map(|child| Self { child });
        }
        #[cfg(not(any(windows, unix)))]
        {
            Err("keep_awake_unsupported".to_string())
        }
    }
}

impl Drop for Assertion {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        #[cfg(windows)]
        {
            // `ThreadAssertion::drop` sends the stop signal and joins.
            let _ = &self.thread;
        }
    }
}

#[cfg(windows)]
struct ThreadAssertion {
    stop: Option<std::sync::mpsc::Sender<()>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl ThreadAssertion {
    fn acquire() -> Result<Self, String> {
        use std::sync::mpsc;
        use std::time::Duration;
        use windows::Win32::System::Power::{
            SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED, EXECUTION_STATE,
        };

        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let handle = std::thread::Builder::new()
            .name("zcode-keep-awake".to_string())
            .spawn(move || {
                let previous =
                    unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
                if previous == EXECUTION_STATE(0) {
                    let _ = ready_tx.send(Err("keep_awake_assertion_failed".to_string()));
                    return;
                }
                let _ = ready_tx.send(Ok(()));
                // The execution state is thread-scoped: keep this thread alive
                // until the assertion is released.
                let _ = stop_rx.recv();
                unsafe {
                    SetThreadExecutionState(ES_CONTINUOUS);
                }
            })
            .map_err(|error| format!("keep_awake_thread_failed: {error}"))?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                stop: Some(stop_tx),
                handle: Some(handle),
            }),
            Ok(Err(error)) => {
                let _ = handle.join();
                Err(error)
            }
            Err(_) => {
                let _ = stop_tx.send(());
                let _ = handle.join();
                Err("keep_awake_assertion_timeout".to_string())
            }
        }
    }
}

#[cfg(windows)]
impl Drop for ThreadAssertion {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(target_os = "macos")]
fn spawn_unix_assertion() -> Result<std::process::Child, String> {
    std::process::Command::new("caffeinate")
        .arg("-i")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| format!("keep_awake_spawn_failed: {error}"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn spawn_unix_assertion() -> Result<std::process::Child, String> {
    std::process::Command::new("systemd-inhibit")
        .args([
            "--what=idle",
            "--who=ZCode",
            "--why=Keep awake while background tasks run",
            "--mode=block",
            "sleep",
            "infinity",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| format!("keep_awake_spawn_failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_starts_disabled() {
        let manager = PowerSaveManager::default();
        assert!(!manager.is_enabled());
        // Disabling an already-disabled manager is a no-op.
        manager.set_enabled(false).unwrap();
        assert!(!manager.is_enabled());
    }

    #[cfg(windows)]
    #[test]
    fn toggles_the_windows_assertion() {
        let manager = PowerSaveManager::default();
        manager.set_enabled(true).unwrap();
        assert!(manager.is_enabled());
        // Enabling twice must not create a second assertion thread.
        manager.set_enabled(true).unwrap();
        assert!(manager.is_enabled());
        manager.set_enabled(false).unwrap();
        assert!(!manager.is_enabled());
        manager.set_enabled(false).unwrap();
        assert!(!manager.is_enabled());
    }
}
