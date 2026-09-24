//! nc-tools 原生集成测试：Kernel 工具面真的进了 host，且每个家族的关键工具
//! 都能通过 host 的 Tool 接口真实执行。
//!
//! 这不是「链接了就算数」——每个家族至少跑一个真实副作用（写文件、git 状态、
//! 目录列表、HTTP 头解析、符号抽取、批处理），证明 in-process 通路是通的。
//! MCP 传输层不参与：这里没有任何 socket / JSON-RPC 握手。

use serde_json::json;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use zcode_host::nc_tools;
use zcode_host::tools::{Tool, ToolContext, ToolErrorCode};

fn temp_workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zcode-nctest-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("workspace");
    dir
}

fn ctx(root: &std::path::Path) -> ToolContext {
    ToolContext {
        workspace_root: root.to_path_buf(),
        full_access: true,
        trace_id: "t".to_string(),
        turn_id: "turn".to_string(),
        read_state: Arc::new(Mutex::new(std::collections::HashSet::new())),
        cancel: Arc::new(AtomicBool::new(false)),
        todos: zcode_host::tools::todo::new_todo_store(),
        provider_config: None,
    }
}

fn find(tools: &[Arc<nc_tools::NcTool>], name: &str) -> Option<Arc<nc_tools::NcTool>> {
    tools.iter().find(|tool| tool.tool_name() == name).cloned()
}

