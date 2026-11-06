//! Benchmark: transcript replay (cold resume) throughput.
//!
//! Measures `TranscriptStore::load_transcript` over a transcript with a realistic
//! mix of rows, acks and history entries (session memory). Guards against
//! regressions in the replay hot path that runs on every host restart.

use criterion::{criterion_group, criterion_main, Criterion};
use nc_code_host::session::{Session, SessionInit};
use nc_code_host::store::{SessionMeta, TranscriptStore};
use nc_code_provider::request::{Message, MessageRole};
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

fn make_session(root: &PathBuf, session_id: &str, seeded_rows: usize) -> std::sync::Arc<Session> {
    let store = TranscriptStore::new(root);
    let meta = SessionMeta {
        session_id: session_id.to_string(),
        title: "bench".to_string(),
        title_source: "default".to_string(),
        log_epoch: "epoch".to_string(),
        revision: 0,
        workspace_identity: "/tmp/ws".to_string(),
        created_at: 0,
        followup_mode: "queue".to_string(),
        collaboration_mode: "build".to_string(),
        provider: String::new(),
        model: String::new(),
        thought: String::new(),
        goal: None,
        updated_at: 0,
        task_type: None,
        parent_session_id: None,
        plan_previous_mode: String::new(),
    };
    store.create_session(&meta).unwrap();
    let mut rows = Vec::new();
    for i in 0..seeded_rows {
        let row = json!({"rowId": i as u64, "kind": "turnHeader", "state": "completed"});
        store.append_row(session_id, &row).unwrap();
        rows.push(row);
    }
    Session::new(SessionInit {
        id: session_id.to_string(),
        workspace_root: root.clone(),
        workspace_identity: "/tmp/ws".to_string(),
        log_epoch: "epoch".to_string(),
        title: "bench".to_string(),
        title_source: "default".to_string(),
        rows,
        revision: 0,
        transcript_acks: HashMap::new(),
        next_row_id: seeded_rows as u64,
        history: Vec::new(),
        turn_history_starts: Vec::new(),
        file_changes: Vec::new(),
        followup_mode: "queue".to_string(),
        collaboration_mode: "build".to_string(),
        provider: String::new(),
        model: String::new(),
        thought: String::new(),
        goal: None,
        hooks: bench_hook_admission("bench-seed"),
        store,
        created_at: 0,
        task_type: None,
        parent_session_id: None,
    })
}

fn bench_hook_admission(
    id: &str,
) -> std::sync::Arc<nc_code_host::workspace_hooks::WorkspaceHookAdmission> {
    let base = std::env::temp_dir().join(format!("zcode-bench-hooks-{id}"));
    let store = std::sync::Arc::new(nc_code_host::workspace_hooks::TrustStore::new(
        base.join(nc_code_host::workspace_hooks::TRUST_STORE_FILE),
    ));
    std::sync::Arc::new(nc_code_host::workspace_hooks::WorkspaceHookAdmission::new(
        base,
        id.to_string(),
        id.to_string(),
        store,
    ))
}

fn seed_transcript(root: &PathBuf, session_id: &str, n_history: usize, n_rows: usize) {
    let store = TranscriptStore::new(root);
    let meta = SessionMeta {
        session_id: session_id.to_string(),
        title: "bench".to_string(),
        title_source: "default".to_string(),
        log_epoch: "epoch".to_string(),
        revision: 0,
        workspace_identity: "/tmp/ws".to_string(),
        created_at: 0,
        followup_mode: "queue".to_string(),
        collaboration_mode: "build".to_string(),
        provider: String::new(),
        model: String::new(),
        thought: String::new(),
        goal: None,
        updated_at: 0,
        task_type: None,
        parent_session_id: None,
        plan_previous_mode: String::new(),
    };
    store.create_session(&meta).unwrap();
    for i in 0..n_rows {
        store
            .append_row(
                session_id,
                &json!({"rowId": i, "kind": "turnHeader", "state": "completed"}),
            )
            .unwrap();
    }
    for i in 0..n_history {
        store
            .append_history(
                session_id,
                &Message::text(
                    if i % 2 == 0 {
                        MessageRole::User
                    } else {
                        MessageRole::Assistant
                    },
                    format!("message {i}"),
                ),
            )
            .unwrap();
    }
}

fn bench_load_transcript(c: &mut Criterion) {
    let root = std::env::temp_dir().join("zcode-bench-transcript");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let session_id = "bench-session";
    seed_transcript(&root, session_id, 200, 50);
    let store = TranscriptStore::new(&root);
    c.bench_function("load_transcript_200hist_50rows", |b| {
        b.iter(|| {
            let t = store.load_transcript(session_id).unwrap();
            assert!(!t.history.is_empty());
        })
    });
    let _ = fs::remove_dir_all(&root);
}

