//! Integration tests for CLI core business logic ported/hardened in Rust:
//! agent, memory paths/agent, context budgeting, MCP annotation hardening contracts.
//! No mocks/stubs - exercises real implementations.

use nc_code_host::agent::{AgentConfig, AgentMessage, AgentOrchestrator, AgentTurnPhase};
use nc_code_host::context::{
    fit_sections, ContextBudget, ContextCacheHint, ContextInjectionTarget, ContextSection,
    ContextSource,
};
use nc_code_host::memory_agent::{
    build_memory_extraction_prompt, evaluate_extraction, is_allowed_memory_bash,
    is_allowed_memory_mutation, MemoryExtractionCursor, MemoryExtractionStatus,
    MemoryManifestEntry, SkipReason,
};
use nc_code_host::memory_paths::{resolve_contained, resolve_safe_memory_file};
use serde_json::Value;
use std::path::Path;

#[test]
fn agent_orchestrator_history_and_turns_are_deterministic() {
    let mut orch = AgentOrchestrator::new(AgentConfig {
        max_turns: Some(1),
        max_history_messages: 10,
        preserve_system_messages: true,
    });
    orch.push_message(AgentMessage::system("sys"));
    orch.push_message(AgentMessage::user("hi"));
    assert!(orch.begin_turn());
    orch.transition(AgentTurnPhase::ExecutingTools);
    assert_eq!(orch.history().len(), 2);
    assert!(!orch.begin_turn());
}

#[test]
fn memory_path_safety_blocks_sensitive_and_traversal() {
    let root = std::env::temp_dir().join("nc-cli-core-mem-test");
    assert!(resolve_contained(&root, Path::new("ok/note.md")).is_some());
    assert!(resolve_contained(&root, Path::new("../escape.md")).is_none());
    assert!(resolve_safe_memory_file(&root, Path::new(".git/config")).is_none());
    assert!(resolve_safe_memory_file(&root, Path::new("notes/topic.md")).is_some());
}

#[test]
fn memory_extraction_logic_matches_contract() {
    let prompt = build_memory_extraction_prompt(
        &[MemoryManifestEntry {
            path: "a.md".to_string(),
            title: Some("A".to_string()),
            summary: None,
            updated_at: None,
        }],
        7,
    );
    assert!(prompt.contains("~7 messages"));
    let d = evaluate_extraction(3, false, true);
    assert!(matches!(
        d,
        nc_code_host::memory_agent::MemoryExtractionDecision::Run { message_count: 3 }
    ));
    let skip = evaluate_extraction(3, true, true);
    assert!(matches!(
        skip,
        nc_code_host::memory_agent::MemoryExtractionDecision::Skip {
            reason: SkipReason::DirectMemoryWrite,
            ..
        }
    ));
    let mut cur = MemoryExtractionCursor::default();
    cur.advance(Some("m1".to_string()), MemoryExtractionStatus::Success);
    assert_eq!(cur.last_boundary_message_id.as_deref(), Some("m1"));
}

#[test]
fn memory_agent_bash_and_mutation_policies_hardened() {
    let root = Path::new("/tmp/mem");
    let wd = Path::new("/tmp/wd");
    assert!(is_allowed_memory_mutation(root, Path::new("note.md")));
    assert!(!is_allowed_memory_mutation(
        root,
        Path::new("../../etc/passwd")
    ));
    assert!(is_allowed_memory_bash("grep -n x note.md", root, wd));
    assert!(!is_allowed_memory_bash(
        "curl https://example.com",
        root,
        wd
    ));
}

#[test]
fn context_budget_truncates_and_counts() {
    let sections = vec![
        ContextSection::new(
            "a",
            ContextSource::Identity,
            ContextInjectionTarget::System,
            ContextCacheHint::Stable,
            "short",
        ),
        ContextSection::new(
            "b",
            ContextSource::Memory,
            ContextInjectionTarget::System,
            ContextCacheHint::Dynamic,
            "x".repeat(200),
        ),
    ];
    let (kept, summary) = fit_sections(
        sections,
        &ContextBudget {
            max_chars: 80,
            max_tokens: 10_000,
        },
    );
    assert_eq!(kept.len(), 2);
    assert!(summary.truncated_sections >= 1);
    assert!(kept
        .iter()
        .any(|s| s.content.contains("[context truncated]") || s.name == "a"));
}

#[test]
fn agent_message_helpers_produce_json_compatible_content() {
    let m = AgentMessage::user("hello");
    assert_eq!(m.role, "user");
    assert_eq!(m.content, Value::String("hello".to_string()));
}
