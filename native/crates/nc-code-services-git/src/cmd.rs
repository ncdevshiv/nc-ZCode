//! The Git subprocess boundary — port of `providers/gitCommandProvider.ts` plus
//! `providers/gitEnvironmentProvider.ts`.
//!
//! **Injection safety:** `git` is always spawned from an argument array. No shell is
//! involved and no argument is interpolated into a command string, so a path or refname
//! containing `;`, `&&`, backticks or quotes reaches Git verbatim as one argv entry.
//! User-controlled values that could look like options are additionally placed after an
//! explicit `--` by the callers (see `repo.rs`), matching the TS implementation.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nc_code_service_core::ServiceError;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::{Mutex, OnceCell};

pub const DISCOVERY_TIMEOUT: Duration = Duration::from_millis(3_000);
pub const COMMAND_TIMEOUT: Duration = Duration::from_millis(15_000);
/// `git push` is an explicit user action and a repository pre-push hook can run a full
/// test suite; TS relaxes only the push timeout/output budget (`config.ts:5-12`).
pub const PUSH_TIMEOUT: Duration = Duration::from_millis(10 * 60_000);
pub const DIFF_TIMEOUT: Duration = Duration::from_millis(20_000);
pub const OUTPUT_BYTES: usize = 512 * 1024;
pub const PUSH_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
pub const DIFF_BYTES: usize = 1024 * 1024;

/// Env vars a parent Git hook injects into its children (`config.ts:34-51`). Passing them
/// through would make our commands operate on the hook's repository instead of the
/// explicit cwd.
const GIT_LOCAL_ENV_VARS: [&str; 16] = [
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CONFIG",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_DIR",
    "GIT_GRAFT_FILE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_INTERNAL_SUPER_PREFIX",
    "GIT_NAMESPACE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
    "GIT_REPLACE_REF_BASE",
    "GIT_SHALLOW_FILE",
    "GIT_WORK_TREE",
];

/// Result of one `git` invocation. `exit_code` is `None` when Git was terminated by a
/// signal or never reported a code — the same `null` the TS side stores.
#[derive(Debug, Clone)]
pub struct GitCommandResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub output_truncated: bool,
    pub duration_ms: u64,
    pub timeout_ms: u64,
    pub timeout_elapsed_ms: Option<u64>,
}

pub struct GitCommand<'a> {
    pub cwd: &'a Path,
    pub args: &'a [&'a str],
    pub timeout: Duration,
    pub max_output_bytes: usize,
    pub extra_env: &'a [(&'a str, &'a str)],
}

impl<'a> GitCommand<'a> {
    pub fn new(cwd: &'a Path, args: &'a [&'a str]) -> Self {
        Self {
            cwd,
            args,
            timeout: COMMAND_TIMEOUT,
            max_output_bytes: OUTPUT_BYTES,
            extra_env: &[],
        }
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }

    pub fn extra_env(mut self, extra_env: &'a [(&'a str, &'a str)]) -> Self {
        self.extra_env = extra_env;
        self
    }
}

#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    truncated: bool,
}

pub struct GitCommandProvider {
    binary: OnceCell<Option<String>>,
}

impl GitCommandProvider {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            binary: OnceCell::new(),
        })
    }

    /// `resolveGitBinary`: `nc_code_GIT_BINARY`, `git` on PATH, then the Windows Git
    /// install locations; the first candidate that answers `--version` wins and the
    /// answer is cached for the process (`gitEnvironmentProvider.ts:41-59`).
    pub async fn resolve_git_binary(&self) -> Option<String> {
        self.binary
            .get_or_init(|| async {
                for candidate in binary_candidates() {
                    if can_execute_git(&candidate).await {
                        return Some(candidate);
                    }
                }
                None
            })
            .await
            .clone()
    }

    pub async fn run(&self, command: GitCommand<'_>) -> Result<GitCommandResult, ServiceError> {
        let binary = self
            .resolve_git_binary()
            .await
            .ok_or_else(|| ServiceError::message("Git binary is not available"))?;
        let started = Instant::now();
        let mut process = Command::new(&binary);
        process
            .args(command.args)
            .current_dir(command.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            .kill_on_drop(true);
        for (name, value) in command_env() {
            process.env(name, value);
        }
        for (name, value) in command.extra_env {
            process.env(name, value);
        }
        #[cfg(windows)]
        // `windowsHide: true` on the TS side: never flash a console per Git call.
        process.creation_flags(0x0800_0000);

        let mut child = match process.spawn() {
            Ok(child) => child,
            Err(error) => return Ok(spawn_failure_result(&binary, &error, started)),
        };
        let stdout_capture = Arc::new(Mutex::new(Capture::default()));
        let stderr_capture = Arc::new(Mutex::new(Capture::default()));
        let stdout_task = child.stdout.take().map(|reader| {
            tokio::spawn(drain_capped(
                reader,
                command.max_output_bytes,
                stdout_capture.clone(),
            ))
        });
        let stderr_task = child.stderr.take().map(|reader| {
            tokio::spawn(drain_capped(
                reader,
                command.max_output_bytes,
                stderr_capture.clone(),
            ))
        });

        let mut timed_out = false;
        let mut timeout_elapsed_ms = None;
        let status = match tokio::time::timeout(command.timeout, child.wait()).await {
            Ok(status) => status,
            Err(_elapsed) => {
                timed_out = true;
                timeout_elapsed_ms = Some(started.elapsed().as_millis() as u64);
                let _ = child.kill().await;
                child.wait().await
            }
        };
        for mut task in stdout_task.into_iter().chain(stderr_task) {
            if tokio::time::timeout(Duration::from_millis(500), &mut task)
                .await
                .is_err()
            {
                task.abort();
            }
        }

        let stdout = capture_text(&stdout_capture).await;
        let stderr = capture_text(&stderr_capture).await;
        let output_truncated =
            capture_truncated(&stdout_capture).await || capture_truncated(&stderr_capture).await;
        let exit_code = match status {
            Ok(status) => status.code(),
            Err(_) => None,
        };
        Ok(GitCommandResult {
            stdout,
            stderr,
            exit_code,
            timed_out,
            output_truncated,
            duration_ms: started.elapsed().as_millis() as u64,
            timeout_ms: command.timeout.as_millis() as u64,
            timeout_elapsed_ms,
        })
    }
}

