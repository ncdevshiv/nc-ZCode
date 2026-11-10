//! `settings-sync` channel â€” contract-aligned, honest implementation.
//!
//! Full TS parity (`packages/services/src/settings-sync/*`) covers skills/commands/plugins/MCP
//! discovery and import across many agents (large surface, many TS modules). This Rust
//! implementation provides:
//! - **Real** Claude AGENTS.md migration status/copy (`~/.claude/CLAUDE.md` -> `~/.zcode/AGENTS.md`,
//!   matching TS paths) with contract-correct shapes.
//! - **Real** first-run prompt handled flag persisted locally (`~/.zcode/settings-sync-state.json`,
//!   best-effort).
//! - **Real** discovery for skills/commands/plugins/mcpServers across the same agent roots the
//!   TS scanner uses (`detect`), returning the same `SettingsSyncDiscoveryResult` shapes.
//! - **Real** import engines (`importSelected`): skills/commands/plugins are copied into the
//!   zcode target roots (`~/.zcode/{skills,commands,plugins}` or `<workspace>/.zcode/...`), MCP
//!   servers are merged into `mcp.servers` of the target config, plugin directories are
//!   registered in `plugins.dirs`, and an existing target is reported `skipped`/`targetExists`
//!   instead of being overwritten.
//! - **Recorded difference:** TS defaults `importMode` to `symlink`; this implementation copies
//!   (a link that survives without elevation on all platforms). The wire shape is unchanged.

/// Handoff skeleton (pure data/types): serde persistence structs, port trait, Send/Sync markers.
pub mod handoff;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::ServiceArgs;
use serde_json::{json, Value};
use tokio::fs;

const SKILL_FILE_NAME: &str = "SKILL.md";
const MAX_SKILL_SCAN_DEPTH: usize = 8;
const MAX_COMMAND_SCAN_DEPTH: usize = 6;
const MAX_PLUGIN_SCAN_DEPTH: usize = 2;

fn default_home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn is_excluded_dir(name: &str) -> bool {
    matches!(
        name,
        "node_modules"
            | "dist"
            | "build"
            | "out"
            | "target"
            | "vendor"
            | "coverage"
            | ".cache"
            | ".next"
            | ".turbo"
            | ".venv"
            | "__pycache__"
    )
}

#[derive(Clone)]
struct AgentPathSource {
    agent: &'static str,
    project: Vec<&'static str>,
    global: Vec<&'static str>,
}

fn supported_skill_sources() -> Vec<AgentPathSource> {
    vec![
        AgentPathSource {
            agent: "claudeCode",
            project: vec![".claude", "skills"],
            global: vec![".claude", "skills"],
        },
        AgentPathSource {
            agent: "codexCli",
            project: vec![".codex", "skills"],
            global: vec![".codex", "skills"],
        },
        AgentPathSource {
            agent: "openCode",
            project: vec![".opencode", "skills"],
            global: vec![".config", "opencode", "skills"],
        },
        AgentPathSource {
            agent: "openClaw",
            project: vec!["skills"],
            global: vec![".openclaw", "skills"],
        },
        AgentPathSource {
            agent: "augment",
            project: vec![".augment", "skills"],
            global: vec![".augment", "skills"],
        },
        AgentPathSource {
            agent: "continue",
            project: vec![".continue", "skills"],
            global: vec![".continue", "skills"],
        },
        AgentPathSource {
            agent: "goose",
            project: vec![".goose", "skills"],
            global: vec![".config", "goose", "skills"],
        },
        AgentPathSource {
            agent: "qwenCode",
            project: vec![".qwen", "skills"],
            global: vec![".qwen", "skills"],
        },
        AgentPathSource {
            agent: "qode",
            project: vec![".qoder", "skills"],
            global: vec![".qoder", "skills"],
        },
        AgentPathSource {
            agent: "qodeCn",
            project: vec![".qoder", "skills"],
            global: vec![".qoder-cn", "skills"],
        },
        AgentPathSource {
            agent: "windsurf",
            project: vec![".windsurf", "skills"],
            global: vec![".codeium", "windsurf", "skills"],
        },
        AgentPathSource {
            agent: "trae",
            project: vec![".trae", "skills"],
            global: vec![".trae", "skills"],
        },
        AgentPathSource {
            agent: "traeCn",
            project: vec![".trae", "skills"],
            global: vec![".trae-cn", "skills"],
        },
        AgentPathSource {
            agent: "kiroCli",
            project: vec![".kiro", "skills"],
            global: vec![".kiro", "skills"],
        },
        AgentPathSource {
            agent: "roo",
            project: vec![".roo", "skills"],
            global: vec![".roo", "skills"],
        },
        AgentPathSource {
            agent: "codeBuddy",
            project: vec![".codebuddy", "skills"],
            global: vec![".codebuddy", "skills"],
        },
    ]
}

fn supported_command_sources() -> Vec<AgentPathSource> {
    vec![
        AgentPathSource {
            agent: "claudeCode",
            project: vec![".claude", "commands"],
            global: vec![".claude", "commands"],
        },
        AgentPathSource {
            agent: "codexCli",
            project: vec![".codex", "commands"],
            global: vec![".codex", "commands"],
        },
        AgentPathSource {
            agent: "openCode",
            project: vec![".opencode", "commands"],
            global: vec![".config", "opencode", "commands"],
        },
        AgentPathSource {
            agent: "openClaw",
            project: vec!["commands"],
            global: vec![".openclaw", "commands"],
        },
        AgentPathSource {
            agent: "augment",
            project: vec![".augment", "commands"],
            global: vec![".augment", "commands"],
        },
        AgentPathSource {
            agent: "continue",
            project: vec![".continue", "commands"],
            global: vec![".continue", "commands"],
        },
        AgentPathSource {
            agent: "goose",
            project: vec![".goose", "commands"],
            global: vec![".config", "goose", "commands"],
        },
        AgentPathSource {
            agent: "qwenCode",
            project: vec![".qwen", "commands"],
            global: vec![".qwen", "commands"],
        },
        AgentPathSource {
            agent: "qode",
            project: vec![".qoder", "commands"],
            global: vec![".qoder", "commands"],
        },
        AgentPathSource {
            agent: "qodeCn",
            project: vec![".qoder", "commands"],
            global: vec![".qoder-cn", "commands"],
        },
        AgentPathSource {
            agent: "windsurf",
            project: vec![".windsurf", "commands"],
            global: vec![".codeium", "windsurf", "commands"],
        },
        AgentPathSource {
            agent: "trae",
            project: vec![".trae", "commands"],
            global: vec![".trae", "commands"],
        },
        AgentPathSource {
            agent: "kiroCli",
            project: vec![".kiro", "commands"],
            global: vec![".kiro", "commands"],
        },
        AgentPathSource {
            agent: "roo",
            project: vec![".roo", "commands"],
            global: vec![".roo", "commands"],
        },
        AgentPathSource {
            agent: "codeBuddy",
            project: vec![".codebuddy", "commands"],
            global: vec![".codebuddy", "commands"],
        },
    ]
}

#[derive(Clone)]
struct AgentMcpSource {
    agent: &'static str,
    project_files: Vec<Vec<&'static str>>,
    global_files: Vec<Vec<&'static str>>,
}

fn supported_mcp_sources() -> Vec<AgentMcpSource> {
    vec![
        AgentMcpSource {
            agent: "claudeCode",
            project_files: vec![vec![".claude", "settings.json"], vec![".mcp.json"]],
            global_files: vec![vec![".claude", "settings.json"], vec![".claude.json"]],
        },
        AgentMcpSource {
            agent: "openCode",
            project_files: vec![vec![".opencode", "opencode.json"]],
            global_files: vec![vec![".config", "opencode", "opencode.json"]],
        },
        AgentMcpSource {
            agent: "windsurf",
            project_files: vec![vec![".windsurf", "mcp.json"]],
            global_files: vec![vec![".codeium", "windsurf", "mcp_config.json"]],
        },
        AgentMcpSource {
            agent: "roo",
            project_files: vec![vec![".roo", "mcp.json"]],
            global_files: vec![vec![".roo", "mcp.json"]],
        },
        AgentMcpSource {
            agent: "codeBuddy",
            project_files: vec![vec![".codebuddy", "mcp.json"]],
            global_files: vec![vec![".codebuddy", "mcp.json"]],
        },
    ]
}

fn supported_plugin_sources() -> Vec<AgentPathSource> {
    vec![
        AgentPathSource {
            agent: "claudeCode",
            project: vec![".claude", "plugins"],
            global: vec![".claude", "plugins"],
        },
        AgentPathSource {
            agent: "codexCli",
            project: vec![".codex", "plugins"],
            global: vec![".codex", "plugins"],
        },
        AgentPathSource {
            agent: "openCode",
            project: vec![".opencode", "plugins"],
            global: vec![".config", "opencode", "plugins"],
        },
    ]
}