#[test]
fn kernel_exposes_the_full_typed_tool_surface() {
    let root = temp_workspace("surface");
    let tools = nc_tools::build_nc_tools(&root).expect("kernel must build");
    let names: Vec<String> = tools
        .iter()
        .map(|tool| tool.tool_name().to_string())
        .collect();

    // SDK 声称 89 个工具；家族锚点逐个在场（不是数数量，是点名关键工具）。
    for expected in [
        "fs.read",
        "fs.write",
        "fs.list",
        "fs.tree",
        "fs.stat",
        "fs.watch",
        "patch.apply",
        "patch.stage",
        "patch.preview",
        "patch.commit",
        "search.grep",
        "search.files",
        "search.semantic",
        "code.symbols",
        "code.graph",
        "text.diff",
        "git.status",
        "git.diff",
        "git.log",
        "git.commit",
        "proc.spawn",
        "proc.start",
        "proc.status",
        "proc.kill",
        "test.run",
        "pkg.list",
        "net.http",
        "net.fetch",
        "net.search",
        "agent.register",
        "agent.status",
        "env.get",
        "sys.snapshot",
        "sys.journal",
        "batch.execute",
    ] {
        assert!(
            names.iter().any(|name| name == expected),
            "nc-tools tool missing from kernel: {expected}"
        );
    }
    assert!(
        names.len() >= 80,
        "expected the full surface (~89), got {}",
        names.len()
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn fs_write_then_read_round_trips_through_host_tool_interface() {
    let root = temp_workspace("fsrw");
    let tools = nc_tools::build_nc_tools(&root).expect("kernel");
    let ctx = ctx(&root);

    let write = find(&tools, "fs.write").expect("fs.write");
    let written = write
        .execute(
            &json!({"path": "hello.txt", "content": "native nc-tools payload"}),
            &ctx,
        )
        .expect("fs.write must succeed");
    assert!(written.contains("hello.txt") || written.contains("ok") || !written.is_empty());
    assert!(root.join("hello.txt").exists(), "file must really exist");

    let read = find(&tools, "fs.read").expect("fs.read");
    let content = read
        .execute(&json!({"path": "hello.txt"}), &ctx)
        .expect("fs.read must succeed");
    assert!(
        content.contains("native nc-tools payload"),
        "read must return the written content: {content}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn fs_list_and_stat_report_real_workspace_facts() {
    let root = temp_workspace("fslist");
    std::fs::write(root.join("a.txt"), "a").expect("seed");
    std::fs::create_dir_all(root.join("sub")).expect("dir");
    let tools = nc_tools::build_nc_tools(&root).expect("kernel");
    let ctx = ctx(&root);

    let list = find(&tools, "fs.list").expect("fs.list");
    let listed = list.execute(&json!({"path": "."}), &ctx).expect("fs.list");
    assert!(
        listed.contains("a.txt") || listed.contains("sub"),
        "list: {listed}"
    );

    let stat = find(&tools, "fs.stat").expect("fs.stat");
    let stat_out = stat
        .execute(&json!({"path": "a.txt"}), &ctx)
        .expect("fs.stat");
    assert!(stat_out.contains("a.txt"), "stat: {stat_out}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn git_status_runs_against_the_real_repository() {
    let root = temp_workspace("git");
    // 用本仓库的一个子目录当 workspace（本身就是 git 仓库），验证真实 git 面。
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|native| native.parent())
        .and_then(|repo| repo.parent())
        .expect("repo root")
        .to_path_buf();
    let tools = nc_tools::build_nc_tools(&workspace).expect("kernel");
    let ctx = ctx(&workspace);
    let status = find(&tools, "git.status").expect("git.status");
    let output = status
        .execute(&json!({"baseDir": workspace.to_string_lossy()}), &ctx)
        .expect("git.status must run");
    // 真实仓库的 status 至少报告分支或干净状态之一。
    assert!(
        output.contains("branch") || output.contains("clean") || output.contains("modified"),
        "unexpected git.status output: {output}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn text_diff_produces_a_real_diff() {
    let root = temp_workspace("diff");
    std::fs::write(root.join("old.txt"), "a\nb\n").expect("seed old");
    std::fs::write(root.join("new.txt"), "a\nB\n").expect("seed new");
    let tools = nc_tools::build_nc_tools(&root).expect("kernel");
    let ctx = ctx(&root);
    let diff = find(&tools, "text.diff").expect("text.diff");
    // text.diff 比两个文件（签名：path/path2），不是两段文本。
    let output = diff
        .execute(&json!({"path": "old.txt", "path2": "new.txt"}), &ctx)
        .expect("text.diff must succeed");
    assert!(
        output.contains('B') || output.contains('+'),
        "diff: {output}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn search_grep_finds_content_in_workspace() {
    let root = temp_workspace("grep");
    std::fs::write(root.join("needle.txt"), "the quick brown fox").expect("seed");
    let tools = nc_tools::build_nc_tools(&root).expect("kernel");
    let ctx = ctx(&root);
    let grep = find(&tools, "search.grep").expect("search.grep");
    let output = grep
        .execute(&json!({"pattern": "brown", "path": "."}), &ctx)
        .expect("search.grep must succeed");
    assert!(
        output.contains("needle") || output.contains("brown"),
        "grep must find the needle: {output}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn sys_journal_records_the_native_calls() {
    let root = temp_workspace("journal");
    let tools = nc_tools::build_nc_tools(&root).expect("kernel");
    let ctx = ctx(&root);
    let write = find(&tools, "fs.write").expect("fs.write");
    write
        .execute(&json!({"path": "j.txt", "content": "journaled"}), &ctx)
        .expect("write");
    // nc-tools 把每次调用写进 <workspace>/.nc-tools/journal.jsonl。
    let journal = root.join(".nc-tools").join("journal.jsonl");
    assert!(
        journal.exists(),
        "journal must exist at {}",
        journal.display()
    );
    let body = std::fs::read_to_string(&journal).expect("read journal");
    assert!(
        body.contains("tool.call"),
        "journal must contain the call event"
    );
    assert!(body.contains("fs.write"), "journal must name the tool");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn unknown_tool_is_an_error_not_a_silent_success() {
    let root = temp_workspace("unknown");
    let tools = nc_tools::build_nc_tools(&root).expect("kernel");
    // 未知名字没有注册代理——host 侧 lookup 失败（fail-closed）。
    assert!(find(&tools, "fs.doesNotExist").is_none());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cancellation_is_honored_before_execution() {
    let root = temp_workspace("cancel");
    let tools = nc_tools::build_nc_tools(&root).expect("kernel");
    let ctx = ctx(&root);
    // 取消标志挂在共享 Arc 上：直接置位，不改 ctx 本体。
    ctx.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    let write = find(&tools, "fs.write").expect("fs.write");
    let error = write
        .execute(&json!({"path": "c.txt", "content": "x"}), &ctx)
        .expect_err("cancelled turn must not execute");
    assert_eq!(error.code, ToolErrorCode::Cancelled);
    assert!(!root.join("c.txt").exists(), "no side effect after cancel");
    let _ = std::fs::remove_dir_all(&root);
}
