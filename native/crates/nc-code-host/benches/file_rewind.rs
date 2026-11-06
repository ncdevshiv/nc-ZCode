//! Benchmark: 文件回滚的计划与分桶（`applyFileRewind` 的热路径）。
//!
//! 每开一次「撤销本轮文件改动」都要把整轮台账按路径归并、再对每个文件做一次
//! SHA-256 内容比对。轮次越大（一轮里改上百个文件），这条路径就是 UI 卡顿的来源，
//! 所以它必须被度量而不是被假设。

use criterion::{criterion_group, criterion_main, Criterion};
use nc_code_host::file_ledger::FileChangeRecord;
use nc_code_host::rewind;
use std::collections::HashMap;
use std::hint::black_box;
use std::sync::{Arc, Mutex};

fn records(count: usize) -> Vec<FileChangeRecord> {
    (0..count)
        .map(|index| {
            let path = format!("src/module-{index}.ts");
            let before = if index % 3 == 0 {
                None
            } else {
                Some(format!("content {index} v1"))
            };
            let after = format!("content {index} v2");
            let (additions, deletions) =
                nc_code_host::file_ledger::line_change_counts(before.as_deref(), Some(&after));
            FileChangeRecord {
                turn_id: "t1".to_string(),
                tool_call_id: format!("call-{index}"),
                tool_name: "Write".to_string(),
                path,
                existed_before: before.is_some(),
                before_content: before,
                after_content: Some(after),
                additions,
                deletions,
            }
        })
        .collect()
}

/// 内存只读视图：基准测的是**归并 + 哈希 + 分桶**，不掺磁盘噪声。
fn reader(contents: &HashMap<String, String>) -> impl Fn(&str) -> Result<Option<String>, String> {
    let contents = Arc::new(Mutex::new(contents.clone()));
    move |path: &str| Ok(contents.lock().unwrap().get(path).cloned())
}

fn bench_rewind_plan(c: &mut Criterion) {
    let mut group = c.benchmark_group("file_rewind");
    let source = records(200);

    group.bench_function("operations_merge_200_records", |b| {
        b.iter(|| rewind::operations(black_box(&source)).len())
    });

    let ops = rewind::operations(&source);
    let contents: HashMap<String, String> = source
        .iter()
        .map(|record| {
            (
                record.path.clone(),
                record.after_content.clone().unwrap_or_default(),
            )
        })
        .collect();
    let read = reader(&contents);
    group.bench_function("preview_200_paths_all_safe", |b| {
        b.iter(|| {
            let preview = rewind::preview(black_box(&ops), &read);
            (preview.can_apply, preview.safe_files.len())
        })
    });

    // 一个外部改动就足以让整笔回滚被拒——测的是"最坏情况也要扫完全部文件"的代价。
    let mut tampered = contents.clone();
    if let Some(first) = tampered.keys().next() {
        let key = first.clone();
        tampered.insert(key, "edited by the user".to_string());
    }
    let read_tampered = reader(&tampered);
    group.bench_function("preview_200_paths_one_unsafe", |b| {
        b.iter(|| {
            let preview = rewind::preview(black_box(&ops), &read_tampered);
            (preview.can_apply, preview.unsafe_files.len())
        })
    });
    group.finish();
}

/// 台账查询本身也在热路径上：turnHeader 的 `fileChanges` 每轮收口都要算一次。
/// 这里用真实 `Session`（带 transcript）而不是裸 Vec，测的是产品实际走的那条路。
fn bench_ledger_query(c: &mut Criterion) {
    let root = std::env::temp_dir().join("zcode-bench-ledger");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("ledger root");
    let store = nc_code_host::store::TranscriptStore::new(&root);
    // transcript 文件必须先存在：ledger 的 sink 走 append（不 create），
    // 少了这一步就是"记账永远失败"，测出来的数字也就不是产品路径。
    store
        .create_session(&nc_code_host::store::SessionMeta {
            session_id: "bench-ledger".to_string(),
            title: "bench".to_string(),
            title_source: "default".to_string(),
            log_epoch: "epoch".to_string(),
            revision: 0,
            workspace_identity: "/tmp/ws".to_string(),
            created_at: 0,
            updated_at: 0,
            task_type: None,
            parent_session_id: None,
            plan_previous_mode: String::new(),
            followup_mode: "queue".to_string(),
            collaboration_mode: "build".to_string(),
            provider: String::new(),
            model: String::new(),
            thought: String::new(),
            goal: None,
        })
        .expect("create session");
    let session = nc_code_host::session::Session::new(nc_code_host::session::SessionInit {
        id: "bench-ledger".to_string(),
        workspace_root: root.clone(),
        workspace_identity: "/tmp/ws".to_string(),
        created_at: 0,
        task_type: None,
        parent_session_id: None,
        log_epoch: "epoch".to_string(),
        title: "bench".to_string(),
        title_source: "default".to_string(),
        rows: Vec::new(),
        revision: 0,
        transcript_acks: std::collections::HashMap::new(),
        next_row_id: 0,
        history: Vec::new(),
        turn_history_starts: Vec::new(),
        file_changes: Vec::new(),
        followup_mode: "queue".to_string(),
        collaboration_mode: "build".to_string(),
        provider: String::new(),
        model: String::new(),
        thought: String::new(),
        goal: None,
        hooks: {
            let hook_base = std::env::temp_dir().join("zcode-bench-hooks-ledger");
            let hook_store = std::sync::Arc::new(nc_code_host::workspace_hooks::TrustStore::new(
                hook_base.join(nc_code_host::workspace_hooks::TRUST_STORE_FILE),
            ));
            std::sync::Arc::new(nc_code_host::workspace_hooks::WorkspaceHookAdmission::new(
                hook_base,
                "bench-ledger".to_string(),
                "bench-ledger".to_string(),
                hook_store,
            ))
        },
        store,
    });
    for record in records(200) {
        session.file_ledger().record(record).expect("record");
    }
    c.bench_function("ledger_summary_for_turn_200_records", |b| {
        b.iter(|| {
            session
                .file_ledger()
                .summary_for_turn("t1")
                .map(|s| s.files)
        })
    });
    let _ = std::fs::remove_dir_all(&root);
}

criterion_group!(benches, bench_rewind_plan, bench_ledger_query);
criterion_main!(benches);