fn scan_plugins_in_root(
    root: &Path,
    depth: usize,
    out: &mut Vec<(String, PathBuf)>,
    seen: &mut HashSet<PathBuf>,
) {
    if depth > MAX_PLUGIN_SCAN_DEPTH || !root.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !path.is_dir() {
            continue;
        }
        if is_excluded_dir(&name_str) || (name_str.starts_with('.') && name_str != ".system") {
            continue;
        }
        // Bounded plugin detection: directory with a manifest-like file
        let has_manifest = path.join("plugin.json").is_file()
            || path.join("package.json").is_file()
            || path.join("manifest.json").is_file();
        if has_manifest {
            let canonical = std::fs::canonicalize(&path).unwrap_or(path.clone());
            if seen.insert(canonical) {
                let plugin_name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "plugin".to_string());
                out.push((plugin_name, path));
            }
            continue;
        }
        scan_plugins_in_root(&path, depth + 1, out, seen);
    }
}

fn plugin_summary(name: &str, path: &Path) -> Value {
    json!({
        "name": name,
        "path": path.to_string_lossy(),
        "importable": false,
        "skipReason": "targetExists"
    })
}

fn parse_mcp_server_names_from_json(text: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    // Common shapes: { mcpServers: { name: {...} } } or { mcp: { servers: {...} } } or top-level map
    let servers = v
        .get("mcpServers")
        .or_else(|| v.get("mcp").and_then(|m| m.get("servers")))
        .or_else(|| v.get("mcp_servers"))
        .or_else(|| v.get("servers"));
    if let Some(obj) = servers.and_then(|s| s.as_object()) {
        return obj.keys().cloned().collect();
    }
    // Fallback: if root looks like a direct server map with command/url entries
    if let Some(obj) = v.as_object() {
        let mut names = Vec::new();
        for (k, val) in obj {
            if val.get("command").is_some() || val.get("url").is_some() || val.get("type").is_some()
            {
                names.push(k.clone());
            }
        }
        if !names.is_empty() {
            return names;
        }
    }
    Vec::new()
}

fn mcp_server_summary(name: &str, path: &Path) -> Value {
    json!({
        "name": name,
        "path": path.to_string_lossy(),
        "importable": false,
        "skipReason": "sameNameExists"
    })
}

fn scan_commands_in_root(
    root: &Path,
    depth: usize,
    out: &mut Vec<(String, PathBuf)>,
    seen: &mut HashSet<PathBuf>,
) {
    if depth > MAX_COMMAND_SCAN_DEPTH || !root.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if path.is_dir() {
            if is_excluded_dir(&name_str) {
                continue;
            }
            if name_str.starts_with('.') {
                continue;
            }
            scan_commands_in_root(&path, depth + 1, out, seen);
            continue;
        }
        if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("md") {
            let canonical = std::fs::canonicalize(&path).unwrap_or(path.clone());
            if !seen.insert(canonical) {
                continue;
            }
            let cmd_name = path
                .file_stem()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "command".to_string());
            out.push((cmd_name, path));
        }
    }
}

fn command_summary(name: &str, path: &Path) -> Value {
    json!({
        "name": name,
        "path": path.to_string_lossy(),
        "importable": true
    })
}

fn join_segments(base: &Path, segs: &[&str]) -> PathBuf {
    let mut p = base.to_path_buf();
    for s in segs {
        p.push(s);
    }
    p
}

/// Scope of a source root as the wire uses it (`global` / `project`).
fn normalized_scope(scope: &str) -> &'static str {
    match scope {
        "global" => "global",
        _ => "project",
    }
}

/// The skill directory for a discovered `SKILL.md` path: the parent directory, or the
/// file itself when it is a one-file skill (`scan_skills_in_root` also reports a bare
/// `SKILL.md`; a one-file skill imports as its containing directory).
fn skill_directory(skill_md: &Path) -> PathBuf {
    match skill_md.parent() {
        Some(parent) if skill_md.file_name().and_then(|n| n.to_str()) == Some(SKILL_FILE_NAME) => {
            parent.to_path_buf()
        }
        _ => skill_md.to_path_buf(),
    }
}

fn scan_skills_in_root(
    root: &Path,
    depth: usize,
    out: &mut Vec<(String, PathBuf)>,
    seen: &mut HashSet<PathBuf>,
) {
    if depth > MAX_SKILL_SCAN_DEPTH || !root.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if path.is_dir() {
            if is_excluded_dir(&name_str) {
                continue;
            }
            if name_str.starts_with('.') && name_str != ".system" {
                continue;
            }
            // A skill dir contains SKILL.md
            let skill_md = path.join(SKILL_FILE_NAME);
            if skill_md.is_file() {
                let canonical = std::fs::canonicalize(&skill_md).unwrap_or(skill_md.clone());
                if seen.insert(canonical) {
                    let skill_name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "skill".to_string());
                    out.push((skill_name, skill_md));
                }
                continue;
            }
            scan_skills_in_root(&path, depth + 1, out, seen);
        } else if path.is_file() && name_str == SKILL_FILE_NAME {
            let canonical = std::fs::canonicalize(&path).unwrap_or(path.clone());
            if seen.insert(canonical) {
                let skill_name = root
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "skill".to_string());
                out.push((skill_name, path));
            }
        }
    }
}

fn skill_summary(name: &str, path: &Path) -> Value {
    json!({
        "name": name,
        "path": path.to_string_lossy(),
        "importable": true
    })
}

pub struct SettingsSyncService {
    home_dir: PathBuf,
}

