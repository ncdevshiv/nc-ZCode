//! Benchmark: workspace hook 发现/摘要/匹配/打码的热路径。
//!
//! 会话启动时要把工作区 hook 全部 canonical 化并算两条 SHA-256 摘要（每条声明一条、
//! 整包一条）；每个 tool call 还要按 matcher 匹配一次。配置越复杂（几十条 hook），
//! 这两处就是启动与工具回合里最容易被忽略的开销。

use criterion::{criterion_group, criterion_main, Criterion};
use nc_code_host::workspace_hooks::config::{
    read_sources, resolve_runtime_root, RuntimeRoot, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_TIMEOUT_MS,
};
use nc_code_host::workspace_hooks::digest::build_bundle;
use nc_code_host::workspace_hooks::display::sanitize_hook_display_text;
use nc_code_host::workspace_hooks::runtime::matches_hook_matcher;
use serde_json::json;
use std::hint::black_box;
use std::path::PathBuf;

fn workspace_with_hooks(hook_count: usize) -> PathBuf {
    let base = std::env::temp_dir().join("zcode-bench-hooks");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join(".zcode")).unwrap();
    let mut pre_tool = Vec::new();
    for index in 0..hook_count {
        pre_tool.push(json!({
            "matcher": if index % 2 == 0 { "Bash".to_string() } else { format!("Write|Edit{index}") },
            "hooks": [
                {"type": "command", "command": format!("node scripts/check-{index}.js --strict")},
                {"type": "process", "command": "git", "args": ["status", "--short"], "timeoutMs": 5000.0}
            ]
        }));
    }
    let config = json!({
        "hooks": {
            "enabled": true,
            "events": {
                "PreToolUse": pre_tool,
                "PostToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": "node audit.js"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "node stop.js", "async": true}]}]
            }
        }
    });
    std::fs::write(
        base.join(".zcode").join("config.json"),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .unwrap();
    base
}

fn hooks_hot_paths(criteria: &mut Criterion) {
    let workspace = workspace_with_hooks(24);
    let (sources, errors) = read_sources(&workspace, None);
    assert!(errors.is_empty());
    let roots: Vec<_> = sources
        .iter()
        .map(|source| nc_code_host::workspace_hooks::config::RootGates {
            enabled: source.hooks.enabled,
            timeout_ms: source.hooks.timeout_ms,
            max_output_bytes: source.hooks.max_output_bytes,
        })
        .collect();
    let runtime: RuntimeRoot = resolve_runtime_root(&roots);
    assert_eq!(runtime.timeout_ms, DEFAULT_TIMEOUT_MS);
    assert_eq!(runtime.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
    let bundle = build_bundle(
        "bench",
        &workspace,
        &sources,
        &runtime,
        "2026-01-01T00:00:00.000Z",
    )
    .expect("bundle");
    assert!(bundle.hooks.len() >= 48);

    let mut group = criteria.benchmark_group("workspace_hooks");

    group.bench_function("build_bundle_50_declarations", |bencher| {
        bencher.iter(|| {
            black_box(
                build_bundle(
                    "bench",
                    black_box(&workspace),
                    black_box(&sources),
                    black_box(&runtime),
                    "2026-01-01T00:00:00.000Z",
                )
                .expect("bundle"),
            )
        });
    });

    group.bench_function("matcher_exact_set", |bencher| {
        bencher.iter(|| black_box(matches_hook_matcher(Some("Write|Edit7"), Some("Edit7"))));
    });

    group.bench_function("matcher_regex_cached", |bencher| {
        bencher.iter(|| black_box(matches_hook_matcher(Some("^Ba.*$"), Some("Bash"))));
    });

    let long_command = format!(
        "node scripts/audit.js --token={} --url=https://user:pass@example.com/x?access_token={}",
        "a".repeat(120),
        "b".repeat(120)
    );
    group.bench_function("sanitize_long_command", |bencher| {
        bencher.iter(|| black_box(sanitize_hook_display_text(black_box(&long_command))));
    });

    group.finish();
}

criterion_group!(benches, hooks_hot_paths);
criterion_main!(benches);
