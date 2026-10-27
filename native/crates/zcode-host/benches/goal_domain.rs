//! Benchmark: goal 状态机的热路径。
//!
//! 这条路径每轮至少跑一次（记账）且每次裁判跑两次（begin/record），还要在每次
//! 状态变更时重建整份 `goalStateSchema` 投影（客户端快照直接吃这份 JSON）。
//! 20 条 verifications 的保留上限与 4000 字符 objective 是协议常量，所以规模是
//! 有界的——但"有界"不等于"便宜"，这些函数在 goal 循环里被反复调用。

use criterion::{criterion_group, criterion_main, Criterion};
use serde_json::json;
use std::hint::black_box;
use zcode_host::goal::{
    format_goal_completion_verification_prompt, format_goal_continuation_prompt,
    parse_verification_text, GoalRecord, VerificationOutcome, MAX_GOAL_OBJECTIVE_CHARS,
    VERIFICATIONS_RETAINED,
};

fn record_with_ledger(max_entries: usize) -> GoalRecord {
    let mut record = GoalRecord::new("goal-1", "ship the port with no mocks");
    for index in 0..max_entries {
        record.begin_verification(index as i64);
        record.record_verification(
            VerificationOutcome::NotSatisfied,
            index as i64,
            Some(index as u64 + 1),
            Some("missing coverage for the queue subsystem".to_string()),
            Some(format!("add coverage for subsystem {index}")),
        );
        record.set_iteration_items(
            vec![json!({"id": format!("todo-{index}"), "text": "work", "status": "pending"})],
            "notSatisfied",
            index as i64,
        );
        record.set_status(zcode_host::goal::GoalStatus::Active);
    }
    record
}

fn goal_hot_paths(criteria: &mut Criterion) {
    let mut group = criteria.benchmark_group("goal_domain");

    let mut ledger = record_with_ledger(VERIFICATIONS_RETAINED);
    group.bench_function("record_verification_with_full_ledger", |bencher| {
        let mut iteration = ledger.iteration;
        bencher.iter(|| {
            iteration += 1;
            ledger.begin_verification(black_box(iteration as i64));
            ledger.record_verification(
                black_box(VerificationOutcome::NotSatisfied),
                black_box(iteration as i64),
                Some(black_box(iteration as u64)),
                Some(black_box("reason text".to_string())),
                Some(black_box("next action".to_string())),
            );
        });
    });

    group.bench_function("projection_with_full_ledger", |bencher| {
        bencher.iter(|| black_box(ledger.to_projection()));
    });

    let long_objective = "字".repeat(MAX_GOAL_OBJECTIVE_CHARS);
    let long_goal = GoalRecord::new("goal-2", long_objective);
    group.bench_function("continuation_prompt_max_objective", |bencher| {
        bencher.iter(|| {
            black_box(format_goal_continuation_prompt(
                &long_goal,
                Some(("reason", "next action")),
            ))
        });
    });
    group.bench_function("verification_prompt_max_objective", |bencher| {
        bencher.iter(|| black_box(format_goal_completion_verification_prompt(&long_goal)));
    });

    let fenced = format!(
        "Here is the verdict:\n```json\n{}\n```\nThanks!",
        r#"{"passed": false, "reason": "tests missing", "nextAction": "write the tests"}"#
    );
    group.bench_function("parse_verifier_text_fenced", |bencher| {
        bencher.iter(|| black_box(parse_verification_text(black_box(&fenced))));
    });

    group.finish();
}

criterion_group!(benches, goal_hot_paths);
criterion_main!(benches);
