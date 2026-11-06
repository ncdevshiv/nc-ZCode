//! Node capability detection (R2's first sentence: "Tool detects user-provided Node at startup;
//! absent → reports unavailable").
//!
//! The search order mirrors TS's actual discovery mechanism:
//! 1. An executable explicitly injected by the host (TS starts the MCP host with `process.execPath`,
//!    `bootstrap/src/app/official-plugin-runtime.ts:33`; a pure Rust host has no
//!    "running Node" to reuse, so this became explicit injection and is skipped by default).
//! 2. `PATH` entries — TS's PATH is topped up by the login-shell snapshot + the Windows standard locations
//!    (`packages/services/src/runtime-tools/runtimeCommandEnv.ts:157-208`).
//! 3. Standard install locations — Windows uses TS's very same list
//!    (`runtimeCommandEnv.ts:15-20`: `%APPDATA%\npm`, `%ProgramFiles%\nodejs`,
//!    `%ProgramFiles(x86)%\nodejs`, `%LOCALAPPDATA%\Programs\nodejs`),
//!    POSIX uses the common prefixes that TS relies on the login-shell snapshot to cover (differences documented in the README).
//! 4. Version-manager installs directories (the environment-variable roots of NVM / Volta / mise / fnm / nodenv).
//!
//! **`mise.toml` is never parsed**: TS does not parse it either (`mise.toml:2` only constrains the repository development environment,
//! the runtime has no code that reads it), and a mise-installed Node is discovered through the installs directories
//! under `MISE_DATA_DIR`. This boundary is copied verbatim; no capability undefined by the spec is added.
//!
//! Every step of the probe runs `node --version` with an **argument array** and never goes through a shell
//! (the cross-platform clauses of the root AGENTS.md / apps/nc-code-cli/agents.md).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::error::{NodeReplUnavailable, UnavailableCode};
use crate::version::NodeVersion;

/// The kernel driver's minimum capability line: `node:vm` + ESM `.mjs` driver + global `fetch` (Node 18 onwards).
/// Anything below this line fails closed reporting `unsupported_version`; there is no partial compatibility.
pub const MIN_SUPPORTED_NODE: NodeVersion = NodeVersion {
    major: 18,
    minor: 0,
    patch: 0,
};

/// The default deadline of the `node --version` probe.
pub const DEFAULT_PROBE_TIMEOUT_MS: u64 = 5_000;

/// The category of the source where Node was probed (goes into `details`, never into message, to avoid leaking personal directories).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoverySource {
    /// Explicitly injected by the host.
    Explicit,
    /// A `PATH` entry.
    Path,
    /// The platform's standard install location.
    StandardLocation,
    /// A version-manager installs directory.
    VersionManager,
}

impl DiscoverySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Path => "path",
            Self::StandardLocation => "standardLocation",
            Self::VersionManager => "versionManager",
        }
    }
}

/// All input for one probe. Constructed from the process environment by default; use [`DiscoveryConfig::empty`] for testing
/// Or specify the directory structure so that Node does not need to be actually uninstalled.
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    pub explicit: Option<PathBuf>,
    pub path_dirs: Vec<PathBuf>,
    pub standard_dirs: Vec<PathBuf>,
    pub version_manager_dirs: Vec<PathBuf>,
    pub probe_timeout: Duration,
    /// Child process executable file name (Windows `node.exe`, others `node`).
    pub exe_name: &'static str,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self::from_environment()
    }
}

impl DiscoveryConfig {
    /// Constructed from the current process environment (production path).
    pub fn from_environment() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Constructed from any "environment variable reading function" - making the Windows/POSIX branches testable individually.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let windows = cfg!(windows);
        //PATH case: Windows registry environment is not case-sensitive, but the injected lookup function may
        // Distinguish - first check "PATH" and then check "Path" (same order as TS runtimeCommandEnv).
        let path_value = lookup("PATH")
            .or_else(|| lookup("Path"))
            .unwrap_or_default();
        let delimiter = if windows { ';' } else { ':' };
        let path_dirs = split_paths(&path_value, delimiter);

        let home = lookup(if windows { "USERPROFILE" } else { "HOME" })
            .map(PathBuf::from)
            .unwrap_or_default();

        let standard_dirs = if windows {
            windows_standard_dirs(&lookup)
        } else {
            posix_standard_dirs()
        };

