//! `terminal` channel: shell / working directory / environment resolution.
//!
//! Mirrors `resolveTerminalShell`, `resolveTerminalCwd`, `isExecutable` and
//! `resolveTerminalEnv` in `packages/services/src/terminal/terminalService.ts`, including
//! the fallback ordering and the exact error texts, because the frontend surfaces those
//! messages when a shell or directory is unusable.

use std::path::{Path, PathBuf};

use nc_code_service_core::AppPaths;

/// Node's `process.platform` names, which the TypeScript service branches on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Windows,
    MacOS,
    Linux,
    Other(&'static str),
}

impl Platform {
    pub fn current() -> Self {
        match std::env::consts::OS {
            "windows" => Self::Windows,
            "macos" => Self::MacOS,
            "linux" => Self::Linux,
            other => Self::Other(other),
        }
    }

    /// Node's spelling, used in `windowsPty` style platform checks and profile gates.
    pub fn as_node_str(self) -> &'static str {
        match self {
            Self::Windows => "win32",
            Self::MacOS => "darwin",
            Self::Linux => "linux",
            Self::Other(name) => name,
        }
    }

    pub fn is_windows(self) -> bool {
        matches!(self, Self::Windows)
    }

    pub fn is_darwin(self) -> bool {
        matches!(self, Self::MacOS)
    }
}

/// The environment handed to the PTY: `(name, value)` pairs in a stable order.
pub type Environment = Vec<(String, String)>;

/// `process.env` as a list. Order is preserved; lookups use the platform's convention.
pub fn inherited_environment() -> Environment {
    std::env::vars().collect()
}

pub fn environment_get<'a>(env: &'a Environment, key: &str, platform: Platform) -> Option<&'a str> {
    env.iter()
        .find(|(name, _)| {
            if platform.is_windows() {
                name.eq_ignore_ascii_case(key)
            } else {
                name == key
            }
        })
        .map(|(_, value)| value.as_str())
}

fn environment_set(env: &mut Environment, key: &str, value: &str, platform: Platform) {
    if let Some(existing) = env.iter_mut().find(|(name, _)| {
        if platform.is_windows() {
            name.eq_ignore_ascii_case(key)
        } else {
            name == key
        }
    }) {
        existing.1 = value.to_string();
        return;
    }
    env.push((key.to_string(), value.to_string()));
}

fn environment_remove(env: &mut Environment, key: &str, platform: Platform) {
    env.retain(|(name, _)| {
        if platform.is_windows() {
            !name.eq_ignore_ascii_case(key)
        } else {
            name != key
        }
    });
}

/// `isExecutable`: absolute/relative paths are checked directly, bare names are looked
/// up on `PATH` — the same rule the TypeScript helper uses.
pub fn is_executable(command: &str, env: &Environment, platform: Platform) -> bool {
    if command.is_empty() {
        return false;
    }
    if command.contains('/') || (platform.is_windows() && command.contains('\\')) {
        return executable_file(Path::new(command), platform);
    }
    let Some(path_value) = environment_get(env, "PATH", platform) else {
        return false;
    };
    let separator = if platform.is_windows() { ';' } else { ':' };
    path_value.split(separator).any(|dir| {
        let trimmed = dir.trim();
        if trimmed.is_empty() {
            return false;
        }
        executable_file(&Path::new(trimmed).join(command), platform)
    })
}

fn executable_file(path: &Path, platform: Platform) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(windows)]
    {
        let _ = platform;
        true
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
}

