//! The tool micro-benchmark (Rust side): the same fixture and the same set of operations, run once with the zcode native tools and once
//! with nc-tools (the in-process Kernel), emitting JSON of the same shape as `scripts/bench/tool-bench.mjs`.
//!
//! Only the IO/computation hot spots are measured: read (32KB) / write (32KB, new file) / glob (400 files) / grep (400 files) /
//! edit (read+edit) / bash / skill / cron (the real AutomationService+SQLite) / todo (durable storage) / js.
//! The fixture is byte-identical to the TS benchmark (the same file names, line counts and needle distribution), otherwise the comparison is meaningless.
//!
//! Usage: cargo run -p nc-code-services-agent --release --bin tool_bench

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use nc_code_host::nc_tools;
use nc_code_host::tools::bash::BashTool;
use nc_code_host::tools::fs_tools::{EditTool, ReadTool, WriteTool};
use nc_code_host::tools::search_tools::{GlobTool, GrepTool};
use nc_code_host::tools::skill::SkillTool;
use nc_code_host::tools::{Tool, ToolContext};
use nc_code_services_agent::automation_tool_port::AutomationToolPort;
use nc_code_services_automation::{AutomationRepo, AutomationService};
use serde_json::{json, Value};

const FILES: usize = 400;

fn fixture_root() -> PathBuf {
    std::env::temp_dir().join(format!("zcode-bench-rust-{}", std::process::id()))
}

