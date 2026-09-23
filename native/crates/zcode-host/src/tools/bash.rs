//! Bash：shell 命令执行（语义对齐 TS bash 工具：Windows `cmd /D /C`，Unix `sh -c`）。
//!
//! 安全模型（与 TS 同类，不是更弱）：单次调用按 argv 前缀做**保守只读分类**，
//! 只读命令（echo/ls/cat/git status…）放行；其余一律视为有副作用，headless 无交互
//! 客户端可问 → 默认拒绝，仅 `--full-access` 自动允许（PermissionBroker deny 缺省
//! 的移植）。炮弹（pipe/&&/重定向）交给 shell 语义（与 TS 一致），分类只认
//! **首词 + 静态 argv**：任何 `$`、反引号、`;`、`|`、`>`、`<` 都判非只读。
//! 超时杀整棵树（`zcode-proc::kill_tree`，S1 crate in-process 复用）。

use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{
    optional_string, optional_u64, require_string, truncate_to_limit, Tool, ToolContext,
    ToolContract, ToolError, ToolErrorCode,
};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const DEFAULT_OUTPUT_CAP: usize = 30_000;

/// 保守只读命令表：首词命中且 argv 全静态才判只读。
const READONLY_COMMANDS: [&str; 22] = [
    "cat", "echo", "pwd", "ls", "dir", "head", "tail", "wc", "which", "where", "env", "printenv",
    "date", "whoami", "hostname", "uname", "find", "grep", "rg", "git", "cargo", "rustc",
];

/// git 的只读子命令（其余 git 子命令一律非只读）。
const READONLY_GIT_SUBCOMMANDS: [&str; 12] = [
    "status",
    "diff",
    "log",
    "show",
    "branch",
    "remote",
    "rev-parse",
    "describe",
    "blame",
    "shortlog",
    "config",
    "ls-files",
];

/// cargo/rustc 的只读子命令。
const READONLY_CARGO_SUBCOMMANDS: [&str; 4] = ["--version", "tree", "metadata", "fmt"];

/// argv 中任何这些字符都取消只读判定（保守：宁可多问/多拒）。
/// `~` 只在 token 首位取消（tilde 展开只发生在词首；`HEAD~1` 这类 git ref 安全）。
fn argv_is_static(token: &str) -> bool {
    if token.starts_with('~') {
        return false;
    }
    !token.contains([
        '$', '`', ';', '|', '>', '<', '&', '\n', '(', ')', '{', '}', '*', '?', '[', ']', '!',
    ])
}

/// 分词：处理引号，不做求值。引号内字符原样保留（shell 会再解一次，这里只用于分类）。
fn tokenize(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut has_content = false;
    for ch in command.chars() {
        match quote {
            Some(active) => {
                if ch == active {
                    quote = None;
                } else {
                    current.push(ch);
                }
            }
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    has_content = true;
                }
                c if c.is_whitespace() => {
                    if has_content || !current.is_empty() {
                        tokens.push(std::mem::take(&mut current));
                        has_content = false;
                    }
                }
                c => current.push(c),
            },
        }
    }
    if has_content || !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

pub struct BashTool;

impl BashTool {
    fn classify_readonly(command: &str) -> bool {
        let tokens = tokenize(command);
        let Some(head) = tokens.first() else {
            return false;
        };
        let head = head.rsplit(['/', '\\']).next().unwrap_or(head);
        if !READONLY_COMMANDS.contains(&head) {
            return false;
        }
        if !tokens.iter().skip(1).all(|token| argv_is_static(token)) {
            return false;
        }
        match head {
            "git" => tokens
                .get(1)
                .is_some_and(|sub| READONLY_GIT_SUBCOMMANDS.contains(&sub.as_str())),
            "cargo" | "rustc" => tokens
                .get(1)
                .is_some_and(|sub| READONLY_CARGO_SUBCOMMANDS.contains(&sub.as_str())),
            _ => true,
        }
    }
}