/// `resolveTerminalShell`: prefer PowerShell 7+, then the system shell, then `cmd.exe`;
/// on POSIX validate `SHELL` before falling back to the usual login shells.
pub fn resolve_terminal_shell(env: &Environment, platform: Platform) -> Result<String, String> {
    let comspec = environment_get(env, "ComSpec", platform)
        .map(str::to_string)
        .or_else(|| environment_get(env, "COMSPEC", platform).map(str::to_string));

    let candidates: Vec<Option<String>> = if platform.is_windows() {
        vec![
            Some("pwsh.exe".to_string()),
            Some("powershell.exe".to_string()),
            comspec,
            Some("cmd.exe".to_string()),
        ]
    } else {
        vec![
            environment_get(env, "SHELL", platform).map(str::to_string),
            Some("/bin/zsh".to_string()),
            Some("/bin/bash".to_string()),
            Some("/bin/sh".to_string()),
        ]
    };

    for candidate in candidates.into_iter().flatten() {
        if !candidate.trim().is_empty() && is_executable(candidate.trim(), env, platform) {
            return Ok(candidate);
        }
    }

    Err(if platform.is_windows() {
        "No usable Windows shell found for terminal startup".to_string()
    } else {
        "No usable shell found for terminal startup".to_string()
    })
}

/// `resolveTerminalCwd`: prefer the requested directory, then HOME / the system home,
/// then the root, so a deleted workspace still opens a terminal.
pub fn resolve_terminal_cwd(
    cwd: Option<&str>,
    env: &Environment,
    platform: Platform,
    paths: &AppPaths,
) -> Result<PathBuf, String> {
    let home = environment_get(env, "HOME", platform)
        .map(PathBuf::from)
        .or_else(|| environment_get(env, "USERPROFILE", platform).map(PathBuf::from));
    let candidates: [Option<PathBuf>; 4] = [
        cwd.map(PathBuf::from),
        home,
        Some(paths.home_dir().to_path_buf()),
        Some(root_directory(platform)),
    ];
    for candidate in candidates.into_iter().flatten() {
        if is_usable_directory(&candidate) {
            return Ok(candidate);
        }
    }
    Err("No usable working directory found for terminal startup".to_string())
}

fn root_directory(platform: Platform) -> PathBuf {
    if platform.is_windows() {
        // Node's `"/"` on Windows resolves to the current drive root.
        PathBuf::from("\\")
    } else {
        PathBuf::from("/")
    }
}

fn is_usable_directory(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
}

/// `resolveTerminalEnv`: interactive-terminal TERM/COLOR settings, a UTF-8 locale when
/// the inherited one is missing or `C`/`POSIX`, and the macOS GUI `PATH` widening.
pub fn resolve_terminal_environment(
    env: &Environment,
    platform: Platform,
    home_dir: &Path,
) -> Environment {
    let mut next = env.clone();
    let fallback_locale = resolve_fallback_utf8_locale(env, platform);

    if platform.is_darwin() {
        let merged = merge_path_entries(
            &[environment_get(&next, "PATH", platform)]
                .into_iter()
                .map(|value| value.map(str::to_string))
                .chain(
                    DARWIN_GUI_FALLBACK_PATHS
                        .iter()
                        .map(|path| Some((*path).to_string())),
                )
                .collect::<Vec<Option<String>>>()
                .iter()
                .map(|value| value.as_deref())
                .collect::<Vec<Option<&str>>>(),
            platform,
        );
        environment_set(&mut next, "PATH", &merged, platform);
    }

    let inherited_term = environment_get(&next, "TERM", platform)
        .unwrap_or("")
        .to_string();
    environment_set(&mut next, "TERM", TERM_PROGRAM_SETTING, platform);
    let color_terminal = environment_get(&next, "COLORTERM", platform)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "truecolor".to_string());
    environment_set(&mut next, "COLORTERM", &color_terminal, platform);
    // The runtime login-shell probe runs with `TERM=dumb CI=1`; a real terminal must not
    // inherit that non-interactive marker (starship/powerlevel10k degrade under it).
    let ci_flag = environment_get(&next, "CI", platform)
        .unwrap_or("")
        .to_string();
    if ci_flag == "1" && inherited_term == "dumb" {
        environment_remove(&mut next, "CI", platform);
    }

    for key in ["LANG", "LC_CTYPE"] {
        let current = environment_get(&next, key, platform)
            .unwrap_or("")
            .to_string();
        if is_missing_or_c_locale(&current) {
            environment_set(&mut next, key, &fallback_locale, platform);
        }
    }
    if let Some(current) = environment_get(&next, "LC_ALL", platform).map(str::to_string) {
        if is_missing_or_c_locale(&current) {
            environment_set(&mut next, "LC_ALL", &fallback_locale, platform);
        }
    }

    let _ = home_dir;
    next
}