async fn capture_text(capture: &Arc<Mutex<Capture>>) -> String {
    let guard = capture.lock().await;
    String::from_utf8_lossy(&guard.bytes).into_owned()
}

async fn capture_truncated(capture: &Arc<Mutex<Capture>>) -> bool {
    capture.lock().await.truncated
}

/// Reads to the byte budget and keeps draining afterwards so Git never blocks on a full
/// pipe. TS kills the child at the budget instead (`gitCommandProvider.ts:196-217`); the
/// only observable difference is that the exit code stays Git's real one, and every caller
/// checks `output_truncated` before the exit code, so the resulting error text matches.
async fn drain_capped<R>(mut reader: R, max_output_bytes: usize, capture: Arc<Mutex<Capture>>)
where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0u8; 64 * 1024];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => {
                let mut guard = capture.lock().await;
                if guard.truncated {
                    continue;
                }
                if guard.bytes.len() + read > max_output_bytes {
                    guard.truncated = true;
                } else {
                    guard.bytes.extend_from_slice(&buffer[..read]);
                }
            }
        }
    }
}

/// `child_process.spawn` failure → a normal command result with `exitCode = -2` and the
/// error message in stderr (`gitCommandProvider.ts:222-229`). Node/libuv reports a missing
/// cwd on Windows as `-ENOENT`, while Rust surfaces `os error 267`; the degraded-repo
/// detection downstream keys on `enoent`, so the message is normalised here.
fn spawn_failure_result(
    binary: &str,
    error: &std::io::Error,
    started: Instant,
) -> GitCommandResult {
    let missing_working_directory = matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    );
    let stderr = if missing_working_directory {
        format!("spawn {binary} ENOENT")
    } else {
        error.to_string()
    };
    GitCommandResult {
        stdout: String::new(),
        stderr,
        exit_code: Some(-2),
        timed_out: false,
        output_truncated: false,
        duration_ms: started.elapsed().as_millis() as u64,
        timeout_ms: COMMAND_TIMEOUT.as_millis() as u64,
        timeout_elapsed_ms: None,
    }
}

fn binary_candidates() -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(configured) = read_env("ZCODE_GIT_BINARY") {
        candidates.push(configured);
    }
    candidates.push("git".to_string());
    if cfg!(windows) {
        let program_root = read_env("ProgramW6432")
            .or_else(|| read_env("ProgramFiles"))
            .unwrap_or_else(|| "C:\\Program Files".to_string());
        let program_x86 =
            read_env("ProgramFiles(x86)").unwrap_or_else(|| "C:\\Program Files (x86)".to_string());
        for root in [program_root, program_x86] {
            for layout in ["cmd", "bin"] {
                let mut path = PathBuf::from(root.as_str());
                path.push("Git");
                path.push(layout);
                path.push("git.exe");
                candidates.push(path.to_string_lossy().into_owned());
            }
        }
    }
    candidates
}

fn read_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// `getGitCommandEnv` (`config.ts:62`): scrub hook-injected Git locals, then pin a
/// non-interactive, locale-stable environment so parsing never depends on the terminal.
fn command_env() -> Vec<(OsString, OsString)> {
    build_command_env(std::env::vars_os())
}

