//! Git subprocess runner for the `git-checkpoint` channel.
//!
//! This is a local copy of the TS plumbing in
//! `packages/services/src/git/providers/gitCommandProvider.ts`,
//! `providers/gitEnvironmentProvider.ts` and the binary/env rules of
//! `packages/services/src/git/config.ts`. It is deliberately **not** shared with
//! `nc-code-services-git`: that crate is a separate port increment and this one must not
//! depend on it (consolidating the two runners is a follow-up for the port owner).
//!
//! Safety: every command is spawned from an argument array with `env_clear()` plus an
//! explicit environment — never through a shell — so no workspace path, ref name or file
//! name can inject extra commands (`config.ts:62-83` strips inherited `GIT_*` variables
//! for exactly that reason).

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use nc_code_service_core::ServiceError;
use serde_json::json;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;

/// `DEFAULT_GIT_DISCOVERY_TIMEOUT_MS` — `config.ts:3`.
pub const DEFAULT_GIT_DISCOVERY_TIMEOUT_MS: u64 = 3_000;
/// `DEFAULT_GIT_COMMAND_TIMEOUT_MS` — `config.ts:4`.
pub const DEFAULT_GIT_COMMAND_TIMEOUT_MS: u64 = 15_000;
/// `DEFAULT_GIT_OUTPUT_BYTES` — `config.ts:9`.
pub const DEFAULT_GIT_OUTPUT_BYTES: usize = 512 * 1024;

/// `GIT_LOCAL_ENV_VARS` — `config.ts:34-51`; inherited values would let git bleed across repositories.
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

/// Node's `windowsHide: true` (`gitCommandProvider.ts:117`).
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// One finished git invocation, in the shape `ensureGitCommandSucceeded` consumes
/// (`packages/services/src/git/repo/gitCliHelpers.ts:186-212`).
#[derive(Debug, Clone, PartialEq)]
pub struct GitOutput {
    pub stdout: String,
    pub stderr: String,
    /// `None` when the child was killed before reporting an exit code, mirroring the
    /// TS `exitCode: null` case.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub truncated: bool,
    pub elapsed_ms: u128,
    pub timeout_ms: u64,
}

impl GitOutput {
    pub fn trimmed_stdout(&self) -> &str {
        self.stdout.trim()
    }

    /// `gitCommandProvider.ts:222-229`: a failed spawn is a *result*, not a throw,
    /// with the TS synthetic exit code `-2`.
    fn spawn_failed(message: String, elapsed_ms: u128) -> Self {
        Self {
            stdout: String::new(),
            stderr: message,
            exit_code: Some(-2),
            timed_out: false,
            truncated: false,
            elapsed_ms,
            timeout_ms: DEFAULT_GIT_COMMAND_TIMEOUT_MS,
        }
    }
}

/// Raw-bytes variant used for `git cat-file blob`, where lossy UTF-8 conversion would
/// hide binary content (the fallback line-stat path must be able to detect it).
#[derive(Debug, Clone, PartialEq)]
pub struct RawGitOutput {
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub truncated: bool,
    /// The real elapsed time: both construction sites (normal / spawn failure) fill it in, and it is carried over on conversion.
    pub raw_elapsed_ms: u128,
}

impl From<RawGitOutput> for GitOutput {
    fn from(value: RawGitOutput) -> Self {
        Self {
            stdout: String::from_utf8_lossy(&value.stdout).into_owned(),
            stderr: value.stderr,
            exit_code: value.exit_code,
            timed_out: value.timed_out,
            truncated: value.truncated,
            // Carry the real elapsed time (it used to be a constant 0, which distorted the timeout message in ensure_succeeded).
            elapsed_ms: value.raw_elapsed_ms,
            timeout_ms: DEFAULT_GIT_COMMAND_TIMEOUT_MS,
        }
    }
}

