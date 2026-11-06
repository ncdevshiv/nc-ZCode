//! Permission primitives shared by nc-code-host (ported from TS core/src/permission).
//!
//! This module contains portable, host-agnostic logic: behavior/result types,
//! wildcard rule matching, rule subject extraction, and small helpers. The full
//! mode-aware `PermissionService` lives in `nc-code-host` because it depends on
//! host/tool contracts and project configuration.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Decision behavior (TS `PermissionBehavior`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionBehavior {
    Allow,
    Ask,
    Deny,
}

/// Risk level aligned with TS contracts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RiskLevel {
    Low,
    #[default]
    Medium,
    High,
    Critical,
}

/// Side-effect scope aligned with TS contracts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SideEffectScope {
    #[default]
    None,
    Workspace,
    Git,
    Network,
    System,
    Session,
    UserInteraction,
}

/// Collaboration mode (subset used by host; TS has more contract wiring).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CollaborationMode {
    #[default]
    Build,
    Edit,
    Plan,
    Yolo,
    Auto,
}

/// Structured permission decision (TS `PermissionDecisionResult` shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionDecisionResult {
    pub decision: PermissionBehavior,
    pub allowed: bool,
    pub escalated: bool,
    pub mode: CollaborationMode,
    pub rule_id: String,
    pub risk_level: RiskLevel,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side_effect_scope: Option<SideEffectScope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub always_ask: Option<bool>,
}

impl PermissionDecisionResult {
    pub fn allow(
        mode: CollaborationMode,
        rule_id: impl Into<String>,
        reason: impl Into<String>,
        risk_level: RiskLevel,
        side_effect_scope: Option<SideEffectScope>,
    ) -> Self {
        Self {
            decision: PermissionBehavior::Allow,
            allowed: true,
            escalated: false,
            mode,
            rule_id: rule_id.into(),
            risk_level,
            reason: Some(reason.into()),
            side_effect_scope,
            always_ask: None,
        }
    }

    pub fn ask(
        mode: CollaborationMode,
        rule_id: impl Into<String>,
        reason: impl Into<String>,
        risk_level: RiskLevel,
        side_effect_scope: Option<SideEffectScope>,
        always_ask: bool,
    ) -> Self {
        Self {
            decision: PermissionBehavior::Ask,
            allowed: false,
            escalated: true,
            mode,
            rule_id: rule_id.into(),
            risk_level,
            reason: Some(reason.into()),
            side_effect_scope,
            always_ask: if always_ask { Some(true) } else { None },
        }
    }

    pub fn deny(
        mode: CollaborationMode,
        rule_id: impl Into<String>,
        reason: impl Into<String>,
        risk_level: RiskLevel,
        side_effect_scope: Option<SideEffectScope>,
    ) -> Self {
        Self {
            decision: PermissionBehavior::Deny,
            allowed: false,
            escalated: false,
            mode,
            rule_id: rule_id.into(),
            risk_level,
            reason: Some(reason.into()),
            side_effect_scope,
            always_ask: None,
        }
    }
}

/// A single permission rule value (portable subset of TS `PermissionRuleValue`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRuleValue {
    pub tool_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_content: Option<String>,
}

/// Simple ruleset grouped by behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PermissionRuleset {
    #[serde(default)]
    pub allow: Vec<PermissionRuleValue>,
    #[serde(default)]
    pub ask: Vec<PermissionRuleValue>,
    #[serde(default)]
    pub deny: Vec<PermissionRuleValue>,
}

/// Portable config for tool allow/deny lists.
#[derive(Debug, Clone, Default)]
pub struct PermissionConfig {
    pub allowed_tools: BTreeSet<String>,
    pub disallowed_tools: BTreeSet<String>,
    pub auto_approve_high_risk: bool,
}

/// Convert a simple wildcard pattern (`*`) to an anchored regex.
/// Mirrors TS `wildcardToRegExp`.
pub fn wildcard_to_regex(pattern: &str) -> Result<regex::Regex, regex::Error> {
    let mut escaped = String::with_capacity(pattern.len() + 8);
    escaped.push('^');
    // Treat `*` as matching any char except path separators (to align with TS glob-like rule matching
    // in the tested cases, e.g. src/*.rs should not match src/lib/mod.rs).
    for (i, part) in pattern.split('*').enumerate() {
        if i > 0 {
            escaped.push_str(r"[^/\\]*");
        }
        escaped.push_str(&regex::escape(part));
    }
    escaped.push('$');
    regex::Regex::new(&escaped)
}

/// Extract `domain:<hostname>` subject for WebFetch-like URLs.
pub fn webfetch_domain_subject(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url.trim()).ok()?;
    let host = parsed
        .host_str()?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if host.is_empty() {
        None
    } else {
        Some(format!("domain:{host}"))
    }
}

/// Extract candidate rule subjects from tool input (portable heuristic).
pub fn rule_subjects(tool_name: &str, input: &serde_json::Value) -> Vec<String> {
    if let Some(s) = input.as_str() {
        return vec![s.to_string()];
    }
    let Some(obj) = input.as_object() else {
        return Vec::new();
    };
    if tool_name == "WebFetch" {
        if let Some(url) = obj.get("url").and_then(|v| v.as_str()) {
            if let Some(subject) = webfetch_domain_subject(url) {
                return vec![subject];
            }
        }
        return Vec::new();
    }
    for key in [
        "command",
        "url",
        "file_path",
        "path",
        "pattern",
        "patch_text",
        "filePath",
    ] {
        if let Some(value) = obj.get(key).and_then(|v| v.as_str()) {
            return vec![value.to_string()];
        }
    }
    Vec::new()
}