/// `getGitCommandEnv` over an explicit source so the scrub rules are testable without
/// mutating the process environment.
fn build_command_env<I: IntoIterator<Item = (OsString, OsString)>>(
    source: I,
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = source
        .into_iter()
        .filter(|(name, _)| {
            let text = name.to_string_lossy();
            !GIT_LOCAL_ENV_VARS
                .iter()
                .any(|local| text.eq_ignore_ascii_case(local))
        })
        .collect();
    env.push(("GIT_OPTIONAL_LOCKS".into(), os("0")));
    env.push(("GIT_PAGER".into(), os("cat")));
    env.push(("PAGER".into(), os("cat")));
    env.push(("TERM".into(), os("dumb")));
    env.push(("LC_ALL".into(), os("C")));
    env.push(("LANG".into(), os("C")));
    env
}

fn os(value: &str) -> OsString {
    OsString::from(value)
}

async fn can_execute_git(candidate: &str) -> bool {
    let mut process = Command::new(candidate);
    process
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear()
        .kill_on_drop(true);
    for (name, value) in command_env() {
        process.env(name, value);
    }
    #[cfg(windows)]
    process.creation_flags(0x0800_0000);
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(_) => return false,
    };
    let status = tokio::time::timeout(DISCOVERY_TIMEOUT, child.wait()).await;
    if status.is_err() {
        let _ = child.kill().await;
    }
    matches!(status, Ok(Ok(status)) if status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runs_git_and_captures_stdout() {
        let provider = GitCommandProvider::new();
        let binary = provider
            .resolve_git_binary()
            .await
            .expect("git is available on the test host");
        assert!(!binary.is_empty());
        let cwd = std::env::temp_dir();
        let result = provider
            .run(GitCommand::new(&cwd, &["--version"]))
            .await
            .expect("run");
        assert_eq!(result.exit_code, Some(0));
        assert!(result.stdout.starts_with("git version "));
        assert!(!result.timed_out);
        assert!(!result.output_truncated);
    }

    #[tokio::test]
    async fn shell_metacharacters_stay_inert_argv() {
        let provider = GitCommandProvider::new();
        let cwd = std::env::temp_dir();
        // Through a shell this would run `echo pwned`; as argv it is one refname.
        let result = provider
            .run(GitCommand::new(
                &cwd,
                &["rev-parse", "--verify", "refs/heads/nope; echo pwned"],
            ))
            .await
            .expect("run");
        assert_ne!(result.exit_code, Some(0));
        assert!(
            !result.stdout.contains("pwned"),
            "the shell must never interpret the argument: {:?}",
            result.stdout
        );
    }

    #[tokio::test]
    async fn truncates_output_beyond_the_budget() {
        let provider = GitCommandProvider::new();
        let cwd = std::env::temp_dir();
        let result = provider
            .run(GitCommand::new(&cwd, &["--help"]).max_output_bytes(1024))
            .await
            .expect("run");
        assert!(result.output_truncated, "git --help is larger than 1 KiB");
    }

    #[tokio::test]
    async fn missing_working_directory_degrades_like_node() {
        let provider = GitCommandProvider::new();
        let missing = std::env::temp_dir().join("zcode-git-missing-cwd-42");
        let result = provider
            .run(GitCommand::new(&missing, &["--version"]))
            .await
            .expect("spawn failure is a result, not an error");
        assert_eq!(result.exit_code, Some(-2));
        assert!(result.stderr.to_lowercase().contains("enoent"));
    }

    #[test]
    fn command_env_scrubs_git_locals_and_pins_locale() {
        // A leaked hook GIT_DIR must not make our commands operate on another repository.
        let env = build_command_env(vec![
            (
                OsString::from("GIT_DIR"),
                OsString::from("/tmp/leaked-hook-repo"),
            ),
            (OsString::from("git_config"), OsString::from("leaked")),
            (OsString::from("PATH"), OsString::from("/bin")),
        ]);
        let names: Vec<String> = env
            .iter()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        assert!(!names.contains(&"GIT_DIR".to_string()));
        assert!(!names.contains(&"git_config".to_string()));
        assert!(names.contains(&"PATH".to_string()));
        for local in GIT_LOCAL_ENV_VARS {
            assert!(
                !names.iter().any(|name| name.eq_ignore_ascii_case(local)),
                "{local} must be scrubbed from the inherited environment"
            );
        }
        assert!(env
            .iter()
            .any(|(name, value)| name == "LC_ALL" && value == &OsString::from("C")));
        assert!(env
            .iter()
            .any(|(name, value)| name == "GIT_OPTIONAL_LOCKS" && value == &OsString::from("0")));
    }

    #[test]
    fn binary_candidates_start_with_path_git() {
        let candidates = binary_candidates();
        let position = candidates
            .iter()
            .position(|candidate| candidate == "git")
            .expect("PATH git is always a candidate");
        assert!(position <= 1, "the nc_code_GIT_BINARY override comes first");
        if cfg!(windows) {
            assert!(candidates
                .iter()
                .any(|candidate| candidate.ends_with("Git\\cmd\\git.exe")));
        }
    }
}