/// Per-command environment additions. `git add/write-tree/commit-tree/read-tree` run
/// against a temporary `GIT_INDEX_FILE`; everything else runs with the cleaned base env
/// only (`gitCheckpointRepo.ts:262,295,307,315,327`).
#[derive(Debug, Clone, Default)]
pub struct GitCommandEnv {
    pub extra: Vec<(String, String)>,
}

impl GitCommandEnv {
    /// `buildCheckpointEnv` — `gitCheckpointRepo` helper at `gitCheckpointHelpers.ts:184-192`.
    pub fn checkpoint_index(temp_index_path: &Path) -> Self {
        let lossy = temp_index_path.to_string_lossy().into_owned();
        Self {
            extra: vec![
                ("GIT_INDEX_FILE".to_string(), lossy),
                (
                    "GIT_AUTHOR_NAME".to_string(),
                    "ZCode Checkpoint".to_string(),
                ),
                (
                    "GIT_AUTHOR_EMAIL".to_string(),
                    "checkpoint@zcode.local".to_string(),
                ),
                (
                    "GIT_COMMITTER_NAME".to_string(),
                    "ZCode Checkpoint".to_string(),
                ),
                (
                    "GIT_COMMITTER_EMAIL".to_string(),
                    "checkpoint@zcode.local".to_string(),
                ),
            ],
        }
    }
}

/// Resolves the git binary once (TS caches it per provider instance —
/// `gitEnvironmentProvider.ts:42-58`) and runs argument-array commands.
pub struct GitRunner {
    pinned: Option<Option<OsString>>,
    resolved: tokio::sync::OnceCell<Option<OsString>>,
}

impl Default for GitRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl GitRunner {
    pub fn new() -> Self {
        Self {
            pinned: None,
            resolved: tokio::sync::OnceCell::new(),
        }
    }

    /// Test seam: `Some(Some(path))` forces a binary, `Some(None)` simulates
    /// "git is not installed" without touching the real environment.
    pub fn with_pinned(binary: Option<OsString>) -> Self {
        Self {
            // pinned is Option<Option<OsString>>: Some(Some(path)) forces the binary;
            // Some(None) = explicitly "not installed" (written by with_availability), None = run detection.
            // The Option<OsString> argument here expresses only the two states "path given / not given" → map(Some).
            pinned: binary.clone().map(Some),
            resolved: tokio::sync::OnceCell::new(),
        }
        .with_availability(binary.is_some(), binary)
    }

    fn with_availability(mut self, _available: bool, binary: Option<OsString>) -> Self {
        self.pinned = Some(binary.clone());
        let _ = self.resolved.set(binary);
        self
    }