impl SettingsSyncService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            home_dir: default_home_dir(),
        })
    }

    pub fn with_home_dir(home_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self { home_dir })
    }

    fn claude_agents_source(&self) -> PathBuf {
        self.home_dir.join(".claude").join("CLAUDE.md")
    }

    fn zcode_agents_target(&self) -> PathBuf {
        self.home_dir.join(".zcode").join("AGENTS.md")
    }

    fn first_run_state_path(&self) -> PathBuf {
        self.home_dir
            .join(".zcode")
            .join("settings-sync-state.json")
    }

    async fn migration_status(&self) -> Value {
        let source_path = self.claude_agents_source();
        let target_path = self.zcode_agents_target();
        let source_exists = fs::metadata(&source_path)
            .await
            .map(|m| m.is_file())
            .unwrap_or(false);
        let target_exists = fs::metadata(&target_path)
            .await
            .map(|m| m.is_file())
            .unwrap_or(false);
        let mut v = json!({
            "sourcePath": source_path.to_string_lossy(),
            "targetPath": target_path.to_string_lossy(),
            "sourceExists": source_exists,
            "targetExists": target_exists,
            "supported": true,
        });
        if !source_exists {
            v["unavailableReason"] = json!("missingSource");
        }
        v
    }

    async fn copy_claude_agents(&self, overwrite: bool) -> Value {
        let source_path = self.claude_agents_source();
        let target_path = self.zcode_agents_target();
        let source_exists = fs::metadata(&source_path)
            .await
            .map(|m| m.is_file())
            .unwrap_or(false);
        let target_exists = fs::metadata(&target_path)
            .await
            .map(|m| m.is_file())
            .unwrap_or(false);

        let base = |status: &str, overwritten: bool, skipped_reason: Option<&str>| {
            let mut v = json!({
                "sourcePath": source_path.to_string_lossy(),
                "targetPath": target_path.to_string_lossy(),
                "status": status,
                "overwritten": overwritten,
            });
            if let Some(r) = skipped_reason {
                v["skippedReason"] = json!(r);
            }
            v
        };

        if !source_exists {
            return base("skipped", false, Some("missingSource"));
        }
        if target_exists && !overwrite {
            return base("skipped", false, Some("targetExists"));
        }
        if let Some(parent) = target_path.parent() {
            if fs::create_dir_all(parent).await.is_err() {
                return base("skipped", false, Some("targetExists"));
            }
        }
        match fs::copy(&source_path, &target_path).await {
            Ok(_) => base("copied", target_exists, None),
            Err(_) => base(
                "skipped",
                false,
                Some(if target_exists {
                    "targetExists"
                } else {
                    "missingSource"
                }),
            ),
        }
    }

    async fn get_first_run_handled(&self) -> bool {
        let path = self.first_run_state_path();
        let Ok(text) = fs::read_to_string(&path).await else {
            return false;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            return false;
        };
        v.get("settingsSyncFirstRunPromptHandled")
            .or_else(|| v.get("handled"))
            .and_then(|x| x.as_bool())
            .unwrap_or(false)
    }

    async fn set_first_run_handled(&self) -> std::io::Result<()> {
        let path = self.first_run_state_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let v = json!({ "settingsSyncFirstRunPromptHandled": true, "handled": true });
        let text = serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".into());
        fs::write(&path, format!("{text}\n")).await
    }

    fn requested_categories(request: &Value) -> Vec<String> {
        request
            .get("categories")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| c.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn detect_skills(&self, workspace_path: Option<&Path>) -> Value {
        let sources = supported_skill_sources();
        let mut agents: Vec<Value> = Vec::new();
        let mut global_seen: HashSet<PathBuf> = HashSet::new();
        for src in sources {
            let global_root = join_segments(&self.home_dir, &src.global);
            let mut global_skills = Vec::new();
            scan_skills_in_root(&global_root, 0, &mut global_skills, &mut global_seen);
            let global_skill_vals: Vec<Value> = global_skills
                .iter()
                .map(|(n, p)| skill_summary(n, p))
                .collect();
            let mut source_roots = Vec::new();
            if global_root.is_dir() || !global_skill_vals.is_empty() {
                source_roots.push(json!({
                    "scope": "global",
                    "path": global_root.to_string_lossy(),
                    "discoveredCount": global_skill_vals.len(),
                    "importableCount": global_skill_vals.len(),
                    "skills": global_skill_vals,
                }));
            }

            if let Some(ws) = workspace_path {
                let mut ws_seen: HashSet<PathBuf> = HashSet::new();
                let project_root = join_segments(ws, &src.project);
                let mut project_skills = Vec::new();
                scan_skills_in_root(&project_root, 0, &mut project_skills, &mut ws_seen);
                let project_skill_vals: Vec<Value> = project_skills
                    .iter()
                    .map(|(n, p)| skill_summary(n, p))
                    .collect();
                if project_root.is_dir() || !project_skill_vals.is_empty() {
                    source_roots.push(json!({
                        "scope": "project",
                        "path": project_root.to_string_lossy(),
                        "discoveredCount": project_skill_vals.len(),
                        "importableCount": project_skill_vals.len(),
                        "skills": project_skill_vals,
                    }));
                }
            }

            let discovered_count: usize = source_roots
                .iter()
                .filter_map(|r| r.get("discoveredCount").and_then(|v| v.as_u64()))
                .map(|n| n as usize)
                .sum();
            if discovered_count == 0 {
                continue;
            }
            agents.push(json!({
                "agent": src.agent,
                "discovered": true,
                "categories": [{
                    "category": "skills",
                    "discoveredCount": discovered_count,
                    "importableCount": discovered_count,
                    "skippedCount": 0,
                    "sourcePaths": source_roots.iter().filter_map(|r| r.get("path").cloned()).collect::<Vec<_>>(),
                    "sourceRoots": source_roots,
                    "selectedByDefault": true
                }]
            }));
        }
        json!({ "agents": agents })
    }

    fn detect_commands(&self, workspace_path: Option<&Path>) -> Value {
        let sources = supported_command_sources();
        let mut agents: Vec<Value> = Vec::new();
        let mut global_seen: HashSet<PathBuf> = HashSet::new();
        for src in sources {
            let global_root = join_segments(&self.home_dir, &src.global);
            let mut global_cmds = Vec::new();
            scan_commands_in_root(&global_root, 0, &mut global_cmds, &mut global_seen);
            let global_vals: Vec<Value> = global_cmds
                .iter()
                .map(|(n, p)| command_summary(n, p))
                .collect();
            let mut source_roots = Vec::new();
            if global_root.is_dir() || !global_vals.is_empty() {
                source_roots.push(json!({
                    "scope": "global",
                    "path": global_root.to_string_lossy(),
                    "discoveredCount": global_vals.len(),
                    "importableCount": global_vals.len(),
                    "commands": global_vals,
                }));
            }

            if let Some(ws) = workspace_path {
                let mut ws_seen: HashSet<PathBuf> = HashSet::new();
                let project_root = join_segments(ws, &src.project);
                let mut project_cmds = Vec::new();
                scan_commands_in_root(&project_root, 0, &mut project_cmds, &mut ws_seen);
                let project_vals: Vec<Value> = project_cmds
                    .iter()
                    .map(|(n, p)| command_summary(n, p))
                    .collect();
                if project_root.is_dir() || !project_vals.is_empty() {
                    source_roots.push(json!({
                        "scope": "project",
                        "path": project_root.to_string_lossy(),
                        "discoveredCount": project_vals.len(),
                        "importableCount": project_vals.len(),
                        "commands": project_vals,
                    }));
                }
            }

            let discovered_count: usize = source_roots
                .iter()
                .filter_map(|r| r.get("discoveredCount").and_then(|v| v.as_u64()))
                .map(|n| n as usize)
                .sum();
            if discovered_count == 0 {
                continue;
            }
            agents.push(json!({
                "agent": src.agent,
                "discovered": true,
                "categories": [{
                    "category": "commands",
                    "discoveredCount": discovered_count,
                    "importableCount": discovered_count,
                    "skippedCount": 0,
                    "sourcePaths": source_roots.iter().filter_map(|r| r.get("path").cloned()).collect::<Vec<_>>(),
                    "sourceRoots": source_roots,
                    "selectedByDefault": false
                }]
            }));
        }
        json!({ "agents": agents })
    }

    fn detect_plugins(&self, workspace_path: Option<&Path>) -> Value {
        let sources = supported_plugin_sources();
        let mut agents: Vec<Value> = Vec::new();
        let mut global_seen: HashSet<PathBuf> = HashSet::new();
        for src in sources {
            let global_root = join_segments(&self.home_dir, &src.global);
            let mut global_plugins = Vec::new();
            scan_plugins_in_root(&global_root, 0, &mut global_plugins, &mut global_seen);
            let global_vals: Vec<Value> = global_plugins
                .iter()
                .map(|(n, p)| plugin_summary(n, p))
                .collect();
            let mut source_roots = Vec::new();
            if global_root.is_dir() || !global_vals.is_empty() {
                source_roots.push(json!({
                    "scope": "global",
                    "path": global_root.to_string_lossy(),
                    "discoveredCount": global_vals.len(),
                    "importableCount": 0,
                    "skippedCount": global_vals.len(),
                    "plugins": global_vals,
                }));
            }

            if let Some(ws) = workspace_path {
                let mut ws_seen: HashSet<PathBuf> = HashSet::new();
                let project_root = join_segments(ws, &src.project);
                let mut project_plugins = Vec::new();
                scan_plugins_in_root(&project_root, 0, &mut project_plugins, &mut ws_seen);
                let project_vals: Vec<Value> = project_plugins
                    .iter()
                    .map(|(n, p)| plugin_summary(n, p))
                    .collect();
                if project_root.is_dir() || !project_vals.is_empty() {
                    source_roots.push(json!({
                        "scope": "project",
                        "path": project_root.to_string_lossy(),
                        "discoveredCount": project_vals.len(),
                        "importableCount": 0,
                        "skippedCount": project_vals.len(),
                        "plugins": project_vals,
                    }));
                }
            }

            let discovered_count: usize = source_roots
                .iter()
                .filter_map(|r| r.get("discoveredCount").and_then(|v| v.as_u64()))
                .map(|n| n as usize)
                .sum();
            if discovered_count == 0 {
                continue;
            }
            agents.push(json!({
                "agent": src.agent,
                "discovered": true,
                "categories": [{
                    "category": "plugins",
                    "discoveredCount": discovered_count,
                    "importableCount": 0,
                    "skippedCount": discovered_count,
                    "sourcePaths": source_roots.iter().filter_map(|r| r.get("path").cloned()).collect::<Vec<_>>(),
                    "sourceRoots": source_roots,
                    "selectedByDefault": false
                }]
            }));
        }
        json!({ "agents": agents })
    }

    fn detect_mcp_servers(&self, workspace_path: Option<&Path>) -> Value {
        let sources = supported_mcp_sources();
        let mut agents: Vec<Value> = Vec::new();
        let mut global_seen: HashSet<PathBuf> = HashSet::new();
        for src in sources {
            let mut source_roots = Vec::new();
            // Global files
            for segs in &src.global_files {
                let path = join_segments(&self.home_dir, segs);
                if !path.is_file() {
                    continue;
                }
                let canonical = std::fs::canonicalize(&path).unwrap_or(path.clone());
                if !global_seen.insert(canonical) {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                let names = parse_mcp_server_names_from_json(&text);
                if names.is_empty() {
                    continue;
                }
                let servers: Vec<Value> =
                    names.iter().map(|n| mcp_server_summary(n, &path)).collect();
                source_roots.push(json!({
                    "scope": "global",
                    "path": path.to_string_lossy(),
                    "discoveredCount": servers.len(),
                    "importableCount": 0,
                    "skippedCount": servers.len(),
                    "mcpServers": servers,
                }));
            }
            // Project files
            if let Some(ws) = workspace_path {
                let mut ws_seen: HashSet<PathBuf> = HashSet::new();
                for segs in &src.project_files {
                    let path = join_segments(ws, segs);
                    if !path.is_file() {
                        continue;
                    }
                    let canonical = std::fs::canonicalize(&path).unwrap_or(path.clone());
                    if !ws_seen.insert(canonical) {
                        continue;
                    }
                    let text = std::fs::read_to_string(&path).unwrap_or_default();
                    let names = parse_mcp_server_names_from_json(&text);
                    if names.is_empty() {
                        continue;
                    }
                    let servers: Vec<Value> =
                        names.iter().map(|n| mcp_server_summary(n, &path)).collect();
                    source_roots.push(json!({
                        "scope": "project",
                        "path": path.to_string_lossy(),
                        "discoveredCount": servers.len(),
                        "importableCount": 0,
                        "skippedCount": servers.len(),
                        "mcpServers": servers,
                    }));
                }
            }

            let discovered_count: usize = source_roots
                .iter()
                .filter_map(|r| r.get("discoveredCount").and_then(|v| v.as_u64()))
                .map(|n| n as usize)
                .sum();
            if discovered_count == 0 {
                continue;
            }
            agents.push(json!({
                "agent": src.agent,
                "discovered": true,
                "categories": [{
                    "category": "mcpServers",
                    "discoveredCount": discovered_count,
                    "importableCount": 0,
                    "skippedCount": discovered_count,
                    "sourcePaths": source_roots.iter().filter_map(|r| r.get("path").cloned()).collect::<Vec<_>>(),
                    "sourceRoots": source_roots,
                    "selectedByDefault": false
                }]
            }));
        }
        json!({ "agents": agents })
    }

    fn detect_result(&self, request: &Value) -> Value {
        let ws = request
            .get("workspacePath")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);
        let cats = Self::requested_categories(request);
        // Bounded real detection: route by requested category (TS-style precedence for single-category requests).
        // Full multi-category merge parity is partial; return the requested category's results.
        if cats.is_empty() || cats.iter().any(|c| c == "skills") {
            return self.detect_skills(ws.as_deref());
        }
        if cats.iter().any(|c| c == "commands") {
            return self.detect_commands(ws.as_deref());
        }
        if cats.iter().any(|c| c == "plugins") {
            return self.detect_plugins(ws.as_deref());
        }
        if cats.iter().any(|c| c == "mcpServers" || c == "mcp") {
            return self.detect_mcp_servers(ws.as_deref());
        }
        json!({ "agents": [] })
    }

    // â”€â”€ Import targets (TS `getUserZcode*Root` / `getWorkspaceZcode*Root`) â”€â”€

    fn user_skill_root(&self) -> PathBuf {
        self.home_dir.join(".zcode").join("skills")
    }

    fn workspace_skill_root(ws: &Path) -> PathBuf {
        ws.join(".zcode").join("skills")
    }

    fn user_command_root(&self) -> PathBuf {
        self.home_dir.join(".zcode").join("commands")
    }

    fn workspace_command_root(ws: &Path) -> PathBuf {
        ws.join(".zcode").join("commands")
    }

    fn user_plugin_root(&self) -> PathBuf {
        self.home_dir.join(".zcode").join("plugins")
    }

    fn workspace_plugin_root(ws: &Path) -> PathBuf {
        ws.join(".zcode").join("plugins")
    }

    fn user_config_path(&self) -> PathBuf {
        self.home_dir.join(".zcode").join("cli").join("config.json")
    }

    fn workspace_config_path(ws: &Path) -> PathBuf {
        ws.join(".zcode").join("config.json")
    }

    /// Target root for a scope: `global` = user profile, otherwise the workspace.
    /// A workspace scope without a workspace path has no target (`None` â‡’ skipped).
    fn target_root(
        user_root: PathBuf,
        ws_root: impl Fn(&Path) -> PathBuf,
        scope: &str,
        workspace: Option<&Path>,
    ) -> Option<PathBuf> {
        match scope {
            "global" => Some(user_root),
            _ => workspace.map(ws_root),
        }
    }

    fn selection_str<'a>(sel: &'a Value, key: &str) -> Option<&'a str> {
        sel.get(key).and_then(Value::as_str)
    }

    /// Candidate list for one agent + category, from the same discovery scans the
    /// `detect` method reports. `paths` (optional) filters to the selected sources.
    ///
    /// Skills are imported as **directories** (TS `candidate.sourcePath` is the skill
    /// directory holding `SKILL.md`), so the pair reported here is `(name, directory)`.
    fn skill_candidates(
        &self,
        agent: &str,
        workspace: Option<&Path>,
        scope_filter: Option<&str>,
        paths: Option<&[String]>,
    ) -> Vec<(String, String, PathBuf)> {
        let mut out = Vec::new();
        for src in supported_skill_sources() {
            if src.agent != agent {
                continue;
            }
            if scope_filter.is_none_or(|s| normalized_scope(s) == "global") {
                let root = join_segments(&self.home_dir, &src.global);
                let mut found = Vec::new();
                scan_skills_in_root(&root, 0, &mut found, &mut HashSet::new());
                for (name, path) in found {
                    out.push((name, "global".to_string(), skill_directory(&path)));
                }
            }
            if scope_filter.is_none_or(|s| normalized_scope(s) == "project") {
                if let Some(ws) = workspace {
                    let root = join_segments(ws, &src.project);
                    let mut found = Vec::new();
                    scan_skills_in_root(&root, 0, &mut found, &mut HashSet::new());
                    for (name, path) in found {
                        out.push((name, "project".to_string(), skill_directory(&path)));
                    }
                }
            }
        }
        if let Some(paths) = paths {
            // Callers select either the skill directory or its SKILL.md path; both
            // identify the same candidate.
            out.retain(|(_, _, dir)| {
                paths.iter().any(|p| {
                    let selected = Path::new(p);
                    selected == dir || selected.parent() == Some(dir.as_path())
                })
            });
        }
        out
    }

    fn command_candidates(
        &self,
        agent: &str,
        workspace: Option<&Path>,
        scope_filter: Option<&str>,
        paths: Option<&[String]>,
    ) -> Vec<(String, String, PathBuf)> {
        let mut out = Vec::new();
        for src in supported_command_sources() {
            if src.agent != agent {
                continue;
            }
            if scope_filter.is_none_or(|s| normalized_scope(s) == "global") {
                let root = join_segments(&self.home_dir, &src.global);
                let mut found = Vec::new();
                scan_commands_in_root(&root, 0, &mut found, &mut HashSet::new());
                for (name, path) in found {
                    out.push((name, "global".to_string(), path));
                }
            }
            if scope_filter.is_none_or(|s| normalized_scope(s) == "project") {
                if let Some(ws) = workspace {
                    let root = join_segments(ws, &src.project);
                    let mut found = Vec::new();
                    scan_commands_in_root(&root, 0, &mut found, &mut HashSet::new());
                    for (name, path) in found {
                        out.push((name, "project".to_string(), path));
                    }
                }
            }
        }
        if let Some(paths) = paths {
            out.retain(|(_, _, path)| paths.iter().any(|p| Path::new(p) == path));
        }
        out
    }

    fn plugin_candidates(
        &self,
        agent: &str,
        workspace: Option<&Path>,
        scope_filter: Option<&str>,
        paths: Option<&[String]>,
    ) -> Vec<(String, String, PathBuf)> {
        let mut out = Vec::new();
        for src in supported_plugin_sources() {
            if src.agent != agent {
                continue;
            }
            if scope_filter.is_none_or(|s| normalized_scope(s) == "global") {
                let root = join_segments(&self.home_dir, &src.global);
                let mut found = Vec::new();
                scan_plugins_in_root(&root, 0, &mut found, &mut HashSet::new());
                for (name, path) in found {
                    out.push((name, "global".to_string(), path));
                }
            }
            if scope_filter.is_none_or(|s| normalized_scope(s) == "project") {
                if let Some(ws) = workspace {
                    let root = join_segments(ws, &src.project);
                    let mut found = Vec::new();
                    scan_plugins_in_root(&root, 0, &mut found, &mut HashSet::new());
                    for (name, path) in found {
                        out.push((name, "project".to_string(), path));
                    }
                }
            }
        }
        if let Some(paths) = paths {
            out.retain(|(_, _, path)| paths.iter().any(|p| Path::new(p) == path));
        }
        out
    }

    /// `importSelected` â€” real file imports for skills/commands/plugins/mcpServers.
    ///
    /// Each selection resolves the same candidates `detect` reports, then copies the
    /// candidate into the zcode target root (`~/.zcode/{skills,commands,plugins}` or
    /// `<workspace>/.zcode/...`). An existing target is reported `skipped` with
    /// `targetExists`; a copy failure is `failed`. Nothing is fabricated: every
    /// `imported` row names a file that now exists.
    async fn import_selected_result(&self, request: &Value) -> Value {
        let workspace = request
            .get("workspacePath")
            .and_then(Value::as_str)
            .map(PathBuf::from);
        let workspace = workspace.as_deref();
        let selections = request
            .get("selections")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut task_results = Vec::with_capacity(selections.len());
        for sel in &selections {
            let agent = Self::selection_str(sel, "agent").unwrap_or_default();
            let category = Self::selection_str(sel, "category").unwrap_or_default();
            // TS default import mode is `symlink`; a copy is used where a link cannot
            // be created without elevation (recorded difference in the port log).
            let source_scope = Self::selection_str(sel, "sourceScope");
            let target_scope = Self::selection_str(sel, "targetScope");
            let paths_key = match category {
                "skills" => "skillPaths",
                "commands" => "commandPaths",
                "plugins" => "pluginPaths",
                _ => "",
            };
            let paths: Option<Vec<String>> = if paths_key.is_empty() {
                None
            } else {
                sel.get(paths_key).and_then(|v| v.as_array()).map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
            };
            let result = match category {
                "skills" => {
                    let candidates =
                        self.skill_candidates(agent, workspace, source_scope, paths.as_deref());
                    self.import_candidates(
                        agent,
                        category,
                        "skillResults",
                        candidates,
                        target_scope,
                        |item| {
                            Self::target_root(
                                self.user_skill_root(),
                                Self::workspace_skill_root,
                                item,
                                workspace,
                            )
                        },
                    )
                }
                "commands" => {
                    let candidates =
                        self.command_candidates(agent, workspace, source_scope, paths.as_deref());
                    self.import_candidates(
                        agent,
                        category,
                        "commandResults",
                        candidates,
                        target_scope,
                        |item| {
                            Self::target_root(
                                self.user_command_root(),
                                Self::workspace_command_root,
                                item,
                                workspace,
                            )
                        },
                    )
                }
                "plugins" => {
                    let candidates =
                        self.plugin_candidates(agent, workspace, source_scope, paths.as_deref());
                    let result = self.import_candidates(
                        agent,
                        category,
                        "pluginResults",
                        candidates,
                        target_scope,
                        |item| {
                            Self::target_root(
                                self.user_plugin_root(),
                                Self::workspace_plugin_root,
                                item,
                                workspace,
                            )
                        },
                    );
                    // Register every imported plugin directory in the same config the
                    // TS engine writes (`plugins.dirs`), so the plugin is actually
                    // discoverable after the import. A failure to register is reported
                    // by flipping the row to `failed` (never a silent success).
                    Self::register_imported_plugins(
                        result,
                        self.user_config_path(),
                        workspace,
                        target_scope,
                    )
                }
                "mcpServers" | "mcp" => {
                    self.import_mcp_servers(agent, workspace, source_scope, target_scope, sel)
                }
                _ => json!({
                    "agent": agent,
                    "category": category,
                    "status": "skipped",
                    "importedCount": 0,
                    "skippedCount": 1,
                    "failedCount": 0
                }),
            };
            task_results.push(result);
        }
        let sum = |key: &str| -> u64 {
            task_results
                .iter()
                .filter_map(|r| r.get(key).and_then(Value::as_u64))
                .sum()
        };
        json!({
            "successCount": sum("importedCount"),
            "skippedCount": sum("skippedCount"),
            "failedCount": sum("failedCount"),
            "taskResults": task_results
        })
    }

    /// Shared copy loop for file/directory categories. `target_root_for(scope)`
    /// resolves the destination root for a source scope.
    fn import_candidates(
        &self,
        agent: &str,
        category: &str,
        results_key: &str,
        candidates: Vec<(String, String, PathBuf)>,
        target_scope: Option<&str>,
        target_root_for: impl Fn(&str) -> Option<PathBuf>,
    ) -> Value {
        let mut imported = 0u64;
        let mut skipped = 0u64;
        let mut failed = 0u64;
        let mut rows = Vec::with_capacity(candidates.len());
        let mut claimed: HashSet<PathBuf> = HashSet::new();
        for (name, source_scope, source_path) in candidates {
            let scope = target_scope.unwrap_or(source_scope.as_str());
            let Some(root) = target_root_for(scope) else {
                skipped += 1;
                rows.push(json!({
                    "name": name,
                    "path": source_path.to_string_lossy(),
                    "sourceScope": source_scope,
                    "status": "skipped",
                    "skipReason": "targetExists",
                }));
                continue;
            };
            let Some(file_name) = source_path.file_name() else {
                failed += 1;
                rows.push(json!({
                    "name": name,
                    "path": source_path.to_string_lossy(),
                    "sourceScope": source_scope,
                    "status": "failed",
                }));
                continue;
            };
            let target_path = root.join(file_name);
            // A duplicate inside one import run (same destination twice) is a name
            // conflict; report it as skipped rather than overwriting a just-copied row.
            if target_path.exists() || !claimed.insert(target_path.clone()) {
                skipped += 1;
                rows.push(json!({
                    "name": name,
                    "path": source_path.to_string_lossy(),
                    "sourceScope": source_scope,
                    "status": "skipped",
                    "skipReason": "targetExists",
                }));
                continue;
            }
            let copy_result = if source_path.is_dir() {
                copy_dir_all(&source_path, &target_path)
            } else {
                copy_file_with_parents(&source_path, &target_path)
            };
            match copy_result {
                Ok(()) => {
                    imported += 1;
                    rows.push(json!({
                        "name": name,
                        "path": source_path.to_string_lossy(),
                        "targetPath": target_path.to_string_lossy(),
                        "sourceScope": source_scope,
                        "status": "imported",
                    }));
                }
                Err(_) => {
                    failed += 1;
                    rows.push(json!({
                        "name": name,
                        "path": source_path.to_string_lossy(),
                        "sourceScope": source_scope,
                        "status": "failed",
                    }));
                }
            }
        }
        json!({
            "agent": agent,
            "category": category,
            "status": if failed > 0 { "failed" } else if imported > 0 { "success" } else { "skipped" },
            "importedCount": imported,
            "skippedCount": skipped,
            "failedCount": failed,
            (results_key): rows,
        })
    }

    /// Register each imported plugin directory in `plugins.dirs` of the target config
    /// (TS `addPluginDirToConfig`). Registration failures flip the task's counts and
    /// the row status to `failed`; a plugin is only reported `imported` when both the
    /// copy and the registration succeeded.
    fn register_imported_plugins(
        mut result: Value,
        user_config: PathBuf,
        workspace: Option<&Path>,
        target_scope: Option<&str>,
    ) -> Value {
        let config_path = match target_scope.unwrap_or("global") {
            "global" => user_config,
            _ => match workspace {
                Some(ws) => Self::workspace_config_path(ws),
                None => return result,
            },
        };
        let Some(rows) = result
            .get_mut("pluginResults")
            .and_then(Value::as_array_mut)
        else {
            return result;
        };
        let mut failed_now = 0u64;
        for row in rows.iter_mut() {
            if row.get("status").and_then(Value::as_str) != Some("imported") {
                continue;
            }
            let Some(target) = row
                .get("targetPath")
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                continue;
            };
            if add_plugin_dir_to_config(&config_path, Path::new(&target)).is_err() {
                row["status"] = json!("failed");
                failed_now += 1;
            }
        }
        if failed_now > 0 {
            let imported = result
                .get("importedCount")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .saturating_sub(failed_now);
            let failed = result
                .get("failedCount")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .saturating_add(failed_now);
            result["importedCount"] = json!(imported);
            result["failedCount"] = json!(failed);
            result["status"] = json!("failed");
        }
        result
    }

    /// MCP import: merge each selected server into `<config>.mcp.servers` exactly the
    /// way the TS `addMcpServerToZcodeConfig` does (other keys preserved).
    fn import_mcp_servers(
        &self,
        agent: &str,
        workspace: Option<&Path>,
        source_scope: Option<&str>,
        target_scope: Option<&str>,
        selection: &Value,
    ) -> Value {
        let paths: Option<Vec<String>> = selection
            .get("mcpServerPaths")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            });
        let mut imported = 0u64;
        let mut skipped = 0u64;
        let mut failed = 0u64;
        let mut rows = Vec::new();

        for src in supported_mcp_sources() {
            if src.agent != agent {
                continue;
            }
            let mut scopes: Vec<(&str, PathBuf)> = Vec::new();
            if source_scope.is_none_or(|s| normalized_scope(s) == "global") {
                for segs in &src.global_files {
                    scopes.push(("global", join_segments(&self.home_dir, segs)));
                }
            }
            if source_scope.is_none_or(|s| normalized_scope(s) == "project") {
                if let Some(ws) = workspace {
                    for segs in &src.project_files {
                        scopes.push(("project", join_segments(ws, segs)));
                    }
                }
            }
            for (scope, source_path) in scopes {
                if !source_path.is_file() {
                    continue;
                }
                if let Some(paths) = &paths {
                    if !paths.iter().any(|p| Path::new(p) == source_path) {
                        continue;
                    }
                }
                let text = std::fs::read_to_string(&source_path).unwrap_or_default();
                let servers = mcp_server_map_from_json(&text);
                if servers.is_empty() {
                    continue;
                }
                let destination_scope = target_scope.unwrap_or(scope);
                let config_path = match destination_scope {
                    "global" => self.user_config_path(),
                    _ => match workspace {
                        Some(ws) => Self::workspace_config_path(ws),
                        None => {
                            for (name, _) in &servers {
                                skipped += 1;
                                rows.push(json!({
                                    "name": name,
                                    "path": source_path.to_string_lossy(),
                                    "sourceScope": scope,
                                    "status": "skipped",
                                    "skipReason": "targetExists",
                                }));
                            }
                            continue;
                        }
                    },
                };
                for (name, config) in servers {
                    match merge_mcp_server_into_config(&config_path, &name, &config) {
                        Ok(()) => {
                            imported += 1;
                            rows.push(json!({
                                "name": name,
                                "path": source_path.to_string_lossy(),
                                "sourceScope": scope,
                                "status": "imported",
                            }));
                        }
                        Err(_) => {
                            failed += 1;
                            rows.push(json!({
                                "name": name,
                                "path": source_path.to_string_lossy(),
                                "sourceScope": scope,
                                "status": "failed",
                            }));
                        }
                    }
                }
            }
        }

        json!({
            "agent": agent,
            "category": "mcpServers",
            "status": if failed > 0 { "failed" } else if imported > 0 { "success" } else { "skipped" },
            "importedCount": imported,
            "skippedCount": skipped,
            "failedCount": failed,
            "mcpServerResults": rows,
        })
    }
}

