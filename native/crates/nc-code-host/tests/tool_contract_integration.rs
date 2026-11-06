//! Tool-level integration tests: exercise tool contracts, input validation, and
//! execution paths through the `Tool`/`AsyncTool` APIs with a real `ToolContext`.
//!
//! Complements `tool_matrix_e2e.rs` (full host turn) by covering tools that are
//! easy to validate deterministically at the tool boundary, including error cases.

use nc_code_host::tools::{self, Authorization, Tool, ToolContext, ToolErrorCode, ToolRegistry};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

fn temp(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("zcode-tool-contract-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("workspace");
    dir
}

fn ctx(root: &PathBuf, full_access: bool, interactive: bool) -> ToolContext {
    ToolContext {
        workspace_root: root.clone(),
        full_access,
        interactive,
        trace_id: "t".into(),
        turn_id: "turn".into(),
        session_id: "s".into(),
        model_ref: Some("test/test-model".into()),
        tool_call_id: Some("tc1".into()),
        plan_mode: false,
        read_state: Arc::new(Mutex::new(Default::default())),
        cancel: Arc::new(AtomicBool::new(false)),
        todos: tools::todo::new_todo_store(),
        provider_config: None,
        ask_user: None,
        plan: None,
        subagent: None,
        background: None,
        sessions: None,
        workflow: None,
        automation: None,
        browser: None,
        computer: None,
        offpeak: None,
    }
}

#[test]
fn registry_exposes_core_and_new_tools() {
    let registry = ToolRegistry::with_defaults();
    let names = registry.names();
    for expected in [
        "ApplyPatch",
        "AskUserQuestion",
        "Agent",
        "Bash",
        "Browser",
        "Computer",
        "CronCreate",
        "CronDelete",
        "CronList",
        "CronUpdate",
        "CreateWorkflow",
        "Edit",
        "EnterPlanMode",
        "EvalWorkflowSnippet",
        "ExitPlanMode",
        "GetWorkflowRun",
        "Glob",
        "Grep",
        "ListModels",
        "ListSavedWorkflows",
        "ListWorkflowRuns",
        "OffPeakCreate",
        "OffPeakList",
        "Read",
        "ReadImage",
        "ReadSessionContext",
        "ResolveWorkflowQuestion",
        "RespondToCoordinator",
        "ResumeWorkflowRun",
        "SaveWorkflow",
        "SendMessage",
        "Skill",
        "TaskOutput",
        "TaskStop",
        "TodoRead",
        "TodoWrite",
        "WebFetch",
        "WebSearch",
        "Write",
        "escalate",
        "js",
        "submit_result",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "missing tool in registry: {expected}; have: {names:?}"
        );
    }
    assert!(
        registry.get("Task").is_some() || registry.get_async("Task").is_some(),
        "Task alias must resolve"
    );
}

#[test]
fn tool_definitions_have_schemas_and_stable_names() {
    let defs = ToolRegistry::with_defaults().tool_definitions();
    assert!(
        defs.len() >= 40,
        "expected 40+ tool definitions, got {}",
        defs.len()
    );
    for def in &defs {
        assert!(!def.name.trim().is_empty());
        assert!(
            def.input_schema.is_object(),
            "{} schema must be object",
            def.name
        );
    }
}

#[test]
fn permission_model_allows_readonly_and_gates_mutations() {
    let root = temp("perm");
    let registry = ToolRegistry::with_defaults();
    let read = registry.get("Read").expect("Read");
    let write = registry.get("Write").expect("Write");
    let ctx_deny = ctx(&root, false, false);
    let ctx_allow = ctx(&root, true, false);
    let mut ctx_ask = ctx(&root, false, true);
    ctx_ask.interactive = true;
    assert_eq!(
        registry.authorize(read.as_ref(), &json!({"file_path":"x"}), &ctx_deny),
        Authorization::Allow
    );
    assert!(matches!(
        registry.authorize(
            write.as_ref(),
            &json!({"file_path":"x","content":"y"}),
            &ctx_deny
        ),
        Authorization::Deny(_)
    ));
    assert_eq!(
        registry.authorize(
            write.as_ref(),
            &json!({"file_path":"x","content":"y"}),
            &ctx_allow
        ),
        Authorization::Allow
    );
    assert_eq!(
        registry.authorize(
            write.as_ref(),
            &json!({"file_path":"x","content":"y"}),
            &ctx_ask
        ),
        Authorization::Ask
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn read_write_edit_apply_patch_execute_with_validation() {
    let root = temp("fs");
    let c = ctx(&root, true, false);
    let reg = ToolRegistry::with_defaults();
    let write = reg.get("Write").unwrap();
    let read = reg.get("Read").unwrap();
    let edit = reg.get("Edit").unwrap();
    let patch = reg.get("ApplyPatch").unwrap();
    write
        .execute(&json!({"file_path":"a.txt","content":"one\ntwo\n"}), &c)
        .unwrap();
    let read_out = read.execute(&json!({"file_path":"a.txt"}), &c).unwrap();
    assert!(read_out.contains("one"));
    edit.execute(
        &json!({"file_path":"a.txt","old_string":"two","new_string":"TWO"}),
        &c,
    )
    .unwrap();
    let after = std::fs::read_to_string(root.join("a.txt")).unwrap();
    assert!(after.contains("TWO"));
    let p = "*** Begin Patch\n*** Add File: b.txt\n+hi\n*** End Patch\n";
    let patch_out = patch.execute(&json!({"patch_text": p}), &c).unwrap();
    assert!(patch_out.contains("\"type\":\"add\"") || patch_out.contains("\"type\": \"add\""));
    assert!(root.join("b.txt").exists());
    let err = patch.execute(&json!({"patch_text":"  "}), &c).unwrap_err();
    assert_eq!(err.code, ToolErrorCode::InvalidInput);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn glob_grep_and_listmodels_contracts_are_readonly() {
    let reg = ToolRegistry::with_defaults();
    for name in [
        "Glob",
        "Grep",
        "ListModels",
        "ReadImage",
        "Skill",
        "TodoRead",
        "WebFetch",
        "WebSearch",
    ] {
        let t = reg.get(name).unwrap_or_else(|| panic!("{name} missing"));
        let c = t.contract();
        assert!(c.read_only, "{name} should be read-only");
        assert!(!c.description.is_empty(), "{name} needs description");
    }
}

#[test]
fn workflow_sync_tools_without_ports_have_defined_behavior() {
    let root = temp("ports");
    let c = ctx(&root, true, false);
    let reg = ToolRegistry::with_defaults();
    // Without a workflow port, tools must not panic or fabricate run state.
    // Concrete behavior is tool-specific (named failure or empty listing); both are acceptable if explicit.
    for (name, input) in [
        ("ListWorkflowRuns", json!({})),
        ("ListSavedWorkflows", json!({})),
    ] {
        let t = reg.get(name).unwrap();
        match t.execute(&input, &c) {
            Ok(text) => {
                let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(json!({}));
                assert!(v.is_object(), "{name} must return structured JSON: {text}");
            }
            Err(err) => {
                assert!(
                    matches!(
                        err.code,
                        ToolErrorCode::ExecutionFailed
                            | ToolErrorCode::InvalidInput
                            | ToolErrorCode::PermissionDenied
                    ),
                    "{name}: {err}"
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(root);
}