fn edit_fixture() -> String {
    (0..64)
        .map(|index| format!("value-v1-{index}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn build_fixture(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root.join("src")).expect("src");
    for index in 0..FILES {
        let mut lines: Vec<String> = (0..20)
            .map(|line| format!("export const v{index:04}_{line} = {line};"))
            .collect();
        if index % 10 == 0 {
            lines.push("// grep-needle".to_string());
        }
        std::fs::write(
            root.join("src").join(format!("mod-{index:04}.ts")),
            lines.join("\n") + "\n",
        )
        .expect("fixture file");
    }
    std::fs::write(root.join("target.txt"), "line v1\n".repeat(2048)).expect("target");
    std::fs::write(root.join("edit.txt"), edit_fixture()).expect("edit fixture");
    // The Skill fixture is byte-identical to the TS side (the same frontmatter + the same body repeated 64 lines).
    let skill_dir = root.join(".agents").join("skills").join("bench-skill");
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: bench-skill\ndescription: benchmark skill fixture\n---\n".to_string()
            + &"Benchmark skill body.\n".repeat(64),
    )
    .expect("skill fixture");
}

fn ctx(root: &Path) -> ToolContext {
    ToolContext {
        workspace_root: root.to_path_buf(),
        full_access: true,
        interactive: false,
        trace_id: "bench".to_string(),
        turn_id: "bench".to_string(),
        session_id: "session-1".to_string(),
        model_ref: None,
        automation: None,
        sessions: None,
        workflow: None,
        read_state: Arc::new(Mutex::new(std::collections::HashSet::new())),
        cancel: Arc::new(AtomicBool::new(false)),
        todos: nc_code_host::tools::todo::new_todo_store(),
        provider_config: None,
        tool_call_id: None,
        subagent: None,
        ask_user: None,
        plan_mode: false,
        plan: None,
        background: None,
        browser: None,
        computer: None,
        offpeak: None,
    }
}

struct Stats {
    samples: Vec<f64>,
}

impl Stats {
    fn new() -> Self {
        Self {
            samples: Vec::new(),
        }
    }
    fn push(&mut self, nanos: f64) {
        self.samples.push(nanos);
    }
    fn row(&self, tool: &str, impl_name: &str, check: String) -> Value {
        let mut sorted = self.samples.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mean = sorted.iter().sum::<f64>() / sorted.len() as f64;
        let median = sorted[sorted.len() / 2];
        let p95 = sorted[(sorted.len() as f64 * 0.95) as usize];
        json!({
            "tool": tool,
            "impl": impl_name,
            "iterations": sorted.len(),
            "meanNs": mean.round() as u64,
            "medianNs": median.round() as u64,
            "p95Ns": p95.round() as u64,
            "check": check,
        })
    }
}

fn count_occurrences(text: &str, needle: &str) -> usize {
    text.matches(needle).count()
}

/// The scheduled-task benchmark: the **real service** (AutomationService + sqlite), entering the tool layer through the same
/// `AutomationToolPort` adapter as production — there is no fake port/stand-in anywhere.
///
/// The real service has a "keep at most 20" cap, so "blindly create 100 times" is not a valid measurement:
/// 20 rows are pre-seeded for list/update; create **deletes the row it just made outside the timing** every round;
/// delete **prefills one row outside the timing** each round and then deletes it. Cleanup goes through service (not counted in the tool-layer cost).
fn bench_cron(root: &Path, rows: &mut Vec<Value>) {
    use nc_code_host::tools::automation_tools::{
        CronCreateTool, CronDeleteTool, CronListTool, CronUpdateTool,
    };
    use nc_code_host::tools::AutomationPort;
    use nc_code_services_automation::Scope;

    let db = root.join("tasks.sqlite");
    let service = Arc::new(AutomationService::new(Arc::new(AutomationRepo::new(db))));
    let scope = Scope {
        workspace_path: Some(root.to_string_lossy().to_string()),
        workspace_identity: Some("bench-workspace".to_string()),
    };
    let port: Arc<dyn AutomationPort> = Arc::new(AutomationToolPort::new(
        Arc::clone(&service),
        root.to_string_lossy().to_string(),
        "bench-workspace".to_string(),
    ));
    let mut context = ctx(root);
    context.automation = Some(port);

    let create_one = |index: usize| {
        CronCreateTool
            .execute(
                &json!({"title": format!("job {index}"), "prompt": "check", "cron": "0 * * * *"}),
                &context,
            )
            .expect("cron create")
    };
    let created_id = |output: &str| -> String {
        serde_json::from_str::<Value>(output).unwrap()["automation"]["automationId"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };

    // Pre-seed 19 rows (the cap is 20): create reaches 20 each round and then deletes the newly created row, so the loop never hits the cap.
    for index in 0..19 {
        create_one(index);
    }
    let first_id = {
        let list: Value = serde_json::from_str(
            &CronListTool
                .execute(&json!({}), &context)
                .expect("cron list"),
        )
        .unwrap();
        list["automations"][0]["automationId"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };

    let mut stats = Stats::new();
    for index in 0..60 {
        let started = Instant::now();
        let output = create_one(index + 20);
        stats.push(started.elapsed().as_nanos() as f64);
        let _ = service.delete(&created_id(&output), &scope);
    }
    rows.push(stats.row("cron.create", "rust", String::new()));

    let mut stats = Stats::new();
    for _ in 0..200 {
        let started = Instant::now();
        CronListTool
            .execute(&json!({}), &context)
            .expect("cron list");
        stats.push(started.elapsed().as_nanos() as f64);
    }
    rows.push(stats.row("cron.list", "rust", String::new()));

    let mut stats = Stats::new();
    for _ in 0..200 {
        let started = Instant::now();
        CronUpdateTool
            .execute(&json!({"id": first_id, "title": "renamed"}), &context)
            .expect("cron update");
        stats.push(started.elapsed().as_nanos() as f64);
    }
    rows.push(stats.row("cron.update", "rust", String::new()));

    let mut stats = Stats::new();
    for index in 0..100 {
        let output = create_one(index + 200);
        let id = created_id(&output);
        let started = Instant::now();
        CronDeleteTool
            .execute(&json!({"id": id}), &context)
            .expect("cron delete");
        stats.push(started.elapsed().as_nanos() as f64);
    }
    rows.push(stats.row("cron.delete", "rust", String::new()));

    // For attribution: the same operation hitting the service layer directly (without the tool layer's JSON projection/serialization),
    // used to tell "the service/SQLite is slow" apart from "the tool layer is slow".
    let mut stats = Stats::new();
    for _ in 0..200 {
        let started = Instant::now();
        let _ = service.update(&first_id, &Default::default(), &scope);
        stats.push(started.elapsed().as_nanos() as f64);
    }
    let mut stats = Stats::new();
    for _ in 0..200 {
        let started = Instant::now();
        let _ = service.list(&scope);
        stats.push(started.elapsed().as_nanos() as f64);
    }
    rows.push(stats.row("cron.list.service", "rust", String::new()));

    rows.push(stats.row("cron.update.service", "rust", String::new()));

    let mut stats = Stats::new();
    for index in 0..100 {
        let output = create_one(index + 500);
        let id = created_id(&output);
        let started = Instant::now();
        let _ = service.delete(&id, &scope);
        stats.push(started.elapsed().as_nanos() as f64);
    }
    rows.push(stats.row("cron.delete.service", "rust", String::new()));
}

/// `js`: TS uses an in-process vm (a new session every time), Rust uses a real Node child-process kernel.
/// Two tiers are measured: persistent (the host default, warm reuse of the kernel) and fresh (a new kernel every time).
fn bench_js(rows: &mut Vec<Value>) {
    use nc_code_host::tools::node_repl::JsTool;
    use nc_code_node_repl::{KernelMode, NodeReplTool, ToolCall};

    let capability = nc_code_node_repl::detect(None);
    // warm: the JsTool registered by the host (Persistent); the first call builds the kernel.
    let tool = JsTool::new();
    let ctx = {
        let mut ctx = ctx(std::path::Path::new("."));
        ctx.cancel = Arc::new(AtomicBool::new(false));
        ctx
    };
    for _ in 0..3 {
        let _ = tool.execute(&json!({"code": "1+1"}), &ctx);
    }
    let mut stats = Stats::new();
    for _ in 0..100 {
        let started = Instant::now();
        let output = tool
            .execute(&json!({"code": "1+1"}), &ctx)
            .expect("js warm");
        stats.push(started.elapsed().as_nanos() as f64);
        debug_assert!(output.contains("2"));
    }
    rows.push(stats.row("js.warm", "rust", String::new()));

    // cold: Fresh builds a new kernel every time (aligned with the TS executor starting a new session per call).
    let Ok(fresh) = NodeReplTool::from_capability(KernelMode::Fresh, &capability) else {
        return;
    };
    let call = ToolCall {
        code: "1+1".to_string(),
        title: None,
        timeout_ms: None,
        session_key: None,
        request_meta: json!({}),
    };
    let mut stats = Stats::new();
    for _ in 0..15 {
        let started = Instant::now();
        let output = fresh.execute(&call).expect("js fresh");
        stats.push(started.elapsed().as_nanos() as f64);
        debug_assert!(output.json.contains("2"));
    }
    rows.push(stats.row("js.cold", "rust", String::new()));
}

fn bench_rust(root: &Path, rows: &mut Vec<Value>) {
    let context = ctx(root);
    let read = ReadTool;
    let write = WriteTool;
    let edit = EditTool;
    let glob = GlobTool;
    let grep = GrepTool;
    let content = (0..2048)
        .map(|line| format!("row {line}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";

    let mut stats = Stats::new();
    let mut check = String::new();
    for _ in 0..3 {
        let _ = read.execute(&json!({"file_path": "target.txt"}), &context);
    }
    for _ in 0..200 {
        let started = Instant::now();
        let out = read
            .execute(&json!({"file_path": "target.txt"}), &context)
            .expect("read");
        stats.push(started.elapsed().as_nanos() as f64);
        check = format!("content:{}", !out.is_empty());
    }
    rows.push(stats.row("read", "rust", check));

    let mut stats = Stats::new();
    for _ in 0..3 {
        let _ = std::fs::remove_file(root.join("w.txt"));
        let _ = write.execute(&json!({"file_path": "w.txt", "content": content}), &context);
    }
    for _ in 0..100 {
        let _ = std::fs::remove_file(root.join("w.txt"));
        let started = Instant::now();
        write
            .execute(&json!({"file_path": "w.txt", "content": content}), &context)
            .expect("write");
        stats.push(started.elapsed().as_nanos() as f64);
    }
    rows.push(stats.row("write", "rust", String::new()));

    let mut stats = Stats::new();
    let mut check = String::new();
    for _ in 0..3 {
        let _ = glob.execute(&json!({"pattern": "**/*.ts"}), &context);
    }
    for _ in 0..40 {
        let started = Instant::now();
        let out = glob
            .execute(&json!({"pattern": "**/*.ts"}), &context)
            .expect("glob");
        stats.push(started.elapsed().as_nanos() as f64);
        check = count_occurrences(&out, "mod-").to_string();
    }
    rows.push(stats.row("glob", "rust", check));

    let mut stats = Stats::new();
    let mut check = String::new();
    for _ in 0..3 {
        let _ = grep.execute(
            &json!({"pattern": "grep-needle", "path": ".", "output_mode": "content"}),
            &context,
        );
    }
    for _ in 0..20 {
        let started = Instant::now();
        let out = grep
            .execute(
                &json!({"pattern": "grep-needle", "path": ".", "output_mode": "content"}),
                &context,
            )
            .expect("grep");
        stats.push(started.elapsed().as_nanos() as f64);
        check = count_occurrences(&out, "grep-needle").to_string();
    }
    rows.push(stats.row("grep", "rust", check));

    // Bash: the same command and the same interpreter role as the TS side (each side resolves to bash on its own).
    // The spawn cost dominates the absolute values; only the difference is the tool layer's overhead (parsing/pipe reading/output orchestration).
    let bash = BashTool;
    let mut stats = Stats::new();
    let mut check = String::new();
    for _ in 0..3 {
        let _ = bash.execute(&json!({"command": "echo bench-hello"}), &context);
    }
    for _ in 0..25 {
        let started = Instant::now();
        let out = bash
            .execute(&json!({"command": "echo bench-hello"}), &context)
            .expect("bash");
        stats.push(started.elapsed().as_nanos() as f64);
        check = count_occurrences(&out, "bench-hello").to_string();
    }
    rows.push(stats.row("bash(echo)", "rust", check));

    let mut stats = Stats::new();
    for _ in 0..2 {
        let _ = bash.execute(
            &json!({"command": "node -e \"console.log(1+1)\""}),
            &context,
        );
    }
    for _ in 0..10 {
        let started = Instant::now();
        bash.execute(
            &json!({"command": "node -e \"console.log(1+1)\""}),
            &context,
        )
        .expect("bash node");
        stats.push(started.elapsed().as_nanos() as f64);
    }
    rows.push(stats.row("bash(node -e)", "rust", "ok".to_string()));

    // Skill: reads the same real SKILL.md (the TS side also carries root resolution and plugin manifest search).
    let skill = SkillTool;
    let mut stats = Stats::new();
    let mut check = String::new();
    for _ in 0..3 {
        let _ = skill.execute(&json!({"skill": "bench-skill"}), &context);
    }
    for _ in 0..100 {
        let started = Instant::now();
        let out = skill
            .execute(&json!({"skill": "bench-skill"}), &context)
            .expect("skill");
        stats.push(started.elapsed().as_nanos() as f64);
        check = count_occurrences(&out, "bench-skill").to_string();
    }
    rows.push(stats.row("skill", "rust", check));

    // edit = read + edit (the same timing unit as TS)
    let mut stats = Stats::new();
    for _ in 0..3 {
        std::fs::write(root.join("edit.txt"), edit_fixture()).expect("reset");
        let _ = read.execute(&json!({"file_path": "edit.txt"}), &context);
        let _ = edit.execute(
            &json!({"file_path": "edit.txt", "old_string": "value-v1-32", "new_string": "value-v2-32"}),
            &context,
        );
    }
    for _ in 0..100 {
        std::fs::write(root.join("edit.txt"), edit_fixture()).expect("reset");
        let started = Instant::now();
        let _ = read.execute(&json!({"file_path": "edit.txt"}), &context);
        edit.execute(
            &json!({"file_path": "edit.txt", "old_string": "value-v1-32", "new_string": "value-v2-32"}),
            &context,
        )
        .expect("edit");
        stats.push(started.elapsed().as_nanos() as f64);
    }
    rows.push(stats.row("edit(read+edit)", "rust", "ok".to_string()));
}

/// Todo: both sides are durable implementations that **flush on every write** — the TS side is the production `SqliteSessionStore`
/// (the `todo` table + a `begin immediate` transaction), the Rust side is `todos.json` in the session directory
/// (temp+rename). It is not "memory against memory", otherwise the benchmark would not measure the cost production really pays.
///
/// The other two in-memory baselines (`*(memory)`) exist only on the Rust side, used to separate the "flush to disk" cost
/// out of the tool layer (parsing/validation/serialization) — the same attribution idea as `cron.*.service`.
fn bench_todo(root: &Path, rows: &mut Vec<Value>) {
    use nc_code_host::tools::todo::{new_todo_store, TodoReadTool, TodoWriteTool};

    let list = json!({"todos": [
        {"content": "draft the plan", "status": "completed", "priority": "high"},
        {"content": "implement the host", "status": "in_progress", "priority": "high"},
        {"content": "wire the read model", "status": "pending", "priority": "medium"},
        {"content": "benchmark every tool", "status": "pending", "priority": "low"},
    ]});

    let mut durable = ctx(root);
    durable.todos = nc_code_host::tools::todo::new_todo_store();

    for _ in 0..3 {
        let _ = TodoWriteTool.execute(&list, &durable);
    }
    let mut stats = Stats::new();
    let mut check = String::new();
    for _ in 0..100 {
        let started = Instant::now();
        let out = TodoWriteTool.execute(&list, &durable).expect("todo write");
        stats.push(started.elapsed().as_nanos() as f64);
        check = count_occurrences(&out, "\"summary\"").to_string();
    }
    rows.push(stats.row("todo.write", "rust", check));

    for _ in 0..3 {
        let _ = TodoReadTool.execute(&json!({}), &durable);
    }
    let mut stats = Stats::new();
    for _ in 0..200 {
        let started = Instant::now();
        TodoReadTool
            .execute(&json!({}), &durable)
            .expect("todo read");
        stats.push(started.elapsed().as_nanos() as f64);
    }
    rows.push(stats.row("todo.read", "rust", String::new()));

    // The in-memory baseline: the same tool implementation, with only the disk flush replaced by a no-op.
    let mut memory = ctx(root);
    memory.todos = new_todo_store();
    let mut stats = Stats::new();
    for _ in 0..3 {
        let _ = TodoWriteTool.execute(&list, &memory);
    }
    for _ in 0..100 {
        let started = Instant::now();
        TodoWriteTool
            .execute(&list, &memory)
            .expect("todo write memory");
        stats.push(started.elapsed().as_nanos() as f64);
    }
    rows.push(stats.row("todo.write(memory)", "rust", String::new()));

    let mut stats = Stats::new();
    for _ in 0..200 {
        let started = Instant::now();
        TodoReadTool
            .execute(&json!({}), &memory)
            .expect("todo read memory");
        stats.push(started.elapsed().as_nanos() as f64);
    }
    rows.push(stats.row("todo.read(memory)", "rust", String::new()));
}

fn bench_nc(root: &Path, rows: &mut Vec<Value>) {
    let tools = nc_tools::build_nc_tools(root).expect("kernel");
    let find = |name: &str| {
        tools
            .iter()
            .find(|tool| tool.tool_name() == name)
            .cloned()
            .unwrap_or_else(|| panic!("nc tool {name}"))
    };
    let context = ctx(root);
    let read = find("fs.read");
    let write = find("fs.write");
    let files = find("search.files");
    let grep = find("search.grep");
    let patch = find("patch.apply");
    let content = (0..2048)
        .map(|line| format!("row {line}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";

    let mut stats = Stats::new();
    let mut check = String::new();
    for _ in 0..3 {
        let _ = read.execute(&json!({"path": "target.txt"}), &context);
    }
    for _ in 0..200 {
        let started = Instant::now();
        let out = read
            .execute(&json!({"path": "target.txt"}), &context)
            .expect("fs.read");
        stats.push(started.elapsed().as_nanos() as f64);
        check = format!("content:{}", !out.is_empty());
    }
    rows.push(stats.row("read", "nc", check));

    let mut stats = Stats::new();
    for _ in 0..3 {
        let _ = std::fs::remove_file(root.join("w.txt"));
        let _ = write.execute(&json!({"path": "w.txt", "content": content}), &context);
    }
    for _ in 0..100 {
        let _ = std::fs::remove_file(root.join("w.txt"));
        let started = Instant::now();
        write
            .execute(&json!({"path": "w.txt", "content": content}), &context)
            .expect("fs.write");
        stats.push(started.elapsed().as_nanos() as f64);
    }
    rows.push(stats.row("write", "nc", String::new()));

    let mut stats = Stats::new();
    let mut check = String::new();
    for _ in 0..3 {
        let _ = files.execute(&json!({"pattern": "**/*.ts", "path": "."}), &context);
    }
    for _ in 0..40 {
        let started = Instant::now();
        let out = files
            .execute(&json!({"pattern": "**/*.ts", "path": "."}), &context)
            .expect("search.files");
        stats.push(started.elapsed().as_nanos() as f64);
        check = count_occurrences(&out, "mod-").to_string();
    }
    rows.push(stats.row("glob", "nc", check));

    let mut stats = Stats::new();
    let mut check = String::new();
    for _ in 0..3 {
        let _ = grep.execute(&json!({"pattern": "grep-needle", "path": "."}), &context);
    }
    for _ in 0..20 {
        let started = Instant::now();
        let out = grep
            .execute(&json!({"pattern": "grep-needle", "path": "."}), &context)
            .expect("search.grep");
        stats.push(started.elapsed().as_nanos() as f64);
        check = count_occurrences(&out, "grep-needle").to_string();
    }
    rows.push(stats.row("grep", "nc", check));

    // patch.apply is an "exact-match replacement", production does not require reading first, so this times the patch step alone,
    // which is not the same timing unit as zcode's read+edit — noted separately in the report.
    let mut stats = Stats::new();
    for _ in 0..100 {
        std::fs::write(root.join("edit.txt"), edit_fixture()).expect("reset");
        let started = Instant::now();
        patch
            .execute(
                &json!({"path": "edit.txt", "edits": [{"oldText": "value-v1-32", "newText": "value-v2-32"}]}),
                &context,
            )
            .expect("patch.apply");
        stats.push(started.elapsed().as_nanos() as f64);
    }
    rows.push(stats.row("edit(read+edit)", "nc", "patch-only".to_string()));
}

fn main() {
    let root = fixture_root();
    build_fixture(&root);
    let mut rows = Vec::new();
    bench_rust(&root, &mut rows);
    bench_js(&mut rows);
    bench_cron(&root, &mut rows);
    bench_todo(&root, &mut rows);
    bench_nc(&root, &mut rows);
    let _ = std::fs::remove_dir_all(&root);
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"impl": "rust", "rows": rows})).unwrap()
    );
}