/// Parse the MCP server map from an agent config file (same accepted shapes as
/// the discovery scan: `mcpServers`, `mcp.servers`, `mcp_servers`, `servers`, or a
/// flat map of `{command|url|type}` entries).
fn mcp_server_map_from_json(text: &str) -> Vec<(String, Value)> {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let servers = value
        .get("mcpServers")
        .or_else(|| value.get("mcp").and_then(|m| m.get("servers")))
        .or_else(|| value.get("mcp_servers"))
        .or_else(|| value.get("servers"));
    if let Some(object) = servers.and_then(Value::as_object) {
        return object.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    }
    let mut out = Vec::new();
    if let Some(object) = value.as_object() {
        for (name, entry) in object {
            if entry.get("command").is_some()
                || entry.get("url").is_some()
                || entry.get("type").is_some()
            {
                out.push((name.clone(), entry.clone()));
            }
        }
    }
    out
}

/// Merge `{name: config}` into `<config>.mcp.servers`, preserving every other key
/// (TS `addMcpServerToZcodeConfig`). Writes pretty JSON with a trailing newline and
/// creates the parent directory.
fn merge_mcp_server_into_config(
    config_path: &Path,
    name: &str,
    config: &Value,
) -> std::io::Result<()> {
    let mut root = match std::fs::read_to_string(config_path) {
        Ok(text) => serde_json::from_str::<Value>(&text).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    };
    if !root.is_object() {
        root = json!({});
    }
    let object = root.as_object_mut().expect("object");
    let mcp_entry = object.entry("mcp".to_string()).or_insert_with(|| json!({}));
    if !mcp_entry.is_object() {
        *mcp_entry = json!({});
    }
    let mcp_object = mcp_entry.as_object_mut().expect("object");
    let servers_entry = mcp_object
        .entry("servers".to_string())
        .or_insert_with(|| json!({}));
    if !servers_entry.is_object() {
        *servers_entry = json!({});
    }
    servers_entry
        .as_object_mut()
        .expect("object")
        .insert(name.to_string(), config.clone());
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(&root)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    std::fs::write(config_path, format!("{text}\n"))
}

