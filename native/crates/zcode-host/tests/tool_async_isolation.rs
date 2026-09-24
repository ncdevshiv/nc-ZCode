//! 工具执行不阻塞 async 运行时：并发进行中的**非工具**异步工作（帧 ticker）必须
//! 在长工具（Bash sleep / nc-tools 大扫描）执行期间继续推进。
//!
//! 这锁住的是 `turn.rs` 的 `spawn_blocking` 决策：若工具在 async 线程上直接同步
//! 执行，长工具会把 runtime worker 占住，帧 ticker（10ms）停摆，订阅者收不到帧。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use zcode_host::tools::{Tool, ToolContext, ToolRegistry};

/// 一个"慢工具"：占用 300ms 同步时间（模拟 Bash sleep / 大扫描）。
struct SlowTool;

impl Tool for SlowTool {
    fn contract(&self) -> zcode_host::tools::ToolContract {
        zcode_host::tools::ToolContract {
            name: "Slow".to_string(),
            description: "sleep".to_string(),
            input_schema: json!({"type": "object"}),
            read_only: true,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: zcode_host::tools::SCOPE_NONE,
            timeout_ms: 5_000,
            max_output_bytes: 1024,
        }
    }

    fn execute(
        &self,
        _input: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<String, zcode_host::tools::ToolError> {
        std::thread::sleep(Duration::from_millis(300));
        Ok("done".to_string())
    }
}

fn ctx() -> ToolContext {
    ToolContext {
        workspace_root: std::env::temp_dir(),
        full_access: true,
        trace_id: "t".to_string(),
        turn_id: "turn".to_string(),
        read_state: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        todos: zcode_host::tools::todo::new_todo_store(),
        provider_config: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_tool_does_not_stall_the_async_runtime() {
    let mut registry = ToolRegistry::with_defaults();
    registry = registry.with_test_tool(Arc::new(SlowTool));
    let tool = registry.get("Slow").expect("Slow tool registered");

    let ticks = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&ticks);
    // 模拟帧 ticker：每 20ms 自增，运行在 runtime 的异步侧。
    let ticker = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(20));
        interval.tick().await;
        loop {
            interval.tick().await;
            counter.fetch_add(1, Ordering::SeqCst);
        }
    });

    let started = Instant::now();
    // 与 turn.rs 相同的执行路径：spawn_blocking 里跑同步工具。
    let outcome = tokio::task::spawn_blocking(move || tool.execute(&json!({}), &ctx()))
        .await
        .expect("tool task")
        .expect("tool result");
    assert_eq!(outcome, "done");
    let elapsed = started.elapsed();

    // 工具期间 ticker 仍在跑：ticks 应远大于 0（若工具占住 runtime worker，
    // 单 worker 运行时下这里会是 0 或极少）。
    let observed = ticks.load(Ordering::SeqCst);
    ticker.abort();
    assert!(
        elapsed >= Duration::from_millis(250),
        "tool must actually be slow"
    );
    assert!(
        observed >= 5,
        "async runtime stalled during tool execution: only {observed} ticker ticks in {elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_async_work_progresses_while_a_tool_runs() {
    // 4 个 runtime worker，2 个慢工具同时执行 + 1 个高频异步计数器。
    let mut registry = ToolRegistry::with_defaults();
    registry = registry.with_test_tool(Arc::new(SlowTool));
    let tool_a = registry.get("Slow").expect("Slow");

    let counter = Arc::new(AtomicU64::new(0));
    let spin = Arc::clone(&counter);
    let spinner = tokio::spawn(async move {
        for _ in 0..200 {
            spin.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
        }
    });

    let t1 = tokio::task::spawn_blocking(move || tool_a.execute(&json!({}), &ctx()));
    let tool_b = registry.get("Slow").expect("Slow");
    let t2 = tokio::task::spawn_blocking(move || tool_b.execute(&json!({}), &ctx()));
    let (r1, r2) = tokio::join!(t1, t2);
    assert_eq!(r1.expect("a").expect("a"), "done");
    assert_eq!(r2.expect("b").expect("b"), "done");
    spinner.await.expect("spinner");
    assert!(
        counter.load(Ordering::SeqCst) >= 190,
        "async work was starved by blocking tools"
    );
}