        let version_manager_dirs = version_manager_dirs(&lookup, &home, windows);

        Self {
            explicit: None,
            path_dirs,
            standard_dirs,
            version_manager_dirs,
            probe_timeout: Duration::from_millis(DEFAULT_PROBE_TIMEOUT_MS),
            exe_name: if windows { "node.exe" } else { "node" },
        }
    }

    /// Configuration without any search surface: used to simulate a "node" path on a machine with Node installed.
    pub fn empty() -> Self {
        Self {
            explicit: None,
            path_dirs: Vec::new(),
            standard_dirs: Vec::new(),
            version_manager_dirs: Vec::new(),
            probe_timeout: Duration::from_millis(DEFAULT_PROBE_TIMEOUT_MS),
            exe_name: if cfg!(windows) { "node.exe" } else { "node" },
        }
    }

    /// Only search in the given directory (for testing: empty directory → it will definitely not be found).
    pub fn only_in(dirs: Vec<PathBuf>) -> Self {
        Self {
            path_dirs: dirs,
            ..Self::empty()
        }
    }

    /// A summary of the search surface that can be safely exposed in `details`: **does not contain any absolute paths**,
    /// Therefore the user's home directory/working directory will not be written to the log or tool results.
    pub fn searched_summary(&self) -> Value {
        json!({
            "explicit": usize::from(self.explicit.is_some()),
            "path": self.path_dirs.len(),
            "standardLocation": self.standard_dirs.len(),
            "versionManager": self.version_manager_dirs.len(),
            "exeName": self.exe_name,
        })
    }

    /// Candidate executable files (deduplication by priority).
    pub fn candidates(&self) -> Vec<(PathBuf, DiscoverySource)> {
        let mut out: Vec<(PathBuf, DiscoverySource)> = Vec::new();
        let mut seen: Vec<PathBuf> = Vec::new();
        let push = |path: PathBuf,
                    source: DiscoverySource,
                    out: &mut Vec<(PathBuf, DiscoverySource)>,
                    seen: &mut Vec<PathBuf>| {
            if path.as_os_str().is_empty() || seen.iter().any(|known| same_path(known, &path)) {
                return;
            }
            seen.push(path.clone());
            out.push((path, source));
        };

        if let Some(explicit) = &self.explicit {
            push(
                explicit.clone(),
                DiscoverySource::Explicit,
                &mut out,
                &mut seen,
            );
        }
        for source in [
            (&self.path_dirs, DiscoverySource::Path),
            (&self.standard_dirs, DiscoverySource::StandardLocation),
            (&self.version_manager_dirs, DiscoverySource::VersionManager),
        ] {
            let (dirs, source) = source;
            for dir in dirs {
                let candidate = if dir.is_file() {
                    dir.clone()
                } else {
                    dir.join(self.exe_name)
                };
                push(candidate, source, &mut out, &mut seen);
            }
        }
        out
    }
}

/// Windows standard location: mirror the template of `runtimeCommandEnv.ts:15-20` item by item + expand
/// (`%VAR%` case-insensitive search, consistent with TS's `readWindowsEnvValue`, :28-35).
fn windows_standard_dirs(lookup: &impl Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    const TEMPLATES: [&str; 4] = [
        "%APPDATA%\\npm",
        "%ProgramFiles%\\nodejs",
        "%ProgramFiles(x86)%\\nodejs",
        "%LOCALAPPDATA%\\Programs\\nodejs",
    ];
    TEMPLATES
        .iter()
        .filter_map(|template| {
            let expanded = expand_windows_vars(template, lookup);
            if expanded.is_empty() {
                return None;
            }
            let path = PathBuf::from(&expanded);
            path.exists().then_some(path)
        })
        .collect()
}