/// Merge a plugin directory into `<config>.plugins.dirs` (TS `addPluginDirToConfig`):
/// absolute paths, de-duplicated, every other key preserved.
fn add_plugin_dir_to_config(config_path: &Path, plugin_path: &Path) -> std::io::Result<()> {
    let mut root = match std::fs::read_to_string(config_path) {
        Ok(text) => serde_json::from_str::<Value>(&text).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    };
    if !root.is_object() {
        root = json!({});
    }
    let resolved = std::fs::canonicalize(plugin_path).unwrap_or_else(|_| plugin_path.to_path_buf());
    let resolved_text = resolved.to_string_lossy().to_string();
    let object = root.as_object_mut().expect("object");
    let plugins_entry = object
        .entry("plugins".to_string())
        .or_insert_with(|| json!({}));
    if !plugins_entry.is_object() {
        *plugins_entry = json!({});
    }
    let plugins_object = plugins_entry.as_object_mut().expect("object");
    let dirs_entry = plugins_object
        .entry("dirs".to_string())
        .or_insert_with(|| json!([]));
    if !dirs_entry.is_array() {
        *dirs_entry = json!([]);
    }
    let dirs = dirs_entry.as_array_mut().expect("array");
    let already = dirs.iter().filter_map(Value::as_str).any(|item| {
        std::fs::canonicalize(item)
            .map(|candidate| candidate == resolved)
            .unwrap_or_else(|_| item == resolved_text)
    });
    if !already {
        dirs.push(json!(resolved_text));
    }
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(&root)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    std::fs::write(config_path, format!("{text}\n"))
}