fn bench_append_history(c: &mut Criterion) {
    let root = std::env::temp_dir().join("zcode-bench-append");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let session_id = "bench-append";
    let store = TranscriptStore::new(&root);
    let meta = SessionMeta {
        session_id: session_id.to_string(),
        title: "bench".to_string(),
        title_source: "default".to_string(),
        log_epoch: "epoch".to_string(),
        revision: 0,
        workspace_identity: "/tmp/ws".to_string(),
        created_at: 0,
        followup_mode: "queue".to_string(),
        collaboration_mode: "build".to_string(),
        provider: String::new(),
        model: String::new(),
        thought: String::new(),
        goal: None,
        updated_at: 0,
        task_type: None,
        parent_session_id: None,
        plan_previous_mode: String::new(),
    };
    store.create_session(&meta).unwrap();
    let mut i = 0usize;
    c.bench_function("append_history_single", |b| {
        b.iter(|| {
            store
                .append_history(
                    session_id,
                    &Message::text(MessageRole::User, format!("m{i}")),
                )
                .unwrap();
            i += 1;
        })
    });
    let _ = std::fs::remove_dir_all(&root);
}

fn bench_upsert_row(c: &mut Criterion) {
    let root = std::env::temp_dir().join("zcode-bench-upsert");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let session_id = "bench-upsert";
    let store = TranscriptStore::new(&root);
    let meta = SessionMeta {
        session_id: session_id.to_string(),
        title: "bench".to_string(),
        title_source: "default".to_string(),
        log_epoch: "epoch".to_string(),
        revision: 0,
        workspace_identity: "/tmp/ws".to_string(),
        created_at: 0,
        followup_mode: "queue".to_string(),
        collaboration_mode: "build".to_string(),
        provider: String::new(),
        model: String::new(),
        thought: String::new(),
        goal: None,
        updated_at: 0,
        task_type: None,
        parent_session_id: None,
        plan_previous_mode: String::new(),
    };
    store.create_session(&meta).unwrap();
    // seed 100 rows so upsert has to scan a realistic in-memory vec.
    for i in 0..100usize {
        store
            .append_row(
                session_id,
                &serde_json::json!({"rowId": i, "state": "running"}),
            )
            .unwrap();
    }
    let mut i = 0usize;
    c.bench_function("upsert_row_single", |b| {
        b.iter(|| {
            store
                .append_upsert(
                    session_id,
                    &serde_json::json!({"rowId": i % 100, "state": "completed"}),
                )
                .unwrap();
            i += 1;
        })
    });
    let _ = std::fs::remove_dir_all(&root);
}

fn bench_snapshot_projection(c: &mut Criterion) {
    // 全量 snapshot 是 subscribe / recovery 的热路径：60 行尾窗口 + 固定信封字段。
    let rows: Vec<serde_json::Value> = (0..60u64)
        .map(|i| json!({"rowId": i, "turnId": "t", "kind": "assistantText", "text": "x"}))
        .collect();
    let usage = json!({"inputTokens": 0, "outputTokens": 0});
    let title = "bench".to_string();
    let title_source = "default".to_string();
    c.bench_function("snapshot_projection_60rows", |b| {
        b.iter(|| {
            let snap = nc_code_host::snapshot::conversation_snapshot(
                &nc_code_host::snapshot::SnapshotInput {
                    session_id: "bench",
                    log_epoch: "epoch",
                    seq: 1,
                    revision: 0,
                    phase: "draft",
                    can_stop: false,
                    last_error: None,
                    title: &title,
                    title_source: &title_source,
                    rows: &rows,
                    total_row_count: rows.len(),
                    first_row_id: Some(0),
                    usage: &usage,
                    queue: serde_json::json!({"items": [], "autoDrain": true}),
                    pending_interactions: serde_json::json!([]),
                    stop_requested: false,
                    followup_mode: "queue".to_string(),
                    provider: String::new(),
                    model: String::new(),
                    thought: String::new(),
                    collaboration_mode: "build".to_string(),
                    goal: serde_json::Value::Null,
                    active_works: Vec::new(),
                    workspace_hook_admission: serde_json::Value::Null,
                },
            );
            assert!(snap.get("rows").is_some());
        })
    });
}

