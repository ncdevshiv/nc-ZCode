//! Which interpreter Bash runs under is a capability decision, not an implementation detail.
//!
//! This ports the auto-detect branches of TS `packages/adapters/src/exec/bash-shell-provider.ts`:
//! the order, the fixed paths, deriving `bash.exe` from `git`, PATH/PATHEXT expansion and the env overlay all match it.
//! Previously this host hardcoded `cmd` on Windows, so `grep`/`$(…)`/heredoc written by the model got entirely different
//! semantics, while the read-only argv classification judged by POSIX commands — the two sides disagreeing is a defect.
//!
//! TS's `user-config` override and shell snapshot depend on the desktop preference read/write chain, which this host has no
//! configuration source for, so only auto-detection is ported (differences recorded in `specs/disabled-features.md`).

use std::collections::HashMap;
use std::sync::OnceLock;

/// The resolution result decides both the argv shape and the env overlay; the two must not be written in separate places.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellDialect {
    /// The bash shipped with Git on Windows.
    GitBash,
    /// The bash/zsh resolved on Unix.
    Posix,
    /// The last fallback: cmd on Windows, equal to TS's legacy-shell.
    Cmd,
}

#[derive(Clone, Debug)]
pub struct Shell {
    dialect: ShellDialect,
    program: String,
    envs: Vec<(String, String)>,
}

impl Shell {
    pub fn dialect(&self) -> ShellDialect {
        self.dialect
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    /// A proper interpreter takes only `-c`; cmd keeps `/D /C` (`/D` skips AutoRun so user registry hooks cannot rewrite the output).
    pub fn argv(&self, command: &str) -> Vec<String> {
        match self.dialect {
            ShellDialect::Cmd => vec!["/D".to_string(), "/C".to_string(), command.to_string()],
            ShellDialect::GitBash | ShellDialect::Posix => {
                vec!["-c".to_string(), command.to_string()]
            }
        }
    }

    pub fn apply(&self, cmd: &mut std::process::Command) {
        for (key, value) in &self.envs {
            cmd.env(key, value);
        }
    }
}

/// Byte-for-byte identical to TS `WINDOWS_GIT_BASH_PATHS`.
const WINDOWS_GIT_BASH_PATHS: [&str; 2] = [
    "C:\\Program Files\\Git\\bin\\bash.exe",
    "C:\\Program Files (x86)\\Git\\bin\\bash.exe",
];
/// Byte-for-byte identical to TS `FIXED_POSIX_SHELL_DIRS`.
const FIXED_POSIX_SHELL_DIRS: [&str; 4] =
    ["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"];
const DEFAULT_WINDOWS_PATHEXT: [&str; 4] = [".com", ".exe", ".bat", ".cmd"];

fn split_path(value: &str, windows: bool) -> Vec<String> {
    let separator = if windows { ';' } else { ':' };
    value
        .split(separator)
        .filter(|entry| !entry.is_empty())
        .map(|entry| entry.trim_matches('"').to_string())
        .collect()
}

fn join_path(dir: &str, name: &str, windows: bool) -> String {
    let trimmed = dir.trim_end_matches(['\\', '/']);
    if windows {
        format!("{trimmed}\\{name}")
    } else {
        format!("{trimmed}/{name}")
    }
}

fn basename(path: &str) -> String {
    path.rsplit(['\\', '/']).next().unwrap_or(path).to_string()
}

fn dirname(path: &str) -> String {
    let bytes: Vec<char> = path.chars().collect();
    for index in (0..bytes.len()).rev() {
        if bytes[index] == '\\' || bytes[index] == '/' {
            return bytes[..index].iter().collect();
        }
    }
    String::new()
}

fn has_extension(path: &str) -> bool {
    basename(path).contains('.')
}

/// TS's `accessSync(path, X_OK)`: on Windows existing is enough to count as executable; on Unix the execute bit is also required.
fn default_exists(path: &str) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return metadata.permissions().mode() & 0o111 != 0;
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Same shape as TS `windowsExtensionCandidates`: if an extension is already present use only it, otherwise bare name + lowercased PATHEXT.
fn extension_candidates(file: &str, path_exts: &[String]) -> Vec<String> {
    if has_extension(file) {
        return vec![file.to_string()];
    }
    let mut candidates = vec![file.to_string()];
    for ext in path_exts {
        candidates.push(format!("{file}{ext}"));
    }
    candidates
}

fn windows_path_exts(env: &HashMap<String, String>) -> Vec<String> {
    let raw = env
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATHEXT"))
        .map(|(_, value)| value.clone())
        .unwrap_or_else(|| DEFAULT_WINDOWS_PATHEXT.join(";"));
    let list = split_path(&raw, true);
    if list.is_empty() {
        return DEFAULT_WINDOWS_PATHEXT
            .iter()
            .map(|ext| ext.to_string())
            .collect();
    }
    list.into_iter().map(|ext| ext.to_lowercase()).collect()
}

/// Consistent with TS `windowsExecutableCandidates`: an absolute path or one containing a separator gets its extensions expanded directly,
/// otherwise PATH is traversed.
pub fn executable_candidates(
    file: &str,
    env: &HashMap<String, String>,
    windows: bool,
    exists: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    if !windows {
        return vec![file.to_string()];
    }
    let exts = windows_path_exts(env);
    let looks_like_path = file.contains('\\') || file.contains('/') || has_extension(file);
    if looks_like_path {
        return extension_candidates(file, &exts)
            .into_iter()
            .filter(|p| exists(p))
            .collect();
    }
    let dirs = env
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
        .map(|(_, value)| split_path(value, true))
        .unwrap_or_default();
    dirs.iter()
        .flat_map(|dir| {
            extension_candidates(file, &exts)
                .into_iter()
                .map(move |name| join_path(dir, &name, true))
        })
        .filter(|candidate| exists(candidate))
        .collect()
}

fn posix_shell_kind(path: &str) -> Option<&'static str> {
    let name = basename(path).to_lowercase();
    if name.contains("bash") {
        Some("bash")
    } else if name.contains("zsh") {
        Some("zsh")
    } else {
        None
    }
}