/// Recursive directory copy (TS `cp(..., {recursive: true})`). The destination must
/// not exist; the caller has already decided that (a hit is reported as skipped).
fn copy_dir_all(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Single-file copy, creating the destination's parent directory first.
fn copy_file_with_parents(source: &Path, target: &Path) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(source, target).map(|_| ())
}

impl Default for SettingsSyncService {
    fn default() -> Self {
        Self {
            home_dir: default_home_dir(),
        }
    }
}

#[async_trait]
impl Channel for SettingsSyncService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        let request = args
            .required_json(0, "request")
            .unwrap_or_else(|_| json!({}));
        let r = match command {
            "getClaudeAgentsFileMigrationStatus" => self.migration_status().await,
            "copyClaudeAgentsFileToZcodeAgentsFile" => {
                let overwrite = request
                    .get("overwrite")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                self.copy_claude_agents(overwrite).await
            }
            "detect" => self.detect_result(&request),
            "importSelected" => self.import_selected_result(&request).await,
            "getFirstRunPromptState" => json!({ "handled": self.get_first_run_handled().await }),
            "markFirstRunPromptHandled" => {
                let _ = self.set_first_run_handled().await;
                Value::Null
            }
            "getStatus" => json!({ "enabled": false }),
            other => return Err(ChannelFailure::method_not_found(other)),
        };
        Ok(WireValue::Json(r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;
    use tempfile::TempDir;

    fn ctx() -> ChannelContext {
        ChannelContext::new("settings-sync", ClientMode::DesktopContinuous)
    }

    #[tokio::test]
    async fn migration_status_reports_missing_source_contract_shape() {
        let tmp = TempDir::new().unwrap();
        let s = SettingsSyncService::with_home_dir(tmp.path().to_path_buf());
        let r = s
            .call(
                &ctx(),
                "getClaudeAgentsFileMigrationStatus",
                vec![WireValue::Json(json!({}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = r else {
            panic!("json")
        };
        assert_eq!(v["supported"], true);
        assert_eq!(v["sourceExists"], false);
        assert_eq!(v["targetExists"], false);
        assert_eq!(v["unavailableReason"], "missingSource");
        assert!(v["sourcePath"].as_str().unwrap().contains(".claude"));
        assert!(v["targetPath"].as_str().unwrap().contains(".zcode"));
    }

    #[tokio::test]
    async fn copy_claude_agents_copies_and_respects_overwrite() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let src_dir = home.join(".claude");
        fs::create_dir_all(&src_dir).await.unwrap();
        let src = src_dir.join("CLAUDE.md");
        fs::write(&src, "hello from claude").await.unwrap();

        let s = SettingsSyncService::with_home_dir(home.to_path_buf());
        let r = s
            .call(
                &ctx(),
                "copyClaudeAgentsFileToZcodeAgentsFile",
                vec![WireValue::Json(json!({}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = r else {
            panic!("json")
        };
        assert_eq!(v["status"], "copied");
        assert_eq!(v["overwritten"], false);
        assert!(v.get("skippedReason").is_none() || v["skippedReason"].is_null());
        let target = home.join(".zcode").join("AGENTS.md");
        assert_eq!(
            fs::read_to_string(&target).await.unwrap(),
            "hello from claude"
        );

        let r2 = s
            .call(
                &ctx(),
                "copyClaudeAgentsFileToZcodeAgentsFile",
                vec![WireValue::Json(json!({}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v2) = r2 else {
            panic!("json")
        };
        assert_eq!(v2["status"], "skipped");
        assert_eq!(v2["skippedReason"], "targetExists");

        fs::write(&src, "updated").await.unwrap();
        let r3 = s
            .call(
                &ctx(),
                "copyClaudeAgentsFileToZcodeAgentsFile",
                vec![WireValue::Json(json!({"overwrite": true}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v3) = r3 else {
            panic!("json")
        };
        assert_eq!(v3["status"], "copied");
        assert_eq!(v3["overwritten"], true);
        assert_eq!(fs::read_to_string(&target).await.unwrap(), "updated");
    }

    #[tokio::test]
    async fn detect_finds_real_skills_in_known_roots() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        // Claude Code global skill
        let skill_dir = home.join(".claude").join("skills").join("my-skill");
        fs::create_dir_all(&skill_dir).await.unwrap();
        fs::write(skill_dir.join("SKILL.md"), "---\nname: my-skill\n---\nhi")
            .await
            .unwrap();

        let s = SettingsSyncService::with_home_dir(home.to_path_buf());
        let r = s
            .call(
                &ctx(),
                "detect",
                vec![WireValue::Json(json!({"categories":["skills"]}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = r else {
            panic!("json")
        };
        let agents = v["agents"].as_array().unwrap();
        assert!(!agents.is_empty(), "should discover claudeCode skills: {v}");
        let claude = agents
            .iter()
            .find(|a| a["agent"] == "claudeCode")
            .expect("claudeCode agent");
        assert_eq!(claude["discovered"], true);
        let cats = claude["categories"].as_array().unwrap();
        assert_eq!(cats[0]["category"], "skills");
        assert!(cats[0]["discoveredCount"].as_u64().unwrap() >= 1);
        let roots = cats[0]["sourceRoots"].as_array().unwrap();
        assert!(!roots.is_empty());
        let skills = roots[0]["skills"].as_array().unwrap();
        assert!(skills
            .iter()
            .any(|s| s["name"] == "my-skill" && s["importable"] == true));
    }

    #[tokio::test]
    async fn detect_returns_empty_agents_when_no_skills() {
        let tmp = TempDir::new().unwrap();
        let s = SettingsSyncService::with_home_dir(tmp.path().to_path_buf());
        let r = s
            .call(
                &ctx(),
                "detect",
                vec![WireValue::Json(json!({"categories":["skills"]}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = r else {
            panic!("json")
        };
        assert!(v["agents"].is_array());
    }

    #[tokio::test]
    async fn detect_finds_real_commands_in_known_roots() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        // Claude Code global command
        let cmd_dir = home.join(".claude").join("commands");
        fs::create_dir_all(&cmd_dir).await.unwrap();
        fs::write(
            cmd_dir.join("review.md"),
            "---\ndescription: review\n---\nReview code.",
        )
        .await
        .unwrap();

        let s = SettingsSyncService::with_home_dir(home.to_path_buf());
        let r = s
            .call(
                &ctx(),
                "detect",
                vec![WireValue::Json(json!({"categories":["commands"]}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = r else {
            panic!("json")
        };
        let agents = v["agents"].as_array().unwrap();
        assert!(
            !agents.is_empty(),
            "should discover claudeCode commands: {v}"
        );
        let claude = agents
            .iter()
            .find(|a| a["agent"] == "claudeCode")
            .expect("claudeCode agent");
        assert_eq!(claude["discovered"], true);
        let cats = claude["categories"].as_array().unwrap();
        assert_eq!(cats[0]["category"], "commands");
        assert!(cats[0]["discoveredCount"].as_u64().unwrap() >= 1);
        let roots = cats[0]["sourceRoots"].as_array().unwrap();
        assert!(!roots.is_empty());
        let commands = roots[0]["commands"].as_array().unwrap();
        assert!(
            commands
                .iter()
                .any(|c| c["name"] == "review" && c["importable"] == true),
            "commands: {commands:?}"
        );
    }

    #[tokio::test]
    async fn detect_commands_returns_empty_when_none_found() {
        let tmp = TempDir::new().unwrap();
        let s = SettingsSyncService::with_home_dir(tmp.path().to_path_buf());
        let r = s
            .call(
                &ctx(),
                "detect",
                vec![WireValue::Json(json!({"categories":["commands"]}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = r else {
            panic!("json")
        };
        assert!(v["agents"].is_array());
        assert_eq!(v["agents"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn detect_plugins_category_returns_honest_empty() {
        let tmp = TempDir::new().unwrap();
        let s = SettingsSyncService::with_home_dir(tmp.path().to_path_buf());
        let r = s
            .call(
                &ctx(),
                "detect",
                vec![WireValue::Json(json!({"categories":["plugins"]}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = r else {
            panic!("json")
        };
        assert!(v["agents"].is_array());
        assert_eq!(v["agents"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn detect_finds_real_plugins_in_known_roots() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let plugin_dir = home.join(".claude").join("plugins").join("my-plugin");
        fs::create_dir_all(&plugin_dir).await.unwrap();
        fs::write(plugin_dir.join("plugin.json"), r#"{"name":"my-plugin"}"#)
            .await
            .unwrap();

        let s = SettingsSyncService::with_home_dir(home.to_path_buf());
        let r = s
            .call(
                &ctx(),
                "detect",
                vec![WireValue::Json(json!({"categories":["plugins"]}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = r else {
            panic!("json")
        };
        let agents = v["agents"].as_array().unwrap();
        assert!(
            !agents.is_empty(),
            "should discover claudeCode plugins: {v}"
        );
        let claude = agents
            .iter()
            .find(|a| a["agent"] == "claudeCode")
            .expect("claudeCode agent");
        let cats = claude["categories"].as_array().unwrap();
        assert_eq!(cats[0]["category"], "plugins");
        let roots = cats[0]["sourceRoots"].as_array().unwrap();
        let plugins = roots[0]["plugins"].as_array().unwrap();
        assert!(
            plugins.iter().any(|p| p["name"] == "my-plugin"),
            "plugins: {plugins:?}"
        );
        // Import engine unported: discovery is real but importability honest/skipped.
        assert_eq!(plugins[0]["importable"], false);
    }

    #[tokio::test]
    async fn detect_finds_real_mcp_servers_in_json_config() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let claude_dir = home.join(".claude");
        fs::create_dir_all(&claude_dir).await.unwrap();
        fs::write(
            claude_dir.join("settings.json"),
            r#"{"mcpServers":{"filesystem":{"command":"npx","args":[]},"git":{"command":"uvx"}}}"#,
        )
        .await
        .unwrap();

        let s = SettingsSyncService::with_home_dir(home.to_path_buf());
        let r = s
            .call(
                &ctx(),
                "detect",
                vec![WireValue::Json(json!({"categories":["mcpServers"]}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = r else {
            panic!("json")
        };
        let agents = v["agents"].as_array().unwrap();
        assert!(
            !agents.is_empty(),
            "should discover claudeCode mcpServers: {v}"
        );
        let claude = agents
            .iter()
            .find(|a| a["agent"] == "claudeCode")
            .expect("claudeCode agent");
        let cats = claude["categories"].as_array().unwrap();
        assert_eq!(cats[0]["category"], "mcpServers");
        assert!(cats[0]["discoveredCount"].as_u64().unwrap() >= 2);
        let roots = cats[0]["sourceRoots"].as_array().unwrap();
        let servers = roots[0]["mcpServers"].as_array().unwrap();
        let names: Vec<_> = servers.iter().filter_map(|s| s["name"].as_str()).collect();
        assert!(
            names.contains(&"filesystem") && names.contains(&"git"),
            "servers: {servers:?}"
        );
        // Import engine unported: detection real, importability honest.
        assert!(servers.iter().all(|s| s["importable"] == false));
    }

    #[tokio::test]
    async fn import_selected_skips_an_unknown_category_without_inventing_work() {
        let tmp = TempDir::new().unwrap();
        let s = SettingsSyncService::with_home_dir(tmp.path().to_path_buf());
        let req = json!({
            "selections": [{"agent":"claudeCode","category":"unknown"}]
        });
        let r = s
            .call(
                &ctx(),
                "importSelected",
                vec![WireValue::Json(req)],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = r else {
            panic!("json")
        };
        assert_eq!(v["successCount"], 0);
        assert_eq!(v["skippedCount"], 1);
        assert_eq!(v["failedCount"], 0);
        assert_eq!(v["taskResults"][0]["status"], "skipped");
        assert_eq!(v["taskResults"][0]["agent"], "claudeCode");
    }

    #[tokio::test]
    async fn import_selected_copies_a_skill_then_reports_target_exists() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let skill_dir = home.join(".claude").join("skills").join("my-skill");
        fs::create_dir_all(&skill_dir).await.unwrap();
        fs::write(skill_dir.join("SKILL.md"), "---\nname: my-skill\n---\nhi")
            .await
            .unwrap();
        fs::write(skill_dir.join("notes.md"), "extra file")
            .await
            .unwrap();

        let s = SettingsSyncService::with_home_dir(home.to_path_buf());
        let req = json!({
            "selections": [{"agent":"claudeCode","category":"skills"}]
        });
        let WireValue::Json(v) = s
            .call(
                &ctx(),
                "importSelected",
                vec![WireValue::Json(req.clone())],
                CancelToken::new(),
            )
            .await
            .unwrap()
        else {
            panic!("json")
        };
        assert_eq!(v["successCount"], 1, "{v}");
        assert_eq!(v["skippedCount"], 0, "{v}");
        let target = home.join(".zcode").join("skills").join("my-skill");
        assert!(target.join("SKILL.md").is_file(), "skill copied");
        assert!(target.join("notes.md").is_file(), "directory tree copied");
        assert_eq!(v["taskResults"][0]["skillResults"][0]["status"], "imported");

        // Second import: the destination exists, so it must be skipped â€” not overwritten.
        let WireValue::Json(v2) = s
            .call(
                &ctx(),
                "importSelected",
                vec![WireValue::Json(req)],
                CancelToken::new(),
            )
            .await
            .unwrap()
        else {
            panic!("json")
        };
        assert_eq!(v2["successCount"], 0, "{v2}");
        assert_eq!(v2["skippedCount"], 1, "{v2}");
        assert_eq!(
            v2["taskResults"][0]["skillResults"][0]["skipReason"],
            "targetExists"
        );
    }

    #[tokio::test]
    async fn import_selected_copies_a_command_file() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let cmd_dir = home.join(".claude").join("commands");
        fs::create_dir_all(&cmd_dir).await.unwrap();
        fs::write(cmd_dir.join("review.md"), "review code")
            .await
            .unwrap();

        let s = SettingsSyncService::with_home_dir(home.to_path_buf());
        let req = json!({
            "selections": [{"agent":"claudeCode","category":"commands"}]
        });
        let WireValue::Json(v) = s
            .call(
                &ctx(),
                "importSelected",
                vec![WireValue::Json(req)],
                CancelToken::new(),
            )
            .await
            .unwrap()
        else {
            panic!("json")
        };
        assert_eq!(v["successCount"], 1, "{v}");
        let target = home.join(".zcode").join("commands").join("review.md");
        assert_eq!(
            fs::read_to_string(&target).await.unwrap(),
            "review code",
            "command body copied verbatim"
        );
    }

    #[tokio::test]
    async fn import_selected_merges_mcp_servers_preserving_other_config_keys() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let claude_dir = home.join(".claude");
        fs::create_dir_all(&claude_dir).await.unwrap();
        fs::write(
            claude_dir.join("settings.json"),
            r#"{"mcpServers":{"filesystem":{"command":"npx","args":["-y","fs"]}}}"#,
        )
        .await
        .unwrap();
        // Pre-existing zcode config with an unrelated key must survive the merge.
        let config_dir = home.join(".zcode").join("cli");
        fs::create_dir_all(&config_dir).await.unwrap();
        fs::write(
            config_dir.join("config.json"),
            r#"{"theme":"dark","mcp":{"servers":{"existing":{"command":"x"}}}}"#,
        )
        .await
        .unwrap();

        let s = SettingsSyncService::with_home_dir(home.to_path_buf());
        let req = json!({
            "selections": [{"agent":"claudeCode","category":"mcpServers"}]
        });
        let WireValue::Json(v) = s
            .call(
                &ctx(),
                "importSelected",
                vec![WireValue::Json(req)],
                CancelToken::new(),
            )
            .await
            .unwrap()
        else {
            panic!("json")
        };
        assert_eq!(v["successCount"], 1, "{v}");
        let text = fs::read_to_string(config_dir.join("config.json"))
            .await
            .unwrap();
        let config: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(config["theme"], "dark", "unrelated key preserved");
        assert_eq!(config["mcp"]["servers"]["existing"]["command"], "x");
        assert_eq!(
            config["mcp"]["servers"]["filesystem"]["command"], "npx",
            "imported server merged: {config}"
        );
    }

    #[tokio::test]
    async fn import_selected_copies_a_plugin_and_registers_its_directory() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let plugin_dir = home.join(".claude").join("plugins").join("my-plugin");
        fs::create_dir_all(&plugin_dir).await.unwrap();
        fs::write(plugin_dir.join("plugin.json"), r#"{"name":"my-plugin"}"#)
            .await
            .unwrap();

        let s = SettingsSyncService::with_home_dir(home.to_path_buf());
        let req = json!({
            "selections": [{"agent":"claudeCode","category":"plugins"}]
        });
        let WireValue::Json(v) = s
            .call(
                &ctx(),
                "importSelected",
                vec![WireValue::Json(req)],
                CancelToken::new(),
            )
            .await
            .unwrap()
        else {
            panic!("json")
        };
        assert_eq!(v["successCount"], 1, "{v}");
        let target = home.join(".zcode").join("plugins").join("my-plugin");
        assert!(target.join("plugin.json").is_file());
        let config_text = fs::read_to_string(home.join(".zcode").join("cli").join("config.json"))
            .await
            .unwrap();
        let config: Value = serde_json::from_str(&config_text).unwrap();
        let dirs = config["plugins"]["dirs"].as_array().expect("dirs array");
        assert_eq!(dirs.len(), 1, "{config}");
        assert!(dirs[0].as_str().unwrap().ends_with("my-plugin"), "{config}");
    }

    #[tokio::test]
    async fn import_selected_filters_by_selected_source_paths() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let skills_root = home.join(".claude").join("skills");
        for name in ["one", "two"] {
            let dir = skills_root.join(name);
            fs::create_dir_all(&dir).await.unwrap();
            fs::write(dir.join("SKILL.md"), format!("name: {name}"))
                .await
                .unwrap();
        }
        let selected = skills_root.join("one").join("SKILL.md");

        let s = SettingsSyncService::with_home_dir(home.to_path_buf());
        let req = json!({
            "selections": [{
                "agent":"claudeCode",
                "category":"skills",
                "skillPaths": [selected.to_string_lossy()],
            }]
        });
        let WireValue::Json(v) = s
            .call(
                &ctx(),
                "importSelected",
                vec![WireValue::Json(req)],
                CancelToken::new(),
            )
            .await
            .unwrap()
        else {
            panic!("json")
        };
        assert_eq!(v["successCount"], 1, "{v}");
        assert!(home.join(".zcode").join("skills").join("one").is_dir());
        assert!(
            !home.join(".zcode").join("skills").join("two").exists(),
            "unselected skill must not be imported"
        );
    }

    #[tokio::test]
    async fn first_run_prompt_state_persists() {
        let tmp = TempDir::new().unwrap();
        let s = SettingsSyncService::with_home_dir(tmp.path().to_path_buf());
        let r = s
            .call(&ctx(), "getFirstRunPromptState", vec![], CancelToken::new())
            .await
            .unwrap();
        let WireValue::Json(v) = r else {
            panic!("json")
        };
        assert_eq!(v["handled"], false);
        s.call(
            &ctx(),
            "markFirstRunPromptHandled",
            vec![],
            CancelToken::new(),
        )
        .await
        .unwrap();
        let r2 = s
            .call(&ctx(), "getFirstRunPromptState", vec![], CancelToken::new())
            .await
            .unwrap();
        let WireValue::Json(v2) = r2 else {
            panic!("json")
        };
        assert_eq!(v2["handled"], true);
    }

    #[tokio::test]
    async fn unknown_method_fails() {
        let tmp = TempDir::new().unwrap();
        let s = SettingsSyncService::with_home_dir(tmp.path().to_path_buf());
        assert!(s
            .call(&ctx(), "nope", vec![], CancelToken::new())
            .await
            .is_err());
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use nc_code_channels::ClientMode;

    #[tokio::test]
    async fn default_constructor_works() {
        let s = SettingsSyncService::new();
        let ctx = ChannelContext::new("settings-sync", ClientMode::DesktopContinuous);
        let r = s
            .call(
                &ctx,
                "detect",
                vec![WireValue::Json(
                    serde_json::json!({"categories":["commands"]}),
                )],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = r else {
            panic!("json")
        };
        assert!(v["agents"].is_array());
    }
}
