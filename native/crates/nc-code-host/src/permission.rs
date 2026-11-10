//! Permission service (ported/hardened from TS core/src/permission).
//!
//! Provides rule-based permission checking and decision making for tool execution.
//! Hardening vs the initial stub:
//! - explicit `deny`/`ask`/`allow` behaviors with structured reasons/rule IDs
//! - disallowed/allowed tool lists
//! - collaboration modes (build/edit/plan/yolo/auto)
//! - plan-mode transition handling and read-only enforcement
//! - alwaysAsk with blocking-first semantics (disallowed/deny rules still win)
//! - session-scoped allow grants ("always allow in this session")
//! - project ruleset matching (allow/ask/deny) with wildcard/prefix subjects
//! - capability-aware risk/side-effect resolution
//!
//! This is the host-side policy layer. Interactive approval UX remains the
//! responsibility of the caller (headless callers must treat `ask` as denied
//! unless they have an explicit approval mechanism).

pub use nc_code_core::permission::{
    matches_rule, resolve_plan_mode_transition, rule_subjects, CollaborationMode,
    PermissionBehavior, PermissionConfig, PermissionDecisionResult, PermissionRuleValue,
    PermissionRuleset, RiskLevel, SideEffectScope,
};

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

/// Tool capability metadata used for permission decisions.
#[derive(Debug, Clone, Default)]
pub struct PermissionToolCapability {
    pub allowed_in_plan_mode: bool,
    pub always_ask: bool,
    pub read_only: bool,
    pub destructive: bool,
    pub requires_user_interaction: bool,
    pub side_effect_scope: Option<SideEffectScope>,
    pub risk_level: Option<RiskLevel>,
    pub needs_approval: Option<bool>,
    pub permission_name: Option<String>,
}

/// Input context for a permission check.
#[derive(Debug, Clone)]
pub struct PermissionContext {
    pub tool_name: String,
    pub input: serde_json::Value,
    pub mode: CollaborationMode,
    pub plan_enabled: Option<bool>,
    pub working_directory: Option<String>,
}

impl PermissionContext {
    pub fn new(tool_name: impl Into<String>, input: serde_json::Value) -> Self {
        Self {
            tool_name: tool_name.into(),
            input,
            mode: CollaborationMode::Build,
            plan_enabled: None,
            working_directory: None,
        }
    }