fn expand_windows_vars(template: &str, lookup: &impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = &after[..end];
        out.push_str(&lookup(name).unwrap_or_default());
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn posix_standard_dirs() -> Vec<PathBuf> {
    [
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/opt/homebrew/bin",
        "/snap/bin",
        "/usr/local/opt/node/bin",
    ]
    .iter()
    .map(PathBuf::from)
    .filter(|path| path.is_dir())
    .collect()
}
/// The version manager's installs root. TS brings these directories into PATH via login-shell snapshot
/// (`NVM_DIR`/`FNM_*`/`VOLTA_HOME`/`MISE_*`/ of `runtimeCommandEnv.ts:68-96`
/// `NODENV_ROOT` whitelist is reserved for this), pure Rust hosts do not run login shell,
/// Therefore, the installs layout is expanded directly according to the same batch of environment variables.
fn version_manager_dirs(
    lookup: &impl Fn(&str) -> Option<String>,
    home: &Path,
    windows: bool,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let var = |name: &str| lookup(name).map(PathBuf::from);

    // nvm: $NVM_DIR/versions/node/<vMAJOR.MINOR.PATCH>/bin
    let nvm = var("NVM_DIR").unwrap_or_else(|| home.join(".nvm"));
    out.extend(versioned_children(
        &nvm.join("versions").join("node"),
        &["bin"],
    ));

    //volta:$VOLTA_HOME/bin
    let volta = var("VOLTA_HOME").unwrap_or_else(|| home.join(".volta"));
    push_dir(&mut out, volta.join("bin"));

    // mise: $MISE_DATA_DIR (default XDG_DATA_HOME or ~/.local/share)/installs/node/<ver>/bin
    let xdg_data = var("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local").join("share"));
    let mise_data = var("MISE_DATA_DIR").unwrap_or_else(|| xdg_data.clone());
    out.extend(versioned_children(
        &mise_data.join("installs").join("node"),
        &["bin"],
    ));

    // fnm: $FNM_DIR or <xdg>/fnm/node-versions/<ver>/installation/bin
    let fnm_root = var("FNM_DIR").unwrap_or_else(|| xdg_data.join("fnm"));
    out.extend(versioned_children(
        &fnm_root.join("node-versions"),
        &["installation", "bin"],
    ));

    // nodenv: $NODENV_ROOT/versions/<ver>/bin
    let nodenv = var("NODENV_ROOT").unwrap_or_else(|| home.join(".nodenv"));
    out.extend(versioned_children(&nodenv.join("versions"), &["bin"]));

    if windows {
        // nvm-windows on Windows puts the version in $NVM_ROOT\<ver>\
        if let Some(root) = var("NVM_ROOT") {
            out.extend(versioned_children(&root, &[]));
        }
    }

    out.retain(|path| path.is_dir());
    out
}

/// List the subdirectories under `root` in descending order of version number, and append a fixed suffix.
/// The descending order ensures that "the latest version installed by the user takes precedence", which is consistent with the parsing order of mise/nvm itself.
fn versioned_children(root: &Path, suffix: &[&str]) -> Vec<PathBuf> {
    let mut children: Vec<(Option<NodeVersion>, PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_string();
            children.push((NodeVersion::parse(&name), path));
        }
    }
    // Directories whose version names cannot be parsed are ranked last, but still participate in detection.
    children.sort_by(|left, right| match (left.0, right.0) {
        (Some(a), Some(b)) => b.cmp(&a),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => left.1.cmp(&right.1),
    });
    children
        .into_iter()
        .map(|(_, path)| {
            let mut full = path;
            for part in suffix {
                full = full.join(part);
            }
            full
        })
        .collect()
}

fn push_dir(out: &mut Vec<PathBuf>, path: PathBuf) {
    if !path.as_os_str().is_empty() {
        out.push(path);
    }
}

fn split_paths(value: &str, delimiter: char) -> Vec<PathBuf> {
    value
        .split(delimiter)
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Path deduplication: Windows case-insensitive and normalized delimiters, POSIX by bytes.
fn same_path(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .to_ascii_lowercase()
            .replace('\\', "/")
            == right
                .to_string_lossy()
                .to_ascii_lowercase()
                .replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

/// Detection conclusion: available (including executable file and version) or unavailable (including CUA-identical errors).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeCapability {
    Available {
        exe: PathBuf,
        version: NodeVersion,
        source: DiscoverySource,
    },
    Unavailable(NodeReplUnavailable),
}

impl NodeCapability {
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }

    pub fn exe(&self) -> Option<&Path> {
        match self {
            Self::Available { exe, .. } => Some(exe),
            Self::Unavailable(_) => None,
        }
    }

    pub fn version(&self) -> Option<NodeVersion> {
        match self {
            Self::Available { version, .. } => Some(*version),
            Self::Unavailable(_) => None,
        }
    }

    /// **The only connection point in the host tool registry**: `false` Do not register this tool
    /// (R2 acceptance: "the tool is omitted/reported unavailable").
    pub fn should_register_tool(&self) -> bool {
        self.is_available()
    }

    /// Structured conclusions for host/diagnostics. `details` only contains source categories and counts.
    pub fn to_report(&self, searched: Value) -> Value {
        match self {
            Self::Available {
                version, source, ..
            } => json!({
                "available": true,
                "toolName": crate::tool::JS_TOOL_NAME,
                "nodeVersion": version.to_string(),
                "source": source.as_str(),
            }),
            Self::Unavailable(unavailable) => json!({
                "available": false,
                "toolName": crate::tool::JS_TOOL_NAME,"code": unavailable.code_str(),
                "message": unavailable.message,
                "details": unavailable
                    .details
                    .clone()
                    .unwrap_or_else(|| json!({ "searched": searched })),
            }),
        }
    }
}