    /// `getGitBinaryCandidates` — `config.ts:53-60`.
    fn candidates() -> Vec<OsString> {
        let mut candidates: Vec<OsString> = Vec::new();
        if let Some(explicit) = std::env::var("ZCODE_GIT_BINARY")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            candidates.push(OsString::from(explicit));
        }
        candidates.push(OsString::from("git"));
        #[cfg(windows)]
        {
            let program_files = std::env::var("ProgramW6432")
                .or_else(|_| std::env::var("ProgramFiles"))
                .unwrap_or_else(|_| "C:\\Program Files".to_string());
            let program_files_x86 = std::env::var("ProgramFiles(x86)")
                .unwrap_or_else(|_| "C:\\Program Files (x86)".to_string());
            for root in [program_files, program_files_x86] {
                for layout in ["cmd", "bin"] {
                    candidates.push(
                        std::path::PathBuf::from(root.clone())
                            .join(layout)
                            .join("git.exe")
                            .into_os_string(),
                    );
                }
            }
        }
        candidates
    }

    pub async fn binary(&self) -> Option<&OsString> {
        if let Some(pinned) = &self.pinned {
            return pinned.as_ref();
        }
        self.resolved.get_or_init(Self::probe).await.as_ref()
    }

    /// `canExecuteGitCandidate` — `gitEnvironmentProvider.ts:13-35`.
    async fn probe() -> Option<OsString> {
        for candidate in Self::candidates() {
            if Self::can_execute(&candidate).await {
                return Some(candidate);
            }
        }
        None
    }

    async fn can_execute(candidate: &OsString) -> bool {
        let mut command = Command::new(candidate);
        command
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .envs(base_git_env())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        let Ok(mut child) = command.spawn() else {
            return false;
        };
        match tokio::time::timeout(
            Duration::from_millis(DEFAULT_GIT_DISCOVERY_TIMEOUT_MS),
            child.wait(),
        )
        .await
        {
            Ok(Ok(status)) => status.code() == Some(0),
            _ => {
                let _ = child.kill().await;
                false
            }
        }
    }

    pub async fn run(
        &self,
        cwd: &Path,
        args: &[&str],
        env: &GitCommandEnv,
    ) -> Result<GitOutput, ServiceError> {
        let raw = self.run_raw(cwd, args, env).await?;
        Ok(GitOutput::from(raw))
    }

    pub async fn run_raw(
        &self,
        cwd: &Path,
        args: &[&str],
        env: &GitCommandEnv,
    ) -> Result<RawGitOutput, ServiceError> {
        let started = Instant::now();
        let Some(binary) = self.binary().await.cloned() else {
            // `gitCommandProvider.ts:88-90`.
            return Err(ServiceError::new("Git binary is not available"));
        };

        let mut command = Command::new(&binary);
        command
            .current_dir(cwd)
            .args(args)
            .env_clear()
            .envs(base_git_env())
            .envs(env.extra.clone())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return Ok(RawGitOutput::from_spawn_failed(
                    error.to_string(),
                    started.elapsed().as_millis(),
                ));
            }
        };

        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let stdout_task = tokio::spawn(drain(stdout_pipe, DEFAULT_GIT_OUTPUT_BYTES));
        let stderr_task = tokio::spawn(drain(stderr_pipe, DEFAULT_GIT_OUTPUT_BYTES));

        let mut timed_out = false;
        let status = match tokio::time::timeout(
            Duration::from_millis(DEFAULT_GIT_COMMAND_TIMEOUT_MS),
            child.wait(),
        )
        .await
        {
            Ok(result) => result.ok(),
            Err(_) => {
                timed_out = true;
                let _ = child.kill().await;
                child.wait().await.ok()
            }
        };

        let (stdout, stdout_truncated) = join_reader(stdout_task).await;
        let (stderr, stderr_truncated) = join_reader(stderr_task).await;
        let elapsed_ms = started.elapsed().as_millis();

        Ok(RawGitOutput {
            stdout,
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            exit_code: if timed_out {
                None
            } else {
                Some(status.map_or(-1, |code| code.code().unwrap_or(-1)))
            },
            timed_out,
            truncated: stdout_truncated || stderr_truncated,
            raw_elapsed_ms: elapsed_ms,
        })
    }
}

async fn join_reader(task: tokio::task::JoinHandle<(Vec<u8>, bool)>) -> (Vec<u8>, bool) {
    task.await.unwrap_or_default()
}

/// Read a pipe fully while keeping at most `limit` bytes; after the cap the pipe is still
/// drained so git can never block on a full pipe (`gitCommandProvider.ts:196-217` kills
/// instead, which only saves the remaining read).
async fn drain<R: AsyncReadExt + Unpin>(pipe: Option<R>, limit: usize) -> (Vec<u8>, bool) {
    let Some(pipe) = pipe else {
        return (Vec::new(), false);
    };
    let mut reader = BufReader::new(pipe);
    let mut collected: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                if truncated {
                    continue;
                }
                let remaining = limit.saturating_sub(collected.len());
                if read > remaining {
                    truncated = true;
                    collected.extend_from_slice(&buffer[..remaining]);
                } else {
                    collected.extend_from_slice(&buffer[..read]);
                }
            }
            Err(_) => break,
        }
    }
    (collected, truncated)
}