    pub fn with_mode(mut self, mode: CollaborationMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_plan_enabled(mut self, enabled: bool) -> Self {
        self.plan_enabled = Some(enabled);
        self
    }
}

#[derive(Debug, Clone)]
struct ResolvedCapability {
    allowed_in_plan_mode: bool,
    always_ask: bool,
    read_only: bool,
    destructive: bool,
    requires_user_interaction: bool,
    side_effect_scope: SideEffectScope,
    risk_level: RiskLevel,
    needs_approval: bool,
    permission_name: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct PermissionServiceConfig {
    pub allowed_tools: BTreeSet<String>,
    pub disallowed_tools: BTreeSet<String>,
    pub auto_approve_high_risk: bool,
    /// Preapproved WebFetch URL prefixes/domains handled by caller policy; when
    /// true and tool is WebFetch with a URL, host may still require explicit
    /// allowlisting. Kept explicit (no hidden network trust).
    pub allow_webfetch_without_prompt: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PermissionService {
    config: PermissionServiceConfig,
    session_rules: Arc<RwLock<PermissionRuleset>>,
}

impl PermissionService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_config(config: PermissionServiceConfig) -> Self {
        Self {
            config,
            session_rules: Arc::new(RwLock::new(PermissionRuleset::default())),
        }
    }

    pub fn grant_session_allow(&self, tool_name: impl Into<String>, rule_content: Option<String>) {
        if let Ok(mut rules) = self.session_rules.write() {
            rules.allow.push(PermissionRuleValue {
                tool_name: tool_name.into(),
                rule_content,
            });
        }
    }

    /// Backward-compatible simple check (tool name + input only).
    /// Hardened default: non-read-only tools require approval in build mode
    /// unless explicitly allowed; read-only tools are allowed.
    pub fn check_permission(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> PermissionDecisionResult {
        let ctx = PermissionContext::new(tool_name, input.clone());
        self.check(&ctx, None, None)
    }

    pub fn check(
        &self,
        context: &PermissionContext,
        tool_capability: Option<&PermissionToolCapability>,
        project_rules: Option<&PermissionRuleset>,
    ) -> PermissionDecisionResult {
        let capability = self.resolve_capability(context, tool_capability);

        if let Some(decision) =
            resolve_plan_mode_transition(&context.tool_name, context.mode, context.plan_enabled)
        {
            return decision;
        }

        if capability.requires_user_interaction {
            if self.config.disallowed_tools.contains(&context.tool_name) {
                return self.deny(
                    context,
                    &capability,
                    "rule.disallowedTools",
                    format!("Tool {} is explicitly disallowed", context.tool_name),
                );
            }
            return self.ask(
                context,
                &capability,
                "tool.userInteraction",
                format!("Tool {} requires user interaction", context.tool_name),
                false,
            );
        }

        if capability.always_ask {
            return self.check_always_ask(context, &capability, project_rules);
        }

        let plan_enabled = context
            .plan_enabled
            .unwrap_or(context.mode == CollaborationMode::Plan);

        if context.mode == CollaborationMode::Yolo && !plan_enabled {
            return self.allow(
                context,
                &capability,
                "mode.yolo",
                "Yolo mode bypasses permission prompts",
            );
        }

        if context.mode == CollaborationMode::Auto {
            return self.deny(
                context,
                &capability,
                "mode.auto.unimplemented",
                "Auto mode is reserved but not implemented yet",
            );
        }

        if self.config.disallowed_tools.contains(&context.tool_name) {
            return self.deny(
                context,
                &capability,
                "rule.disallowedTools",
                format!("Tool {} is explicitly disallowed", context.tool_name),
            );
        }

        if self.matches_project_rules(project_rules, PermissionBehavior::Deny, context) {
            return self.deny(
                context,
                &capability,
                "rule.project.deny",
                format!(
                    "Tool {} is denied by project permission rules",
                    context.tool_name
                ),
            );
        }

        if self.matches_project_rules(project_rules, PermissionBehavior::Ask, context) {
            return self.ask(
                context,
                &capability,
                "rule.project.ask",
                format!(
                    "Tool {} requires approval by project permission rules",
                    context.tool_name
                ),
                false,
            );
        }

        if plan_enabled || context.mode == CollaborationMode::Plan {
            return self.check_plan_mode(context, &capability);
        }

        if self.matches_project_rules(project_rules, PermissionBehavior::Allow, context) {
            return self.allow(
                context,
                &capability,
                "rule.project.allow",
                format!(
                    "Tool {} is allowed by project permission rules",
                    context.tool_name
                ),
            );
        }

        if self.config.allowed_tools.contains(&context.tool_name) {
            return self.allow(
                context,
                &capability,
                "rule.allowedTools",
                format!("Tool {} is explicitly allowed", context.tool_name),
            );
        }

        if context.mode == CollaborationMode::Edit {
            return self.check_edit_mode(context, &capability);
        }

        self.check_build_mode(context, &capability)
    }

    fn check_always_ask(
        &self,
        context: &PermissionContext,
        capability: &ResolvedCapability,
        project_rules: Option<&PermissionRuleset>,
    ) -> PermissionDecisionResult {
        if context.mode == CollaborationMode::Auto {
            return self.deny(
                context,
                capability,
                "mode.auto.unimplemented",
                "Auto mode is reserved but not implemented yet",
            );
        }
        if self.config.disallowed_tools.contains(&context.tool_name) {
            return self.deny(
                context,
                capability,
                "rule.disallowedTools",
                format!("Tool {} is explicitly disallowed", context.tool_name),
            );
        }
        if self.matches_project_rules(project_rules, PermissionBehavior::Deny, context) {
            return self.deny(
                context,
                capability,
                "rule.project.deny",
                format!(
                    "Tool {} is denied by project permission rules",
                    context.tool_name
                ),
            );
        }
        if let Ok(session_rules) = self.session_rules.read() {
            if self.matches_ruleset(&session_rules.allow, context) {
                return self.allow(
                    context,
                    capability,
                    "rule.session.allow",
                    format!("Tool {} was allowed for this session", context.tool_name),
                );
            }
        }
        self.ask(
            context,
            capability,
            "tool.alwaysAsk",
            format!(
                "Tool {} always requires explicit approval",
                context.tool_name
            ),
            true,
        )
    }

    fn check_plan_mode(
        &self,
        context: &PermissionContext,
        capability: &ResolvedCapability,
    ) -> PermissionDecisionResult {
        if capability.read_only && !capability.destructive {
            return self.allow(
                context,
                capability,
                "mode.plan.readOnly",
                "Plan mode allows read-only tool execution",
            );
        }
        if capability.permission_name.as_deref() == Some("mcp") && !capability.destructive {
            return self.allow(
                context,
                capability,
                "mode.plan.mcp",
                "Plan mode allows non-destructive MCP tool execution",
            );
        }
        if capability.allowed_in_plan_mode
            && capability.side_effect_scope == SideEffectScope::Session
            && !capability.destructive
            && !capability.needs_approval
        {
            return self.allow(
                context,
                capability,
                "mode.plan.explicitSessionCapability",
                "Plan mode allows this explicit non-destructive session control action",
            );
        }
        self.deny(
            context,
            capability,
            "mode.plan.nonReadOnly",
            "Plan mode only allows read-only, non-destructive tools",
        )
    }

    fn check_build_mode(
        &self,
        context: &PermissionContext,
        capability: &ResolvedCapability,
    ) -> PermissionDecisionResult {
        if capability.read_only && !capability.destructive && !capability.needs_approval {
            return self.allow(
                context,
                capability,
                "mode.build.readOnly",
                "Build mode allows read-only tools",
            );
        }
        if capability.risk_level == RiskLevel::Critical {
            return self.ask(
                context,
                capability,
                "mode.build.criticalRisk",
                "Critical risk tools require explicit approval",
                false,
            );
        }
        if capability.risk_level == RiskLevel::High && !self.config.auto_approve_high_risk {
            return self.ask(
                context,
                capability,
                "mode.build.highRisk",
                "High risk tools require explicit approval",
                false,
            );
        }
        if capability.side_effect_scope == SideEffectScope::Session
            && capability.risk_level == RiskLevel::Low
            && !capability.destructive
            && !capability.needs_approval
        {
            return self.allow(
                context,
                capability,
                "mode.build.sessionState",
                "Build mode allows low-risk session-local state updates",
            );
        }
        if capability.needs_approval
            || capability.destructive
            || capability.side_effect_scope != SideEffectScope::None
        {
            return self.ask(
                context,
                capability,
                "mode.build.sideEffect",
                "Tool has side effects and requires approval",
                false,
            );
        }
        self.allow(
            context,
            capability,
            "mode.build.lowRisk",
            "Build mode allows low-risk tool execution",
        )
    }

    fn check_edit_mode(
        &self,
        context: &PermissionContext,
        capability: &ResolvedCapability,
    ) -> PermissionDecisionResult {
        if capability.permission_name.as_deref() == Some("edit")
            && capability.side_effect_scope == SideEffectScope::Workspace
        {
            return self.allow(
                context,
                capability,
                "mode.edit.fileEdit",
                "Edit mode allows file edit tools",
            );
        }
        self.check_build_mode(context, capability)
    }

    fn resolve_capability(
        &self,
        context: &PermissionContext,
        tool_capability: Option<&PermissionToolCapability>,
    ) -> ResolvedCapability {
        let read_only_default = is_read_only_tool(&context.tool_name);
        let destructive_default = is_destructive_tool(&context.tool_name);
        let side_effect_default = if read_only_default {
            SideEffectScope::None
        } else {
            SideEffectScope::Workspace
        };
        let read_only = tool_capability
            .map(|c| c.read_only)
            .unwrap_or(read_only_default);
        let destructive = tool_capability
            .map(|c| c.destructive)
            .unwrap_or(destructive_default);
        let risk_level = tool_capability
            .and_then(|c| c.risk_level)
            .unwrap_or_else(|| default_risk_level(&context.tool_name, read_only, destructive));
        let side_effect_scope = tool_capability
            .and_then(|c| c.side_effect_scope)
            .unwrap_or(side_effect_default);
        let needs_approval = tool_capability
            .and_then(|c| c.needs_approval)
            .unwrap_or(!read_only || destructive);
        ResolvedCapability {
            allowed_in_plan_mode: tool_capability
                .map(|c| c.allowed_in_plan_mode)
                .unwrap_or(false),
            always_ask: tool_capability.map(|c| c.always_ask).unwrap_or(false),
            read_only,
            destructive,
            requires_user_interaction: tool_capability
                .map(|c| c.requires_user_interaction)
                .unwrap_or(side_effect_scope == SideEffectScope::UserInteraction),
            side_effect_scope,
            risk_level,
            needs_approval,
            permission_name: tool_capability.and_then(|c| c.permission_name.clone()),
        }
    }

    fn matches_project_rules(
        &self,
        ruleset: Option<&PermissionRuleset>,
        behavior: PermissionBehavior,
        context: &PermissionContext,
    ) -> bool {
        let Some(ruleset) = ruleset else {
            return false;
        };
        let rules = match behavior {
            PermissionBehavior::Allow => &ruleset.allow,
            PermissionBehavior::Ask => &ruleset.ask,
            PermissionBehavior::Deny => &ruleset.deny,
        };
        self.matches_ruleset(rules, context)
    }

    fn matches_ruleset(&self, rules: &[PermissionRuleValue], context: &PermissionContext) -> bool {
        rules
            .iter()
            .any(|rule| matches_rule(rule, &context.tool_name, &context.input, true))
    }

    fn allow(
        &self,
        context: &PermissionContext,
        capability: &ResolvedCapability,
        rule_id: &str,
        reason: impl Into<String>,
    ) -> PermissionDecisionResult {
        PermissionDecisionResult::allow(
            context.mode,
            rule_id,
            reason,
            capability.risk_level,
            Some(capability.side_effect_scope),
        )
    }

    fn ask(
        &self,
        context: &PermissionContext,
        capability: &ResolvedCapability,
        rule_id: &str,
        reason: impl Into<String>,
        always_ask: bool,
    ) -> PermissionDecisionResult {
        PermissionDecisionResult::ask(
            context.mode,
            rule_id,
            reason,
            capability.risk_level,
            Some(capability.side_effect_scope),
            always_ask,
        )
    }

    fn deny(
        &self,
        context: &PermissionContext,
        capability: &ResolvedCapability,
        rule_id: &str,
        reason: impl Into<String>,
    ) -> PermissionDecisionResult {
        PermissionDecisionResult::deny(
            context.mode,
            rule_id,
            reason,
            capability.risk_level,
            Some(capability.side_effect_scope),
        )
    }
}

fn is_read_only_tool(name: &str) -> bool {
    matches!(
        name,
        "Read"
            | "Glob"
            | "Grep"
            | "WebSearch"
            | "WebFetch"
            | "TodoRead"
            | "AskUserQuestion"
            | "Skill"
            | "ls"
            | "find"
            | "cat"
            | "head"
            | "tail"
    )
}

fn is_destructive_tool(name: &str) -> bool {
    matches!(name, "Bash" | "shell" | "exec" | "delete" | "rm")
}

fn default_risk_level(name: &str, read_only: bool, destructive: bool) -> RiskLevel {
    if destructive {
        return RiskLevel::High;
    }
    if read_only || is_read_only_tool(name) {
        return RiskLevel::Low;
    }
    if matches!(name, "Write" | "Edit" | "ApplyPatch" | "Bash" | "shell") {
        return RiskLevel::Medium;
    }
    RiskLevel::Medium
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn read_only_tools_allowed_by_default_in_build() {
        let service = PermissionService::new();
        let result = service.check_permission("Read", &json!({"file_path":"a.txt"}));
        assert_eq!(result.decision, PermissionBehavior::Allow);
        assert!(result.allowed);
        assert_eq!(result.rule_id, "mode.build.readOnly");
    }

    #[test]
    fn side_effect_tools_ask_by_default_in_build() {
        let service = PermissionService::new();
        let result = service.check_permission("Write", &json!({"path":"a.txt"}));
        assert_eq!(result.decision, PermissionBehavior::Ask);
        assert!(result.escalated);
    }

    #[test]
    fn plan_mode_denies_non_readonly() {
        let service = PermissionService::new();
        let ctx = PermissionContext::new("Write", json!({"path":"a.txt"}))
            .with_mode(CollaborationMode::Plan);
        let result = service.check(&ctx, None, None);
        assert_eq!(result.decision, PermissionBehavior::Deny);
        assert_eq!(result.rule_id, "mode.plan.nonReadOnly");
    }

    #[test]
    fn plan_mode_allows_readonly() {
        let service = PermissionService::new();
        let ctx = PermissionContext::new("Read", json!({"file_path":"a.txt"}))
            .with_mode(CollaborationMode::Plan);
        let result = service.check(&ctx, None, None);
        assert_eq!(result.decision, PermissionBehavior::Allow);
    }

    #[test]
    fn disallowed_tool_denied_even_if_readonly() {
        let mut config = PermissionServiceConfig::default();
        config.disallowed_tools.insert("Read".to_string());
        let service = PermissionService::with_config(config);
        let result = service.check_permission("Read", &json!({}));
        assert_eq!(result.decision, PermissionBehavior::Deny);
        assert_eq!(result.rule_id, "rule.disallowedTools");
    }

    #[test]
    fn project_deny_overrides_allow() {
        let service = PermissionService::new();
        let ctx = PermissionContext::new("Bash", json!({"command":"rm -rf /"}));
        let rules = PermissionRuleset {
            deny: vec![PermissionRuleValue {
                tool_name: "Bash".to_string(),
                rule_content: Some("rm:*".to_string()),
            }],
            ..Default::default()
        };
        let result = service.check(&ctx, None, Some(&rules));
        assert_eq!(result.decision, PermissionBehavior::Deny);
        assert_eq!(result.rule_id, "rule.project.deny");
    }

    #[test]
    fn always_ask_still_respects_disallowed() {
        let mut config = PermissionServiceConfig::default();
        config.disallowed_tools.insert("Danger".to_string());
        let service = PermissionService::with_config(config);
        let ctx = PermissionContext::new("Danger", json!({}));
        let cap = PermissionToolCapability {
            always_ask: true,
            ..Default::default()
        };
        let result = service.check(&ctx, Some(&cap), None);
        assert_eq!(result.decision, PermissionBehavior::Deny);
        assert_ne!(result.rule_id, "tool.alwaysAsk");
    }

    #[test]
    fn session_grant_allows_always_ask_tool() {
        let service = PermissionService::new();
        service.grant_session_allow("AskTool", None);
        let ctx = PermissionContext::new("AskTool", json!({}));
        let cap = PermissionToolCapability {
            always_ask: true,
            read_only: false,
            needs_approval: Some(true),
            ..Default::default()
        };
        let result = service.check(&ctx, Some(&cap), None);
        assert_eq!(result.decision, PermissionBehavior::Allow);
        assert_eq!(result.rule_id, "rule.session.allow");
    }

    #[test]
    fn yolo_bypasses_prompts_outside_plan() {
        let service = PermissionService::new();
        let ctx = PermissionContext::new("Bash", json!({"command":"echo hi"}))
            .with_mode(CollaborationMode::Yolo);
        let result = service.check(&ctx, None, None);
        assert_eq!(result.decision, PermissionBehavior::Allow);
        assert_eq!(result.rule_id, "mode.yolo");
    }

    #[test]
    fn auto_mode_denied_as_unimplemented() {
        let service = PermissionService::new();
        let ctx = PermissionContext::new("Read", json!({})).with_mode(CollaborationMode::Auto);
        let result = service.check(&ctx, None, None);
        assert_eq!(result.decision, PermissionBehavior::Deny);
        assert_eq!(result.rule_id, "mode.auto.unimplemented");
    }
}