/// Detect Node: Return [`NodeCapability`]. For any uncertainty go Unavailable and don't guess.
pub fn probe(config: &DiscoveryConfig) -> NodeCapability {
    let searched = config.searched_summary();
    let mut stale: Option<(PathBuf, NodeVersion)> = None;
    let mut unparseable = 0_usize;

    for (candidate, source) in config.candidates() {
        if !candidate.is_file() {
            continue;
        }
        match probe_version(&candidate, config.probe_timeout) {
            Ok(version) => {
                if version.supports_kernel(MIN_SUPPORTED_NODE) {
                    return NodeCapability::Available {
                        exe: candidate,
                        version,
                        source,
                    };
                }
                // Record the **highest** too low version as the basis for diagnosis of UnsupportedVersion.
                match &stale {
                    Some((_, best)) if *best >= version => {}
                    _ => stale = Some((candidate.clone(), version)),
                }
            }
            Err(_) => unparseable += 1,
        }
    }

    if let Some((_, version)) = stale {
        return NodeCapability::Unavailable(NodeReplUnavailable::new(
            UnavailableCode::UnsupportedVersion,
            format!(
                "Node REPL runtime is unavailable: found {version} but the kernel needs >= {MIN_SUPPORTED_NODE}."
            ),
            Some(json!({
                "searched": searched,
                "observedVersion": version.to_string(),"minSupported": MIN_SUPPORTED_NODE.to_string(),
            })),
        ));
    }

    if unparseable > 0 {
        return NodeCapability::Unavailable(NodeReplUnavailable::new(
            UnavailableCode::ProbeFailed,
            "Node REPL runtime is unavailable: no candidate answered `node --version`.",
            Some(json!({ "searched": searched, "probeFailures": unparseable })),
        ));
    }

    NodeCapability::Unavailable(NodeReplUnavailable::new(
        UnavailableCode::RuntimeUnavailable,
        "Node REPL runtime is unavailable: no user-provided Node was found.",
        Some(json!({ "searched": searched })),
    ))
}

/// Convenient entrance: detect by process environment.
pub fn detect(config: Option<&DiscoveryConfig>) -> NodeCapability {
    probe(&config.cloned().unwrap_or_default())
}

