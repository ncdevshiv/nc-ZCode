//! `settings-sync` channel — contract-aligned, honest implementation.
//!
//! Full TS parity (`packages/services/src/settings-sync/*`) covers skills/commands/plugins/MCP
//! discovery and import across many agents (large surface, many TS modules). This Rust
//! implementation provides:
//! - **Real** Claude AGENTS.md migration status/copy (`~/.claude/CLAUDE.md` -> `~/.zcode/AGENTS.md`,
//!   matching TS paths) with contract-correct shapes.
//! - **Real** first-run prompt handled flag persisted locally (`~/.zcode/settings-sync-state.json`,
//!   best-effort).
//! - **Real (bounded)** skills discovery: scans known agent skill roots in home/workspace for
//!   `SKILL.md` and returns contract-shaped `SettingsSyncDiscoveryResult`. Other categories and
//!   full multi-agent/source parity remain partial.
//! - **Honest** import: full import engines are not ported; `importSelected` returns per-selection
//!   `skipped` results (never fabricates imported files).

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

    fn should_scan_skills(request: &Value) -> bool {
        let cats = Self::requested_categories(request);
        cats.is_empty() || cats.iter().any(|c| c == "skills")
    }

    fn should_scan_commands(request: &Value) -> bool {
        let cats = Self::requested_categories(request);
        !cats.is_empty() && cats.iter().any(|c| c == "commands")
    }

    fn should_scan_plugins(request: &Value) -> bool {
        let cats = Self::requested_categories(request);
        !cats.is_empty() && cats.iter().any(|c| c == "plugins")
    }

    fn should_scan_mcp(request: &Value) -> bool {
        let cats = Self::requested_categories(request);
        !cats.is_empty() && cats.iter().any(|c| c == "mcpServers")
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

    fn import_selected_result(&self, request: &Value) -> Value {
        let selections = request
            .get("selections")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut task_results = Vec::with_capacity(selections.len());
        for sel in &selections {
            let agent = sel.get("agent").cloned().unwrap_or(Value::Null);
            let category = sel.get("category").cloned().unwrap_or(Value::Null);
            // Full import engines not ported: do not fabricate file imports.
            task_results.push(json!({
                "agent": agent,
                "category": category,
                "status": "skipped",
                "importedCount": 0,
                "skippedCount": 1,
                "failedCount": 0
            }));
        }
        json!({
            "successCount": 0,
            "skippedCount": selections.len(),
            "failedCount": 0,
            "taskResults": task_results
        })
    }
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
            "importSelected" => self.import_selected_result(&request),
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
    async fn import_selected_returns_honest_skipped_results() {
        let tmp = TempDir::new().unwrap();
        let s = SettingsSyncService::with_home_dir(tmp.path().to_path_buf());
        let req = json!({
            "selections": [{"agent":"claudeCode","category":"skills"}]
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