impl Tool for BashTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "Bash".to_string(),
            description: "Run a shell command in the workspace (cmd on Windows, sh on Unix). Read-only commands run without --full-access; everything else is denied without it. Default timeout 120s, output capped at 30k chars.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": string_schema_ref(),
                    "timeout_ms": {"type": "integer", "description": "timeout in ms (max 600000)"},
                    "description": string_schema_ref(),
                },
                "required": ["command"],
            }),
            read_only: false,
            destructive: false,
            concurrent_safe: false,
            side_effect_scope: super::SCOPE_SYSTEM,
            timeout_ms: MAX_TIMEOUT_MS,
            max_output_bytes: 64 * 1024,
        }
    }

    fn is_read_only_invocation(&self, input: &Value) -> bool {
        input
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(Self::classify_readonly)
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        if ctx.is_cancelled() {
            return Err(ToolError::new(ToolErrorCode::Cancelled, "turn cancelled"));
        }
        let command = require_string(input, "command")?;
        let timeout_ms = optional_u64(input, "timeout_ms")
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);
        let _description = optional_string(input, "description");

        let child = {
            #[cfg(windows)]
            {
                Command::new("cmd")
                    .args(["/D", "/C", command])
                    .current_dir(&ctx.workspace_root)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .stdin(Stdio::null())
                    .spawn()
            }
            #[cfg(not(windows))]
            {
                Command::new("sh")
                    .args(["-c", command])
                    .current_dir(&ctx.workspace_root)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .stdin(Stdio::null())
                    .spawn()
            }
        }
        .map_err(|error| {
            ToolError::new(ToolErrorCode::ExecutionFailed, format!("spawn: {error}"))
        })?;

        let pid = child.id();
        // 超时看守：独立线程等待，到点杀整棵树。
        let (done_tx, done_rx) = mpsc::channel::<()>();
        thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            loop {
                if done_rx.try_recv().is_ok() {
                    return;
                }
                if Instant::now() >= deadline {
                    let _ = zcode_proc::kill_tree(pid, 2_000);
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
        });

        let output = child.wait_with_output();
        let _ = done_tx.send(());
        let output = match output {
            Ok(output) => output,
            Err(error) => {
                return Err(ToolError::new(
                    ToolErrorCode::ExecutionFailed,
                    format!("wait: {error}"),
                ))
            }
        };
        let _ = zcode_proc::kill_tree(pid, 2_000); // 兜底：清理由 shell 派生的孙进程

        let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
        if !output.stderr.is_empty() {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        match output.status.code() {
            Some(code) => combined.push_str(&format!("\nexit code: {code}")),
            None => combined.push_str("\nexit code: signal"),
        }
        let (combined, _) = truncate_to_limit(combined, DEFAULT_OUTPUT_CAP);
        Ok(combined)
    }
}

fn string_schema_ref() -> serde_json::Value {
    super::string_schema("command")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    fn test_ctx(root: &Path) -> ToolContext {
        ToolContext {
            workspace_root: root.to_path_buf(),
            full_access: true,
            trace_id: "t".to_string(),
            turn_id: "turn".to_string(),
            read_state: Arc::new(Mutex::new(std::collections::HashSet::new())),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn temp_workspace(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zcode-host-bash-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("workspace");
        dir
    }

    #[test]
    fn readonly_classification_is_conservative() {
        assert!(BashTool::classify_readonly("echo hello"));
        assert!(BashTool::classify_readonly("git status --short"));
        assert!(BashTool::classify_readonly("git diff HEAD~1"));
        assert!(BashTool::classify_readonly("cargo --version"));
        assert!(
            !BashTool::classify_readonly("git commit -m x"),
            "git commit is a write"
        );
        assert!(
            !BashTool::classify_readonly("rm -rf build"),
            "rm is never readonly"
        );
        assert!(
            !BashTool::classify_readonly("echo $(whoami)"),
            "substitution cancels readonly"
        );
        assert!(
            !BashTool::classify_readonly("git status; rm -rf x"),
            "compound cancels"
        );
        assert!(
            !BashTool::classify_readonly("echo hi | tee f"),
            "pipe cancels"
        );
        assert!(
            !BashTool::classify_readonly("curl https://x"),
            "network is not readonly"
        );
    }

    #[test]
    fn tokenizer_handles_quotes_and_flags() {
        assert_eq!(
            tokenize("git commit -m 'a b'"),
            vec!["git", "commit", "-m", "a b"]
        );
        assert_eq!(tokenize("echo \"x y\" z"), vec!["echo", "x y", "z"]);
    }

    #[test]
    fn executes_real_command_with_stdout_and_exit_code() {
        let root = temp_workspace("exec");
        let ctx = test_ctx(&root);
        let out = BashTool
            .execute(&json!({"command": "echo hello-native"}), &ctx)
            .expect("echo");
        assert!(out.contains("hello-native"));
        assert!(out.contains("exit code: 0"));

        let failing = BashTool
            .execute(&json!({"command": "exit 3"}), &ctx)
            .expect("failing command still returns output");
        assert!(failing.contains("exit code: 3"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cwd_is_workspace_root() {
        let root = temp_workspace("cwd");
        let ctx = test_ctx(&root);
        fs::write(root.join("marker.txt"), "x").expect("write");
        let out = BashTool
            .execute(&json!({"command": "cat marker.txt"}), &ctx)
            .expect("cat in workspace");
        assert!(out.contains('x'));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn timeout_kills_the_command() {
        let root = temp_workspace("timeout");
        let ctx = test_ctx(&root);
        let started = Instant::now();
        let out = BashTool
            .execute(&json!({"command": "sleep 5", "timeout_ms": 300}), &ctx)
            .expect("timeout returns");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "must not wait for the full sleep"
        );
        assert!(out.contains("exit code"));
        let _ = fs::remove_dir_all(&root);
    }
}