/// Execute `<exe> --version` (parameter array, no shell) and parse the version.
///
/// **Must have time limit**: The candidate in PATH may be a bad shim (infinitely suspended batch/agent),
/// Unbounded `output()` will cause one detection to hang up the entire startup process - if it times out, the candidate will be judged to have failed.
/// Continue to try the next one (probe's loop semantics). After timeout, kill + harvest, leaving no zombies.
fn probe_version(exe: &Path, timeout: Duration) -> Result<NodeVersion, String> {
    let started = Instant::now();
    let mut command = Command::new(exe);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        // 0x08000000 = CREATE_NO_WINDOW: The detection must not pop up the console window.
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn failed: {error}"))?;
    loop {
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("probe timed out after {timeout:?}"));
        }
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(error) => return Err(format!("wait failed: {error}")),
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait failed: {error}"))?;
    if !output.status.success() {
        return Err(format!("exit status {}", output.status));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    NodeVersion::parse(&stdout).ok_or_else(|| format!("unparseable version output: {stdout:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        move |key: &str| {
            owned
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn empty_config_finds_nothing_and_is_fail_closed() {
        // This is the simulation of a "node machine": nothing is uninstalled, just the search surface is cleared.
        let capability = probe(&DiscoveryConfig::empty());
        let NodeCapability::Unavailable(unavailable) = &capability else {
            panic!("empty search space must be unavailable: {capability:?}");
        };
        assert_eq!(unavailable.code, UnavailableCode::RuntimeUnavailable);
        assert_eq!(unavailable.name, NodeReplUnavailable::TYPE_NAME);
        assert!(!capability.should_register_tool());
    }

    #[test]
    fn only_in_a_temp_dir_without_node_is_unavailable() {
        let dir = std::env::temp_dir().join(format!(
            "zcode-node-repl-empty-probe-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create probe dir");
        let capability = probe(&DiscoveryConfig::only_in(vec![dir.clone()]));
        assert!(
            !capability.is_available(),
            "an empty dir must not be mistaken for Node: {capability:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn windows_standard_dirs_expand_like_the_typescript_list() {
        // Mirror the four templates of runtimeCommandEnv.ts:15-20 and the expansion semantics of :28-45.
        let lookup = env(&[
            ("APPDATA", "/fake/appdata"),
            ("ProgramFiles", "/fake/programfiles"),
            ("ProgramFiles(x86)", "/fake/x86"),
            ("LOCALAPPDATA", "/fake/localappdata"),
        ]);
        let expanded = expand_windows_vars("%APPDATA%\\npm", &lookup);
        assert_eq!(expanded, "/fake/appdata\\npm");
        assert_eq!(
            expand_windows_vars("%ProgramFiles(x86)%\\nodejs", &lookup),
            "/fake/x86\\nodejs"
        );
        // Unknown variables are expanded into empty strings (TS also returns "", which is then filtered out by existsSync).
        assert_eq!(expand_windows_vars("%NOPE%\\nodejs", &lookup), "\\nodejs");
        // The real directory does not exist → the list is empty and no error is reported.
        assert!(windows_standard_dirs(&lookup).is_empty());
    }

    #[test]
    fn path_is_split_per_platform_delimiter_and_keeps_order() {
        // Delimiters by platform (Windows ';' / POSIX ':', mirror runtimeCommandEnv's
        // path.delimiter semantics): The test uses the platform delimiter to construct PATH and asserts that the two segments are in the same order.
        let delimiter = if cfg!(windows) { ';' } else { ':' };
        let raw = format!("/opt/first/bin{delimiter}/opt/second/bin{delimiter}");
        let config = DiscoveryConfig::from_lookup(env(&[
            ("PATH", Box::leak(raw.into_boxed_str())),
            ("HOME", "/home/user"),
        ]));
        assert_eq!(
            config.path_dirs,
            vec![
                PathBuf::from("/opt/first/bin"),
                PathBuf::from("/opt/second/bin")
            ]
        );
        // Empty entries are discarded, PATH takes precedence over standard locations (user's own choice wins).
        let candidates = config.candidates();
        assert_eq!(
            candidates.first().map(|(_, source)| *source),
            Some(DiscoverySource::Path)
        );
    }

    #[test]
    fn version_manager_children_rank_newest_first() {
        let root = std::env::temp_dir().join(format!("zcode-node-repl-vm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for name in ["v18.20.4", "v24.14.0", "notaversion"] {
            std::fs::create_dir_all(root.join(name).join("bin")).expect("create vm child");
        }
        let ranked = versioned_children(&root, &["bin"]);
        assert_eq!(
            ranked.first(),
            Some(&root.join("v24.14.0").join("bin")),
            "newest Node wins: {ranked:?}"
        );
        assert_eq!(ranked[1], root.join("v18.20.4").join("bin"));
        assert_eq!(ranked.last(), Some(&root.join("notaversion").join("bin")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn explicit_candidate_precedes_everything() {
        let mut config = DiscoveryConfig::only_in(vec![PathBuf::from("/usr/bin")]);
        config.explicit = Some(PathBuf::from("/opt/mise/node"));
        let candidates = config.candidates();
        assert_eq!(
            candidates[0],
            (PathBuf::from("/opt/mise/node"), DiscoverySource::Explicit)
        );
        assert_eq!(candidates[1].1, DiscoverySource::Path);
    }

    #[test]
    fn min_supported_boundary_is_inclusive() {
        assert!(NodeVersion {
            major: 18,
            minor: 0,
            patch: 0
        }
        .supports_kernel(MIN_SUPPORTED_NODE));
        assert!(!NodeVersion {
            major: 17,
            minor: 9,
            patch: 9
        }
        .supports_kernel(MIN_SUPPORTED_NODE));
    }
}