/// Consistent with TS `preferredPosixShellKinds`: when `$SHELL` is bash, bash is preferred, otherwise zsh comes first.
fn preferred_posix_shell_kinds(env: &HashMap<String, String>) -> [&'static str; 2] {
    let shell = env.get("SHELL").map(String::as_str).unwrap_or("");
    if posix_shell_kind(shell) == Some("bash") {
        ["bash", "zsh"]
    } else {
        ["zsh", "bash"]
    }
}

fn resolve_posix_bash(
    env: &HashMap<String, String>,
    exists: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(shell) = env.get("SHELL") {
        if posix_shell_kind(shell).is_some() {
            candidates.push(shell.clone());
        }
    }
    let dirs = env
        .get("PATH")
        .map(|value| split_path(value, false))
        .unwrap_or_default();
    for kind in preferred_posix_shell_kinds(env) {
        for dir in &dirs {
            candidates.push(join_path(dir, kind, false));
        }
        for dir in FIXED_POSIX_SHELL_DIRS {
            candidates.push(join_path(dir, kind, false));
        }
    }
    let mut seen = std::collections::HashSet::new();
    candidates
        .iter()
        .find(|candidate| seen.insert((*candidate).clone()) && exists(candidate))
        .cloned()
}

/// Same order as TS `resolveWindowsGitBashShell`: fixed paths → derived from `git`.
fn resolve_windows_git_bash(
    env: &HashMap<String, String>,
    exists: &dyn Fn(&str) -> bool,
) -> Option<String> {
    for candidate in WINDOWS_GIT_BASH_PATHS {
        if exists(candidate) {
            return Some(candidate.to_string());
        }
    }
    let git = executable_candidates("git", env, true, exists)
        .into_iter()
        .next()?;
    let git_dir = dirname(&git);
    for level in ["..\\bin\\bash.exe", "..\\..\\bin\\bash.exe"] {
        let candidate = format!("{git_dir}\\{level}");
        if exists(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn bash_shell(dialect: ShellDialect, program: String) -> Shell {
    // Both TS provider factories inject the same overlay pair; dropping GIT_EDITOR would make git commit
    // hang on an interactive editor that does not exist.
    Shell {
        dialect,
        envs: vec![
            ("GIT_EDITOR".to_string(), "true".to_string()),
            ("SHELL".to_string(), program.clone()),
        ],
        program,
    }
}

pub fn resolve(
    env: &HashMap<String, String>,
    platform: &str,
    exists: &dyn Fn(&str) -> bool,
) -> Shell {
    if platform == "windows" || platform == "win32" {
        if let Some(bash) = resolve_windows_git_bash(env, exists) {
            return bash_shell(ShellDialect::GitBash, bash);
        }
        let fallback = env
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("COMSPEC"))
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| "cmd".to_string());
        return Shell {
            dialect: ShellDialect::Cmd,
            program: fallback,
            envs: Vec::new(),
        };
    }
    if let Some(shell) = resolve_posix_bash(env, exists) {
        return bash_shell(ShellDialect::Posix, shell);
    }
    Shell {
        dialect: ShellDialect::Posix,
        program: "sh".to_string(),
        envs: Vec::new(),
    }
}

fn current_env() -> HashMap<String, String> {
    std::env::vars().collect()
}

static SHELL: OnceLock<Shell> = OnceLock::new();

/// Resolved once in-process and cached: equivalent to TS's session-level shell snapshot, so PATH is not rescanned on every call,
/// and so two paths in the same turn cannot resolve to different interpreters.
pub fn shell() -> &'static Shell {
    SHELL.get_or_init(|| resolve(&current_env(), std::env::consts::OS, &default_exists))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn windows_prefers_the_fixed_git_bash_paths() {
        let shell = resolve(
            &env(&[("PATH", "C:\\Windows\\system32")]),
            "win32",
            &|path| path == WINDOWS_GIT_BASH_PATHS[1],
        );
        assert_eq!(shell.dialect(), ShellDialect::GitBash);
        assert_eq!(shell.program(), WINDOWS_GIT_BASH_PATHS[1]);
        assert_eq!(shell.argv("echo hi"), vec!["-c", "echo hi"]);
        assert!(shell
            .envs
            .contains(&("GIT_EDITOR".to_string(), "true".to_string())));
    }

    #[test]
    fn windows_infers_bash_from_the_git_location() {
        // git lives at …\Git\cmd\git.exe → derive …\Git\bin\bash.exe (TS's two-level fallback).
        let exists = |path: &str| {
            path == "C:\\Program Files\\Git\\cmd\\git.exe"
                || path == "C:\\Program Files\\Git\\cmd\\..\\bin\\bash.exe"
        };
        let shell = resolve(
            &env(&[("PATH", "C:\\Program Files\\Git\\cmd")]),
            "win32",
            &exists,
        );
        assert_eq!(shell.dialect(), ShellDialect::GitBash);
        assert!(shell.program().ends_with("bin\\bash.exe"));
    }

    #[test]
    fn windows_without_git_falls_back_to_cmd_with_d_and_c() {
        let shell = resolve(&env(&[("PATH", "C:\\Windows\\system32")]), "win32", &|_| {
            false
        });
        assert_eq!(shell.dialect(), ShellDialect::Cmd);
        assert_eq!(shell.argv("dir"), vec!["/D", "/C", "dir"]);
        assert!(shell.envs.is_empty(), "cmd does not inject bash's overlay");
    }

    #[test]
    fn posix_honours_a_bash_shell_variable_first() {
        let shell = resolve(
            &env(&[("SHELL", "/bin/bash"), ("PATH", "/usr/bin")]),
            "linux",
            &|path| path == "/bin/bash" || path == "/usr/bin/zsh",
        );
        assert_eq!(shell.dialect(), ShellDialect::Posix);
        assert_eq!(shell.program(), "/bin/bash");
        assert_eq!(shell.argv("ls"), vec!["-c", "ls"]);
    }

    #[test]
    fn posix_prefers_zsh_when_shell_is_not_bash() {
        // TS: when $SHELL is not bash, zsh comes first, so with both on PATH, zsh is chosen.
        let shell = resolve(
            &env(&[("SHELL", "/bin/fish"), ("PATH", "/usr/bin")]),
            "linux",
            &|path| path == "/usr/bin/zsh" || path == "/usr/bin/bash",
        );
        assert_eq!(shell.program(), "/usr/bin/zsh");
    }

    #[test]
    fn posix_without_any_shell_uses_sh() {
        let shell = resolve(&env(&[("PATH", "/nonexistent")]), "linux", &|_| false);
        assert_eq!(shell.program(), "sh");
        assert!(shell.envs.is_empty());
    }

    #[test]
    fn non_bash_shell_variable_is_not_a_candidate() {
        // When $SHELL is fish, fish must not be used as bash (TS's posixShellKind only accepts bash/zsh).
        let shell = resolve(
            &env(&[("SHELL", "/usr/bin/fish"), ("PATH", "")]),
            "linux",
            &|path| path == "/usr/bin/fish",
        );
        assert_ne!(shell.program(), "/usr/bin/fish");
    }

    #[test]
    fn windows_path_ext_expansion_only_keeps_existing_files() {
        let candidates = executable_candidates(
            "git",
            &env(&[("PATH", "C:\\a;C:\\b"), ("PATHEXT", ".COM;.EXE")]),
            true,
            &|path| path == "C:\\b\\git.exe",
        );
        assert_eq!(candidates, vec!["C:\\b\\git.exe".to_string()]);
    }

    #[test]
    fn cached_shell_is_stable_and_executable() {
        let first = shell();
        assert!(
            std::ptr::eq(first, shell()),
            "The parsing result must be counted only once"
        );
        // Do not guess the concrete interpreter; pin only the invariant: the cached shell really can run one command.
        let output = {
            let mut cmd = std::process::Command::new(first.program());
            first.apply(&mut cmd);
            cmd.args(first.argv("echo shell-probe"))
                .stdout(std::process::Stdio::piped())
                .output()
        };
        let output = output.expect("shell must be spawnable");
        assert!(String::from_utf8_lossy(&output.stdout).contains("shell-probe"));
    }
}