const TERM_PROGRAM_SETTING: &str = "xterm-256color";

const DARWIN_GUI_FALLBACK_PATHS: [&str; 8] = [
    "/opt/homebrew/bin",
    "/opt/homebrew/sbin",
    "/usr/local/bin",
    "/usr/local/sbin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
];

/// `mergePathEntries`: dedupe while keeping the caller's order.
pub fn merge_path_entries(entries: &[Option<&str>], platform: Platform) -> String {
    let separator = if platform.is_windows() { ';' } else { ':' };
    let mut seen: Vec<String> = Vec::new();
    for entry in entries.iter().copied().flatten() {
        for item in entry.split(separator) {
            let trimmed = item.trim();
            if trimmed.is_empty() || seen.iter().any(|known| known == trimmed) {
                continue;
            }
            seen.push(trimmed.to_string());
        }
    }
    seen.join(separator.to_string().as_str())
}

fn is_utf8_locale(value: &str) -> bool {
    let lower = value.to_lowercase().replace('-', "");
    lower.contains("utf8")
}

fn is_missing_or_c_locale(value: &str) -> bool {
    let normalized = value.trim().to_uppercase();
    normalized.is_empty() || normalized == "C" || normalized == "POSIX"
}

fn resolve_fallback_utf8_locale(env: &Environment, platform: Platform) -> String {
    for key in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Some(value) = environment_get(env, key, platform) {
            if is_utf8_locale(value) {
                return value.to_string();
            }
        }
    }
    if platform.is_darwin() {
        "en_US.UTF-8".to_string()
    } else {
        "C.UTF-8".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Environment {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn platform_names_match_node() {
        assert_eq!(Platform::Windows.as_node_str(), "win32");
        assert_eq!(Platform::MacOS.as_node_str(), "darwin");
        assert_eq!(Platform::Linux.as_node_str(), "linux");
    }

    #[test]
    fn environment_helpers_are_case_insensitive_only_on_windows() {
        let env = env(&[("Path", "C:\\Windows")]);
        assert_eq!(
            environment_get(&env, "PATH", Platform::Windows),
            Some("C:\\Windows")
        );
        assert_eq!(environment_get(&env, "PATH", Platform::Linux), None);
    }

    #[test]
    fn resolve_terminal_shell_reports_the_typescript_error_when_nothing_is_usable() {
        let error = resolve_terminal_shell(&env(&[]), Platform::Windows).unwrap_err();
        assert_eq!(error, "No usable Windows shell found for terminal startup");
        let error = resolve_terminal_shell(&env(&[]), Platform::Linux).unwrap_err();
        assert_eq!(error, "No usable shell found for terminal startup");
    }

    #[test]
    fn resolve_terminal_shell_prefers_the_configured_posix_shell() {
        let dir = std::env::temp_dir();
        let fake = dir.join("zcode-fake-shell.sh");
        std::fs::write(&fake, "#!/bin/sh\n").expect("write fake shell");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            let env = env(&[("SHELL", fake.to_str().expect("utf-8 path"))]);
            assert_eq!(
                resolve_terminal_shell(&env, Platform::Linux).expect("shell"),
                fake.to_str().expect("utf-8 path")
            );
        }
        std::fs::remove_file(&fake).ok();
    }

    #[test]
    fn resolve_terminal_shell_ignores_a_dangling_shell_entry() {
        // When the external environment leaves a SHELL that does not exist, it must not be handed straight to the pty; the fallback chain must continue.
        let env = env(&[("SHELL", "/definitely/not/a/shell-xyz")]);
        let resolved = resolve_terminal_shell(&env, Platform::Linux);
        if let Ok(shell) = resolved {
            assert_ne!(shell, "/definitely/not/a/shell-xyz");
        }
    }

    #[test]
    fn resolve_terminal_cwd_falls_back_when_the_request_is_stale() {
        let paths = AppPaths::new(std::env::temp_dir(), std::env::temp_dir());
        let env = env(&[("HOME", std::env::temp_dir().to_str().expect("utf-8"))]);
        let missing = Path::new("/definitely/not/a/directory-xyz");
        let resolved =
            resolve_terminal_cwd(missing.to_str(), &env, Platform::Linux, &paths).expect("cwd");
        assert!(resolved.is_dir());
    }

    #[test]
    fn terminal_environment_sets_term_color_and_utf8_locale() {
        let env = env(&[("LANG", "C"), ("LC_CTYPE", "POSIX")]);
        let next = resolve_terminal_environment(&env, Platform::Linux, Path::new("/"));
        assert_eq!(
            environment_get(&next, "TERM", Platform::Linux),
            Some("xterm-256color")
        );
        assert_eq!(
            environment_get(&next, "COLORTERM", Platform::Linux),
            Some("truecolor")
        );
        assert_eq!(
            environment_get(&next, "LANG", Platform::Linux),
            Some("C.UTF-8")
        );
        assert_eq!(
            environment_get(&next, "LC_CTYPE", Platform::Linux),
            Some("C.UTF-8")
        );
    }

    #[test]
    fn terminal_environment_keeps_a_user_locale_and_drops_the_dumb_ci_marker() {
        let env = env(&[
            ("LANG", "zh_CN.UTF-8"),
            ("TERM", "dumb"),
            ("CI", "1"),
            ("COLORTERM", " 24bit "),
        ]);
        let next = resolve_terminal_environment(&env, Platform::Linux, Path::new("/"));
        assert_eq!(
            environment_get(&next, "LANG", Platform::Linux),
            Some("zh_CN.UTF-8")
        );
        assert_eq!(environment_get(&next, "CI", Platform::Linux), None);
        assert_eq!(
            environment_get(&next, "COLORTERM", Platform::Linux),
            Some("24bit")
        );
    }

    #[test]
    fn terminal_environment_widens_the_darwin_gui_path() {
        let env = env(&[("PATH", "/usr/bin:/custom/bin")]);
        let next = resolve_terminal_environment(&env, Platform::MacOS, Path::new("/"));
        let merged = environment_get(&next, "PATH", Platform::MacOS).expect("PATH");
        assert!(merged.starts_with("/custom/bin:") || merged.contains("/custom/bin"));
        assert!(merged.contains("/opt/homebrew/bin"));
        assert_eq!(
            merged
                .split(':')
                .filter(|entry| *entry == "/usr/bin")
                .count(),
            1,
            "PATH entries are deduped while keeping order"
        );
    }

    #[test]
    fn merge_path_entries_dedupes_and_skips_blanks() {
        let merged = merge_path_entries(&[Some("/a::/b"), Some("/b:/c")], Platform::Linux);
        assert_eq!(merged, "/a:/b:/c");
    }

    #[test]
    fn locale_helpers_match_the_typescript_predicates() {
        assert!(is_utf8_locale("en_US.utf8"));
        assert!(is_utf8_locale("C.UTF-8"));
        assert!(!is_utf8_locale("C"));
        assert!(is_missing_or_c_locale(""));
        assert!(is_missing_or_c_locale(" posix "));
        assert!(!is_missing_or_c_locale("de_DE.UTF-8"));
    }

    #[test]
    fn is_executable_rejects_directories_and_missing_files() {
        let env = env(&[]);
        assert!(!is_executable(
            std::env::temp_dir().to_str().expect("utf-8"),
            &env,
            Platform::Linux
        ));
        assert!(!is_executable("/no/such/thing-xyz", &env, Platform::Linux));
        assert!(!is_executable("", &env, Platform::Linux));
    }
}