fn bench_session_creation(c: &mut Criterion) {
    let root = std::env::temp_dir().join("zcode-bench-session-create");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let mut n = 0usize;
    c.bench_function("session_new_with_50_rows", |b| {
        b.iter(|| {
            let id = format!("bench-sess-{n}");
            let s = make_session(&root, &id, 50);
            assert_eq!(s.total_row_count(), 50);
            n += 1;
        })
    });
    let _ = fs::remove_dir_all(&root);
}

/// 分支截断之后的冷恢复回放：`rowsTruncated` + `historyCompacted` 两条事件
/// 都必须生效，否则 edit/retry 之后的重启会把已经放弃的分支又放回来。
/// 测的是 replay（每次 host 重启都会走这条路径），不是写入。
fn bench_replay_after_branch_cut(c: &mut Criterion) {
    let root = std::env::temp_dir().join("zcode-bench-branch-cut");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let session_id = "bench-branch";
    let store = TranscriptStore::new(&root);
    let meta = SessionMeta {
        session_id: session_id.to_string(),
        title: "bench".to_string(),
        title_source: "default".to_string(),
        log_epoch: "epoch".to_string(),
        revision: 0,
        workspace_identity: "/tmp/ws".to_string(),
        created_at: 0,
        followup_mode: "queue".to_string(),
        collaboration_mode: "build".to_string(),
        provider: String::new(),
        model: String::new(),
        thought: String::new(),
        goal: None,
        updated_at: 0,
        task_type: None,
        parent_session_id: None,
        plan_previous_mode: String::new(),
    };
    store.create_session(&meta).unwrap();
    // 50 轮 × 3 个实体行（turnHeader / userInput / assistantText，各带一次终态 upsert）
    // = 150 行、100 条模型历史。
    for turn in 0..50usize {
        let base = (turn * 4) as u64;
        store
            .append_row(session_id, &json!({"rowId": base, "turnId": format!("t{turn}"), "kind": "turnHeader", "state": "running"}))
            .unwrap();
        store
            .append_row(session_id, &json!({"rowId": base + 1, "turnId": format!("t{turn}"), "kind": "userInput", "text": format!("q{turn}")}))
            .unwrap();
        store
            .append_history(
                session_id,
                &Message::text(MessageRole::User, format!("q{turn}")),
            )
            .unwrap();
        store
            .append_row(session_id, &json!({"rowId": base + 2, "turnId": format!("t{turn}"), "kind": "assistantText", "state": "streaming"}))
            .unwrap();
        store
            .append_history(
                session_id,
                &Message::text(MessageRole::Assistant, format!("a{turn}")),
            )
            .unwrap();
        store
            .append_upsert(session_id, &json!({"rowId": base + 2, "turnId": format!("t{turn}"), "kind": "assistantText", "state": "complete"}))
            .unwrap();
        store
            .append_upsert(session_id, &json!({"rowId": base, "turnId": format!("t{turn}"), "kind": "turnHeader", "state": "completedSuccess"}))
            .unwrap();
    }
    // 截断最后 10 轮：rowId >= 160 的行消失（150 - 30 = 120 行），历史保留 80 条。
    store.append_rows_truncated(session_id, 160).unwrap();
    store
        .append_history_compaction(session_id, &{
            let history: Vec<Message> = (0..40)
                .flat_map(|turn| {
                    [
                        Message::text(MessageRole::User, format!("q{turn}")),
                        Message::text(MessageRole::Assistant, format!("a{turn}")),
                    ]
                })
                .collect();
            history
        })
        .unwrap();

    c.bench_function("replay_transcript_after_branch_cut", |b| {
        let transcript = store.load_transcript(session_id).unwrap();
        assert_eq!(transcript.rows.len(), 120, "rows >= 160 must be gone");
        assert_eq!(
            transcript
                .turn_history_starts
                .last()
                .map(|(turn_id, _)| turn_id.as_str()),
            Some("t39"),
            "the last surviving anchor must be the last kept turn, not a truncated one"
        );
        assert_eq!(
            transcript.history.len(),
            80,
            "history cut to the kept prefix"
        );
        assert_eq!(
            transcript.turn_history_starts.len(),
            40,
            "branch anchors must survive the cut so a later edit can truncate again"
        );
        b.iter(|| store.load_transcript(session_id).unwrap());
    });
    let _ = fs::remove_dir_all(&root);
}

criterion_group!(
    benches,
    bench_load_transcript,
    bench_replay_after_branch_cut,
    bench_append_history,
    bench_upsert_row,
    bench_snapshot_projection,
    bench_session_creation
);
criterion_main!(benches);