/// Match a single rule against tool name and input subjects.
/// `write_implies_edit` mirrors TS `Write` matching `Edit` rules.
pub fn matches_rule(
    rule: &PermissionRuleValue,
    tool_name: &str,
    input: &serde_json::Value,
    write_implies_edit: bool,
) -> bool {
    let name_matches = rule.tool_name == tool_name
        || (write_implies_edit && tool_name == "Write" && rule.tool_name == "Edit");
    if !name_matches {
        return false;
    }
    let Some(content) = rule.rule_content.as_deref() else {
        return true;
    };
    let subjects = rule_subjects(tool_name, input);
    if subjects.is_empty() {
        return false;
    }
    subjects
        .iter()
        .any(|subject| matches_rule_content(subject, content))
}

pub fn matches_rule_content(subject: &str, rule_content: &str) -> bool {
    if let Some(prefix) = rule_content.strip_suffix(":*") {
        return subject == prefix
            || subject.starts_with(&format!("{prefix} "))
            || subject.starts_with(&format!("{prefix}\t"));
    }
    if rule_content.contains('*') {
        return wildcard_to_regex(rule_content)
            .map(|re| re.is_match(subject))
            .unwrap_or(false);
    }
    subject == rule_content
}

pub fn find_matching_rule<'a>(
    rules: &'a [PermissionRuleValue],
    tool_name: &str,
    input: &serde_json::Value,
    write_implies_edit: bool,
) -> Option<&'a PermissionRuleValue> {
    rules
        .iter()
        .find(|rule| matches_rule(rule, tool_name, input, write_implies_edit))
}

/// Resolve plan-mode transition permissions (EnterPlanMode/ExitPlanMode).
pub fn resolve_plan_mode_transition(
    tool_name: &str,
    mode: CollaborationMode,
    plan_enabled: Option<bool>,
) -> Option<PermissionDecisionResult> {
    if tool_name == "EnterPlanMode" {
        return Some(PermissionDecisionResult::allow(
            mode,
            "tool.plan.enter",
            "EnterPlanMode switches to plan mode without a permission prompt",
            RiskLevel::Low,
            Some(SideEffectScope::Session),
        ));
    }
    if tool_name == "ExitPlanMode" {
        let active = plan_enabled.unwrap_or(mode == CollaborationMode::Plan);
        if !active {
            return Some(PermissionDecisionResult::deny(
                mode,
                "mode.plan.exitOnly",
                "ExitPlanMode can only be used while plan mode is active",
                RiskLevel::Low,
                Some(SideEffectScope::Session),
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wildcard_matches_like_ts() {
        assert!(matches_rule_content("cargo test", "cargo:*"));
        assert!(matches_rule_content("cargo", "cargo:*"));
        assert!(!matches_rule_content("cargo-test", "cargo:*"));
        assert!(matches_rule_content("src/main.rs", "src/*.rs"));
        assert!(!matches_rule_content("src/lib/mod.rs", "src/*.rs"));
    }

    #[test]
    fn webfetch_domain_subject_extracted() {
        assert_eq!(
            webfetch_domain_subject("https://Example.COM./path?q=1"),
            Some("domain:example.com".to_string())
        );
        assert_eq!(webfetch_domain_subject("not-a-url"), None);
    }

    #[test]
    fn rule_subjects_handle_webfetch_and_common_keys() {
        assert_eq!(
            rule_subjects("WebFetch", &json!({"url": "https://docs.rs/serde"})),
            vec!["domain:docs.rs".to_string()]
        );
        assert_eq!(
            rule_subjects("Bash", &json!({"command": "ls -la"})),
            vec!["ls -la".to_string()]
        );
        assert_eq!(
            rule_subjects("Read", &json!({"file_path": "a.txt"})),
            vec!["a.txt".to_string()]
        );
    }

    #[test]
    fn write_matches_edit_rule_when_enabled() {
        let rule = PermissionRuleValue {
            tool_name: "Edit".to_string(),
            rule_content: Some("src/*".to_string()),
        };
        assert!(matches_rule(
            &rule,
            "Write",
            &json!({"path":"src/a.rs"}),
            true
        ));
        assert!(!matches_rule(
            &rule,
            "Write",
            &json!({"path":"src/a.rs"}),
            false
        ));
    }

    #[test]
    fn plan_transition_allows_enter_denies_exit_outside_plan() {
        let enter =
            resolve_plan_mode_transition("EnterPlanMode", CollaborationMode::Build, None).unwrap();
        assert_eq!(enter.decision, PermissionBehavior::Allow);
        let exit =
            resolve_plan_mode_transition("ExitPlanMode", CollaborationMode::Build, Some(false))
                .unwrap();
        assert_eq!(exit.decision, PermissionBehavior::Deny);
        assert!(
            resolve_plan_mode_transition("ExitPlanMode", CollaborationMode::Plan, None).is_none()
        );
    }

    #[test]
    fn decision_helpers_set_flags() {
        let ask = PermissionDecisionResult::ask(
            CollaborationMode::Build,
            "r",
            "why",
            RiskLevel::High,
            Some(SideEffectScope::Workspace),
            true,
        );
        assert!(!ask.allowed);
        assert!(ask.escalated);
        assert_eq!(ask.always_ask, Some(true));
    }
}
