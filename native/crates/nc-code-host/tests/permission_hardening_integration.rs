//! Integration tests for permission hardening (nc-code-host + nc-code-core primitives).
//! No mocks/stubs: exercises real `PermissionService` and rule evaluation.

use nc_code_host::permission::{
    matches_rule, CollaborationMode, PermissionBehavior, PermissionContext, PermissionRuleValue,
    PermissionRuleset, PermissionService, PermissionServiceConfig, PermissionToolCapability,
    SideEffectScope,
};
use serde_json::json;

#[test]
fn core_rule_matching_and_host_service_agree_on_deny() {
    let rule = PermissionRuleValue {
        tool_name: "Bash".to_string(),
        rule_content: Some("git push:*".to_string()),
    };
    assert!(matches_rule(
        &rule,
        "Bash",
        &json!({"command":"git push origin main"}),
        true
    ));

    let service = PermissionService::new();
    let ctx = PermissionContext::new("Bash", json!({"command":"git push origin main"}));
    let rules = PermissionRuleset {
        deny: vec![rule],
        ..Default::default()
    };
    let result = service.check(&ctx, None, Some(&rules));
    assert_eq!(result.decision, PermissionBehavior::Deny);
    assert!(!result.allowed);
}

#[test]
fn plan_mode_blocks_workspace_write_but_allows_read() {
    let service = PermissionService::new();
    let read = service.check(
        &PermissionContext::new("Read", json!({"file_path":"Cargo.toml"}))
            .with_mode(CollaborationMode::Plan),
        None,
        None,
    );
    assert_eq!(read.decision, PermissionBehavior::Allow);

    let write = service.check(
        &PermissionContext::new("Write", json!({"path":"x","content":"y"}))
            .with_mode(CollaborationMode::Plan),
        None,
        None,
    );
    assert_eq!(write.decision, PermissionBehavior::Deny);
    assert!(write.reason.unwrap_or_default().contains("read-only"));
}

#[test]
fn explicit_allow_list_can_permit_configured_tool() {
    let mut config = PermissionServiceConfig::default();
    config.allowed_tools.insert("CustomRead".to_string());
    let service = PermissionService::with_config(config);
    let cap = PermissionToolCapability {
        read_only: true,
        needs_approval: Some(false),
        ..Default::default()
    };
    let result = service.check(
        &PermissionContext::new("CustomRead", json!({})),
        Some(&cap),
        None,
    );
    assert_eq!(result.decision, PermissionBehavior::Allow);
}

#[test]
fn always_ask_marks_decision_and_session_grant_clears_prompt() {
    let service = PermissionService::new();
    let cap = PermissionToolCapability {
        always_ask: true,
        read_only: false,
        needs_approval: Some(true),
        ..Default::default()
    };
    let asked = service.check(
        &PermissionContext::new("Sensitive", json!({})),
        Some(&cap),
        None,
    );
    assert_eq!(asked.decision, PermissionBehavior::Ask);
    assert_eq!(asked.always_ask, Some(true));

    service.grant_session_allow("Sensitive", None);
    let allowed = service.check(
        &PermissionContext::new("Sensitive", json!({})),
        Some(&cap),
        None,
    );
    assert_eq!(allowed.decision, PermissionBehavior::Allow);
}

#[test]
fn webfetch_domain_rule_matches_url_input() {
    let service = PermissionService::new();
    let rules = PermissionRuleset {
        allow: vec![PermissionRuleValue {
            tool_name: "WebFetch".to_string(),
            rule_content: Some("domain:docs.rs".to_string()),
        }],
        ..Default::default()
    };
    let result = service.check(
        &PermissionContext::new("WebFetch", json!({"url":"https://docs.rs/tokio/latest"})),
        Some(&PermissionToolCapability {
            read_only: true,
            needs_approval: Some(true),
            side_effect_scope: Some(SideEffectScope::Network),
            ..Default::default()
        }),
        Some(&rules),
    );
    assert_eq!(result.decision, PermissionBehavior::Allow);
    assert_eq!(result.rule_id, "rule.project.allow");
}