impl RawGitOutput {
    fn from_spawn_failed(message: String, elapsed_ms: u128) -> Self {
        let synthetic = GitOutput::spawn_failed(message, elapsed_ms);
        Self {
            stdout: synthetic.stdout.into_bytes(),
            stderr: synthetic.stderr,
            exit_code: synthetic.exit_code,
            timed_out: false,
            truncated: false,
            raw_elapsed_ms: elapsed_ms,
        }
    }
}

/// `getGitCommandEnv` — `config.ts:62-83`. Windows environment keys are case-insensitive,
/// so the strip matches on the lower-cased name (`readEnvValue` in `paths.ts` does the same).
pub fn base_git_env() -> HashMap<String, String> {
    let mut env: HashMap<String, String> = std::env::vars().collect();
    let local: HashMap<String, ()> = GIT_LOCAL_ENV_VARS
        .iter()
        .map(|name| (name.to_lowercase(), ()))
        .collect();
    let stale: Vec<String> = env
        .keys()
        .filter(|key| local.contains_key(&key.to_lowercase()))
        .cloned()
        .collect();
    for key in stale {
        env.remove(&key);
    }
    for (key, value) in [
        ("GIT_OPTIONAL_LOCKS", "0"),
        ("GIT_PAGER", "cat"),
        ("PAGER", "cat"),
        ("TERM", "dumb"),
        ("LC_ALL", "C"),
        ("LANG", "C"),
    ] {
        env.insert(key.to_string(), value.to_string());
    }
    env
}

/// `ensureGitCommandSucceeded` — `gitCliHelpers.ts:186-212`; the message shapes are what
/// the TS layer surfaces to callers, so they are reproduced verbatim.
pub fn ensure_succeeded(
    label: &str,
    output: &GitOutput,
    allowed_exit_codes: &[i32],
) -> Result<(), ServiceError> {
    if output.timed_out {
        return Err(ServiceError::new(format!(
            "{label} timed out after {}ms (elapsed={}ms)",
            output.timeout_ms, output.elapsed_ms
        )));
    }
    if output.truncated {
        return Err(ServiceError::new(format!("{label} output exceeded limit")));
    }
    if allowed_exit_codes.contains(&output.exit_code.unwrap_or(i32::MIN)) {
        return Ok(());
    }
    Err(ServiceError::new(format!(
        "{label} failed: {}",
        result_message(output)
    )))
}

/// `toResultMessage` — `gitCliHelpers.ts:20-22`.
fn result_message(output: &GitOutput) -> String {
    let stderr = output.stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_string();
    }
    let stdout = output.stdout.trim();
    if !stdout.is_empty() {
        return stdout.to_string();
    }
    match output.exit_code {
        Some(code) => format!("exitCode={code}"),
        None => "exitCode=null".to_string(),
    }
}

/// Real line stats through the shared diff engine (`native/crates/nc-code-diff`,
/// jsdiff@9 parity). `None` when either side is binary or the engine declines, in which
/// case the caller falls back to the TS default of `0/0`.
pub fn line_stats_from_contents(old: &[u8], new: &[u8]) -> Option<(u64, u64)> {
    let old_text = std::str::from_utf8(old).ok()?;
    let new_text = std::str::from_utf8(new).ok()?;
    let value =
        nc_code_diff::handle("diff.lines", &json!({ "old": old_text, "new": new_text })).ok()?;
    let mut added = 0_u64;
    let mut removed = 0_u64;
    for chunk in value.get("chunks")?.as_array()? {
        let count = chunk.get("count")?.as_u64()?;
        if chunk.get("added")?.as_bool()? {
            added += count;
        } else if chunk.get("removed")?.as_bool()? {
            removed += count;
        }
    }
    Some((added, removed))
}
