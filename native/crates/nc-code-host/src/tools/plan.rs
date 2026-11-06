//! EnterPlanMode / ExitPlanMode: entering and leaving plan mode and "plan approval".
//!
//! Reference implementations (aligned entry by entry, see specs/rust-native-crates.md):
//! - `core/src/tool/handlers/plan-mode.ts` (the two tool entries, the handlers, the two feedback wordings);
//! - `core/src/tool/handlers/plan-mode-prompts.ts` (the provider-visible descriptions);
//! - `core/src/permission/service.ts:checkPlanMode` (plan mode only lets read-only/non-destructive tools through);
//! - `bootstrap/src/nc-code-protocol/interaction-broker.ts` (the approval Q&A and answer normalisation);
//! - `core/src/runtime/helpers/plan-file-continuity.ts` (writing the plan into the workspace after approval).
//!
//! Two deliberate places where nothing is faked:
//! 1. headless (no answerable subscriber) does **not** let ExitPlanMode through — a plan nobody approved is not a plan;
//! 2. the `allowedInPlanMode` TS capability bit is declared by no tool in this host (the only
//!    `respond-to-coordinator` one is not ported), so no permanently-false field is added in advance.
use crate::host_interactions::PlanApproval;
use crate::session::Session;
use crate::tools::{
    AsyncTool, Tool, ToolContext, ToolContract, ToolError, ToolErrorCode, SCOPE_SESSION,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
pub const ENTER_PLAN_MODE_TOOL_NAME: &str = "EnterPlanMode";
pub const EXIT_PLAN_MODE_TOOL_NAME: &str = "ExitPlanMode";
/// TS `EXIT_PLAN_MODE_APPROVAL_QUESTION` (the key used to look answers up).
pub const PLAN_APPROVAL_QUESTION: &str = "Review this implementation plan.";
/// TS `EXIT_PLAN_MODE_APPROVAL_APPROVE`: the only answer value that lets it through.
pub const PLAN_APPROVAL_APPROVE: &str = "approve";
/// The reason of TS `planModePermission("plan.exit", …)`, which is also the text of that question in the v4 projection.
pub const EXIT_PLAN_MODE_REASON: &str =
    "ExitPlanMode changes session mode after user plan approval";
/// The reason of TS `planModePermission("plan.enter", …)`.
pub const ENTER_PLAN_MODE_REASON: &str = "EnterPlanMode changes session mode to plan";
/// TS `PLAN_MODE_MAX_PLAN_CHARS`.
const MAX_PLAN_CHARS: usize = 20_000;
/// TS `MAX_PLAN_MODE_MODEL_BYTES`.
const MAX_PLAN_MODEL_BYTES: usize = 100_000;
/// The wording TS `formatPlanMode` returns when a non-plan-mode session calls ExitPlanMode.
const NOT_IN_PLAN_MODE: &str = "ExitPlanMode can only be used while plan mode is active. If your plan was already approved, continue with implementation.";
const ENTER_PLAN_MODEL_MESSAGE: &str =
    "Entered plan mode. You should now focus on exploring the codebase and designing an implementation approach.";
/// TS `createEnterPlanModeProviderDescription` (the `embeddedSearchEnabled: false` tier:
/// this host's exploration tools are exactly Glob/Grep/Read, there is no `find`).
const ENTER_PLAN_MODE_DESCRIPTION: &str = r#"Use this tool proactively when you're about to start a non-trivial implementation task. Getting user sign-off on your approach before writing code prevents wasted effort and ensures alignment. This tool transitions you into plan mode where you can explore the codebase and design an implementation approach for user approval.
## When to Use This Tool
**Prefer using EnterPlanMode** for implementation tasks unless they're simple. Use it when ANY of these conditions apply:
1. **New Feature Implementation**: Adding meaningful new functionality
   - Example: "Add a logout button" - where should it go? What should happen on click?
   - Example: "Add form validation" - what rules? What error messages?
2. **Multiple Valid Approaches**: The task can be solved in several different ways
   - Example: "Add caching to the API" - could use Redis, in-memory, file-based, etc.
   - Example: "Improve performance" - many optimization strategies possible3. **Code Modifications**: Changes that affect existing behavior or structure
   - Example: "Update the login flow" - what exactly should change?
   - Example: "Refactor this component" - what's the target architecture?
4. **Architectural Decisions**: The task requires choosing between patterns or technologies
   - Example: "Add real-time updates" - WebSockets vs SSE vs polling
   - Example: "Implement state management" - Redux vs Context vs custom solution5. **Multi-File Changes**: The task will likely touch more than 2-3 files
   - Example: "Refactor the authentication system"
   - Example: "Add a new API endpoint with tests"
6. **Unclear Requirements**: You need to explore before understanding the full scope
   - Example: "Make the app faster" - need to profile and identify bottlenecks
   - Example: "Fix the bug in checkout" - need to investigate root cause7. **User Preferences Matter**: The implementation could reasonably go multiple ways
   - If you would use AskUserQuestion to clarify the approach, use EnterPlanMode instead
   - Plan mode lets you explore first, then present options with context
## When NOT To Use This ToolOnly skip EnterPlanMode for simple tasks:
- Single-line or few-line fixes (typos, obvious bugs, small tweaks)
- Adding a single function with clear requirements
- Tasks where the user has given very specific, detailed instructions
- Pure research/exploration tasks (use the Agent tool instead)
## What Happens in Plan ModeIn plan mode, you'll:
1. Thoroughly explore the codebase using Glob, Grep, and Read2. Understand existing patterns and architecture3. Design an implementation approach4. Present your plan to the user for approval5. Use AskUserQuestion if you need to clarify approaches6. Exit plan mode with ExitPlanMode when ready to implement
## Examples
### GOOD - Use EnterPlanMode:
User: "Add user authentication to the app"
- Requires architectural decisions (session vs JWT, where to store tokens, middleware structure)
User: "Optimize the database queries"
- Multiple approaches possible, need to profile first, significant impactUser: "Implement dark mode"
- Architectural decision on theme system, affects many componentsUser: "Add a delete button to the user profile"
- Seems simple but involves: where to place it, confirmation dialog, API call, error handling, state updatesUser: "Update the error handling in the API"
- Affects multiple files, user should approve the approach
### BAD - Don't use EnterPlanMode:
User: "Fix the typo in the README"
- Straightforward, no planning neededUser: "Add a console.log to debug this function"
- Simple, obvious implementationUser: "What files handle routing?"
- Research task, not implementation planning
## Important Notes
- This tool REQUIRES user approval - they must consent to entering plan mode
- If unsure whether to use it, err on the side of planning - it's better to get alignment upfront than to redo work
- Users appreciate being consulted before significant changes are made to their codebase
"#;
/// TS `EXIT_PLAN_MODE_MODEL_INSTRUCTIONS[0]` (the provider-visible description).
const EXIT_PLAN_MODE_DESCRIPTION: &str = r#"Use this tool when you are in plan mode and have finished writing your plan and are ready for user approval.
## How This Tool Works
- You should have already explored the codebase and finalized the plan you want the user to review
- This tool DOES take the plan content as the required plan parameter in ZCode
- Pass the complete plan in the plan field; the user will review that content before approving implementation
- This tool simply signals that you're done planning and ready for the user to review and approve
- The user will see the contents of the plan parameter when they review it
## When to Use This ToolIMPORTANT: Only use this tool when the task requires planning the implementation steps of a task that requires writing code. For research tasks where you're gathering information, searching files, reading files or in general trying to understand the codebase - do NOT use this tool.
## Before Using This ToolEnsure your plan is complete and unambiguous:
- If you have unresolved questions about requirements or approach, use AskUserQuestion before finalizing your plan
- Once your plan is finalized, use THIS tool to request approval
**Important:** Do NOT use AskUserQuestion to ask "Is this plan okay?" or "Should I proceed?" - that's exactly what THIS tool does. ExitPlanMode inherently requests user approval of your plan.
## Examples1. Initial task: "Search for and understand the implementation of vim mode in the codebase" - Do not use the exit plan mode tool because you are not planning the implementation steps of a task.
2. Initial task: "Help me implement yank mode for vim" - Use the exit plan mode tool after you have finished planning the implementation steps.
3. Initial task: "Add a new feature to handle user authentication" - If unsure about auth method (OAuth, JWT, etc.), use AskUserQuestion first, then use exit plan mode tool after clarifying the approach.
"#;
/// The v4 payload shape of the approval Q&A (TS `createExitPlanModeApprovalQuestion(reason)`:
/// the question text is exactly the reason, the only option is approve, and the rest is expressed through freeText as change requests).
pub fn approval_questions(reason: &str) -> Vec<Value> {
    vec![json!({
        "question": reason,
        "header": "Plan",
        "options": [{
            "value": PLAN_APPROVAL_APPROVE,
            "label": "Approve",
            "description": "Exit plan mode and start implementation.",
        }],
    })]
}
/// Answer → the approval outcome. Corresponds to TS entry by entry:
/// `v4AnswerToUserInputResponse` (with an `action` it follows accept/decline/cancel; without one,
/// freeText/optionId lands in the `content.answer` compatibility slot) +
/// `normalizePlanApprovalAnswer` (`content.answers[question] ?? answer_0 ?? answer`) +
/// `userInputResponseToBrokerResult` (accept with the answer exactly approve → allow;
/// any other non-empty answer → deny using the answer as the reason; no answer → deny).
pub fn approval_from_answer(answer: &Value) -> PlanApproval {
    let action = answer.get("action").and_then(Value::as_str);
    if matches!(action, Some("decline") | Some("cancel")) {
        return PlanApproval::Declined;
    }
    let content = answer.get("content").filter(|value| value.is_object());
    let raw = content
        .and_then(|value| {
            let answers = value.get("answers").filter(|items| items.is_object());
            let keyed = answers
                .as_ref()
                .and_then(|items| items.get(PLAN_APPROVAL_QUESTION))
                .cloned();
            keyed
                .or_else(|| value.get("answer_0").cloned())
                .or_else(|| {
                    // The single-question path: `content.answer`, plus the freeText/optionId v4 puts directly at the top level.
                    value.get("answer").cloned().or_else(|| {
                        answer
                            .get("freeText")
                            .and_then(Value::as_str)
                            .map(|text| json!(text))
                    })
                })
        })
        .or_else(|| {
            // The older shape without content: optionId/freeText is itself the answer.
            answer
                .get("freeText")
                .and_then(Value::as_str)
                .map(|text| json!(text))
                .or_else(|| answer.get("optionId").cloned())
        });
    // Normalisation shares the one function with AskUserQuestion (array → joined with ", ", an empty string after trim is dropped),
    // so no second closed form of an answer is maintained here.
    let answer_text = crate::host_interactions::normalize_answer_value(raw.as_ref());
    match answer_text.as_deref() {
        Some(PLAN_APPROVAL_APPROVE) => PlanApproval::Approved,
        Some(feedback) => PlanApproval::Feedback(feedback.to_string()),
        None => PlanApproval::Declined,
    }
}
/// TS `sanitizePlanFileSessionId`: every run of characters outside `[A-Za-z0-9._-]` becomes `-`, then leading/trailing `-` are removed.
pub fn sanitize_plan_file_session_id(session_id: &str) -> Option<String> {
    let collapsed: String = session_id
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut out = String::new();
    let mut pending_dash = false;
    for c in collapsed.chars() {
        if c == '-' {
            pending_dash = true;
            continue;
        }
        if pending_dash && !out.is_empty() {
            out.push('-');
        }
        pending_dash = false;
        out.push(c);
    }
    (!out.is_empty()).then_some(out)
}
/// Where the approved plan lands on disk (TS `resolveApprovedPlanFilePath`).
pub fn approved_plan_path(workspace_root: &Path, session_id: &str) -> Option<PathBuf> {
    let sanitized = sanitize_plan_file_session_id(session_id)?;
    Some(
        workspace_root
            .join(".zcode")
            .join("plans")
            .join(format!("plan-{sanitized}.md")),
    )
}
/// The atomic write (write a temporary file then rename), the same semantics as TS `writeTextFile({atomic:true})`.
fn write_atomic(path: &Path, content: &str) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_file_name(format!(
        "{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("plan"),
        std::process::id()
    ));
    std::fs::write(&tmp, content)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            Err(error)
        }
    }
}
/// Validates the ExitPlanMode input (TS `ExitPlanModeInputSchema`: plan non-empty, ≤20000 characters,
/// allowedPrompts optional with each item `{tool:"Bash", prompt}`).
fn validate_exit_input(input: &Value) -> Result<String, ToolError> {
    let plan = input
        .get("plan")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::new(ToolErrorCode::InvalidInput, "plan: required string"))?;
    if plan.chars().count() > MAX_PLAN_CHARS {
        return Err(ToolError::new(
            ToolErrorCode::InvalidInput,
            format!("plan: must not exceed {MAX_PLAN_CHARS} characters"),
        ));
    }
    if plan.trim().is_empty() {
        return Err(ToolError::new(
            ToolErrorCode::InvalidInput,
            "String must contain at least 1 character(s)",
        ));
    }
    if let Some(prompts) = input.get("allowedPrompts") {
        let items = prompts.as_array().ok_or_else(|| {
            ToolError::new(
                ToolErrorCode::InvalidInput,
                "allowedPrompts: must be an array",
            )
        })?;
        for item in items {
            if item.get("tool").and_then(Value::as_str) != Some("Bash") {
                return Err(ToolError::new(
                    ToolErrorCode::InvalidInput,
                    "allowedPrompts[].tool: must be \"Bash\"",
                ));
            }
            if item.get("prompt").and_then(Value::as_str).is_none() {
                return Err(ToolError::new(
                    ToolErrorCode::InvalidInput,
                    "allowedPrompts[].prompt: required string",
                ));
            }
        }
    }
    Ok(plan.to_string())
}
/// The session mode controller: one transition = change state + advance the revision + project + persist meta +
/// previousMode. The only owner is `Session`; this only orchestrates the order.
pub struct PlanController {
    host: Arc<crate::HostServer>,
    session: Arc<Session>,
}
impl PlanController {
    pub fn new(host: Arc<crate::HostServer>, session: Arc<Session>) -> Self {
        Self { host, session }
    }

    /// One transition = change state + record planTransition + advance the revision + project + persist meta.
    ///
    /// The order matches TS's `applyRuntimeExecutionState`: the state settles first, only then can the projection see the new value;
    ///
    /// the revision must advance, otherwise a concurrent `baseRevision` CAS would overwrite this transition as a stale write.
    fn apply_mode(&self, mode: &str, tool_call_id: Option<&str>, plan_enabled: bool) {
        self.session.set_collaboration_mode(mode);
        // TS only writes planTransition when `source === "tool" && toolCallId`; a call without a toolCallId
        // (does not exist on this path, but is possible in the types) leaves no invented id.
        if let Some(tool_call_id) = tool_call_id {
            self.session.set_plan_transition(tool_call_id, plan_enabled);
        }
        self.session.bump_revision();
        self.session
            .emit_frames(self.session.project_config_update());
        self.host.persist_meta(&self.session);
    }

    /// Entering plan mode: remembers the mode from before entering, so ExitPlanMode can roll back.
    ///
    /// Returns `(previousMode, mode)`, in the same order as TS's `enterPlanMode()`.
    pub fn enter(&self, tool_call_id: Option<&str>) -> (String, String) {
        let previous = self.session.collaboration_mode();
        if previous != "plan" {
            self.session.set_plan_previous_mode(&previous);
            self.host.persist_meta(&self.session);
        }
        self.apply_mode("plan", tool_call_id, true);
        (previous, "plan".to_string())
    }

    /// Leaving plan mode: returns to the mode from before entering (without bookkeeping it falls back to `build`, matching the host's session default
    ///
    /// mode). Returns `(previousMode, mode)`; TS's `previousMode` is the `plan` from before the exit.
    pub fn exit(&self, tool_call_id: Option<&str>) -> (String, String) {
        let restored = self
            .session
            .plan_previous_mode()
            .unwrap_or_else(|| "build".to_string());
        self.session.set_plan_previous_mode("");
        self.apply_mode(&restored, tool_call_id, false);
        ("plan".to_string(), restored)
    }
    pub fn in_plan_mode(&self) -> bool {
        self.session.is_plan_mode()
    }

    /// Asks the user for one plan approval (the interaction registration/wait/cancellation check all live in the session,
    ///
    /// the same channel as AskUserQuestion, with no second waiting implementation).
    pub async fn request_approval(&self, tool_call_id: &str, input: &Value) -> PlanApproval {
        self.session
            .request_plan_approval(tool_call_id, input)
            .await
    }

    /// After approval the plan is written into the workspace (TS `writeApprovedPlanFile`).
    pub fn write_approved_plan(&self, plan: &str) -> Result<PathBuf, ToolError> {
        let path = approved_plan_path(&self.session.workspace_root, &self.session.id).ok_or_else(
            || {
                ToolError::new(
                    ToolErrorCode::InvalidInput,
                    "Session id cannot produce a plan file name",
                )
            },
        )?;
        write_atomic(&path, plan).map_err(|error| {
            ToolError::new(
                ToolErrorCode::ExecutionFailed,
                format!("write approved plan file failed: {error}"),
            )
        })?;
        Ok(path)
    }
}
pub struct EnterPlanModeTool;
impl Tool for EnterPlanModeTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: ENTER_PLAN_MODE_TOOL_NAME.to_string(),
            description: ENTER_PLAN_MODE_DESCRIPTION.to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
            read_only: false,
            destructive: false,
            concurrent_safe: false,
            side_effect_scope: SCOPE_SESSION,
            timeout_ms: 30_000,
            max_output_bytes: MAX_PLAN_MODEL_BYTES,
            max_model_bytes: MAX_PLAN_MODEL_BYTES,
            preview_direction: "head",
        }
    }

    /// Entering planning is not a write and must work in headless sessions (it only    
    /// changes the session mode).
    fn always_allowed_by_name(&self) -> bool {
        true
    }
    /// TS `EnterPlanModeInputSchema.parse(input)`: no fields, and extra keys given are not interpreted.
    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        if let Some(map) = input.as_object() {
            if !map.is_empty() {
                return Err(ToolError::new(
                    ToolErrorCode::InvalidInput,
                    "EnterPlanMode takes no input fields",
                ));
            }
        } else if !input.is_null() {
            return Err(ToolError::new(
                ToolErrorCode::InvalidInput,
                "EnterPlanMode input must be an object",
            ));
        }
        let Some(controller) = ctx.plan.as_ref() else {
            return Err(ToolError::new(
                ToolErrorCode::ExecutionFailed,
                "EnterPlanMode is unavailable without a session mode controller",
            ));
        };
        controller.enter(ctx.tool_call_id.as_deref());
        Ok(format!(
            "{ENTER_PLAN_MODEL_MESSAGE}\n\nIn plan mode, you should:\n\
             1. Thoroughly explore the codebase to understand existing patterns\n\
             2. Identify similar features and architectural approaches\n\
             3. Consider multiple approaches and their trade-offs\n\
             4. Use AskUserQuestion if you need to clarify the approach\n\
             5. Design a concrete implementation strategy\n\
             6. When ready, use ExitPlanMode to present your plan for approval\n\n\
             Remember: DO NOT write or edit any files yet. This is a read-only exploration and planning phase."
        ))
    }
}
pub struct ExitPlanModeTool;
#[async_trait]
impl AsyncTool for ExitPlanModeTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: EXIT_PLAN_MODE_TOOL_NAME.to_string(),
            description: EXIT_PLAN_MODE_DESCRIPTION.to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plan": {"type": "string", "description": "The implementation plan to present to the user for approval."},
                    "allowedPrompts": {
                        "type": "array",
                        "description": "Prompt-based permissions needed to implement the plan. These describe categories of actions rather than specific commands.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "tool": {"type": "string", "enum": ["Bash"], "description": "The tool this prompt applies to"},
                                "prompt": {"type": "string", "description": "Semantic description of the action, e.g. \"run tests\", \"install dependencies\""}
                            },
                            "required": ["tool", "prompt"]
                        }
                    }
                },
                "required": ["plan"]
            }),
            read_only: false,
            destructive: false,
            concurrent_safe: false,
            side_effect_scope: SCOPE_SESSION,
            timeout_ms: 30_000,
            max_output_bytes: MAX_PLAN_MODEL_BYTES,
            max_model_bytes: MAX_PLAN_MODEL_BYTES,
            preview_direction: "head",
        }
    }
    fn requires_user_interaction(&self) -> bool {
        // The approval interaction is part of this tool itself, so it does not go through the generic permission ask (see the
        // identically named check in `ToolRegistry::authorize_contract`).
        true
    }
    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let plan = validate_exit_input(input)?;
        let Some(controller) = ctx.plan.as_ref() else {
            return Err(ToolError::new(
                ToolErrorCode::ExecutionFailed,
                "ExitPlanMode is unavailable without a session mode controller",
            ));
        };
        if !controller.in_plan_mode() {
            return Err(ToolError::new(
                ToolErrorCode::InvalidInput,
                NOT_IN_PLAN_MODE,
            ));
        }
        let approval = controller
            .request_approval(
                ctx.tool_call_id
                    .as_deref()
                    .unwrap_or(EXIT_PLAN_MODE_TOOL_NAME),
                input,
            )
            .await;
        match approval {
            PlanApproval::Cancelled => Err(ToolError::new(
                ToolErrorCode::Cancelled,
                "ExitPlanMode was cancelled before plan mode was exited",
            )),
            PlanApproval::Declined => Err(ToolError::new(
                ToolErrorCode::PermissionDenied,
                "The user did not approve exiting plan mode. Stay in plan mode and keep the plan open; do not start implementation.",
            )),
            PlanApproval::Feedback(feedback) => Err(ToolError::new(
                ToolErrorCode::PermissionDenied,
                format!(
                    "The user asked for changes before approving: {feedback}. Revise the plan and call ExitPlanMode again."
                ),
            )),
            PlanApproval::Approved => {
                // The order matches TS: **persist first**, then leave the mode (`persistApprovedPlanFileBeforeExitPlanMode`
                // is ordered before `exitPlanMode`). Apart from cancellation, TS swallows a write failure — the user's approval has already happened,
                // and a disk write error cannot revoke it; the host has the same semantics but speaks up on stderr,
                // otherwise "the plan was approved yet it is unreadable everywhere" could only be noticed by the user.
                if let Err(error) = controller.write_approved_plan(&plan) {
                    eprintln!(
                        "[nc-code-host] ExitPlanMode: approved plan was NOT persisted for session {}: {error}",
                        controller.session.id
                    );
                }
                controller.exit(ctx.tool_call_id.as_deref());
                // The feedback wording is aligned with TS `formatExitPlanModeModelContent` character by character: the model uses it to conclude
                // the plan is in effect and starts implementing, so an extra field would change its behaviour.
                Ok(format!(
                    "User has approved your plan. You can now start coding. Start with updating your todo list if applicable.\n\n## Approved Plan:\n{plan}"
                ))
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{AsyncTool, Authorization, Tool, ToolRegistry};
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    fn test_ctx_no_plan() -> crate::tools::ToolContext {
        crate::tools::ToolContext {
            workspace_root: PathBuf::from("."),
            full_access: true,
            interactive: false,
            trace_id: "t".to_string(),
            turn_id: "turn".to_string(),
            read_state: Arc::new(Mutex::new(HashSet::new())),
            cancel: Arc::new(AtomicBool::new(false)),
            todos: crate::tools::todo::new_todo_store(),
            provider_config: None,
            session_id: "test-session".to_string(),
            model_ref: None,
            tool_call_id: None,
            plan_mode: false,
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
    fn only_an_explicit_approve_passes() {
        let approve =
            json!({"content": {"answers": {PLAN_APPROVAL_QUESTION: PLAN_APPROVAL_APPROVE}}});
        assert_eq!(approval_from_answer(&approve), PlanApproval::Approved);
        // The old single-question slots are accepted the same way (TS `answer_0 ?? answer`).
        assert_eq!(
            approval_from_answer(&json!({"content": {"answer": "approve"}})),
            PlanApproval::Approved
        );
        assert_eq!(
            approval_from_answer(&json!({"content": {"answer_0": "  approve  "}})),
            PlanApproval::Approved
        );
        // Free text = a change request, handed to the model verbatim.
        assert_eq!(
            approval_from_answer(&json!({"content": {"answer": "split step 3 first"}})),
            PlanApproval::Feedback("split step 3 first".to_string())
        );
        // decline/cancel/no answer → not let through.
        for answer in [
            json!({"action": "decline"}),
            json!({"action": "cancel", "content": {"answer": PLAN_APPROVAL_APPROVE}}),
            json!({"content": {}}),
            json!({}),
            json!({"content": {"answer": "   "}}),
        ] {
            assert_eq!(
                approval_from_answer(&answer),
                PlanApproval::Declined,
                "{answer} must not approve"
            );
        }
        // The v4 top-level freeText/optionId (the older shape without content) must also be interpretable.
        assert_eq!(
            approval_from_answer(&json!({"freeText": PLAN_APPROVAL_APPROVE})),
            PlanApproval::Approved
        );
        assert_eq!(
            approval_from_answer(&json!({"optionId": PLAN_APPROVAL_APPROVE})),
            PlanApproval::Approved
        );
    }
    #[test]
    fn plan_file_name_sanitizes_like_typescript() {
        assert_eq!(
            sanitize_plan_file_session_id(" ses s/1:2 ").as_deref(),
            Some("ses-s-1-2")
        );
        assert_eq!(
            sanitize_plan_file_session_id("---abc---").as_deref(),
            Some("abc"),
            "First and last - remove, keep the middle"
        );
        assert_eq!(
            sanitize_plan_file_session_id(
                "
///"
            ),
            None,
            "no usable name"
        );
        assert_eq!(sanitize_plan_file_session_id(""), None);
        let path = approved_plan_path(Path::new("/ws"), "ses s/1").expect("path");
        assert_eq!(path, PathBuf::from("/ws/.zcode/plans/plan-ses-s-1.md"));
    }
    #[test]
    fn plan_input_validation_matches_the_ts_schema() {
        assert!(validate_exit_input(&json!({"plan": "do it"})).is_ok());
        assert!(
            validate_exit_input(&json!({"plan": " "})).is_err(),
            "blank plan"
        );
        assert!(validate_exit_input(&json!({})).is_err(), "missing plan");
        assert!(
            validate_exit_input(&json!({"plan": "x".repeat(MAX_PLAN_CHARS + 1)})).is_err(),
            "over the 20000-char cap"
        );
        assert!(validate_exit_input(&json!({
            "plan": "p",
            "allowedPrompts": [{"tool": "Bash", "prompt": "run tests"}]
        }))
        .is_ok());
        assert!(validate_exit_input(&json!({
            "plan": "p",
            "allowedPrompts": [{"tool": "Write", "prompt": "x"}]
        }))
        .is_err());
        assert!(validate_exit_input(&json!({
            "plan": "p",
            "allowedPrompts": [{"tool": "Bash"}]
        }))
        .is_err());
    }
    #[test]
    fn contract_metadata_mirrors_the_ts_tool_entries() {
        let enter = EnterPlanModeTool.contract();
        assert_eq!(enter.name, "EnterPlanMode");
        assert!(!enter.read_only && !enter.destructive && !enter.concurrent_safe);
        assert_eq!(enter.side_effect_scope, "session");
        assert_eq!(enter.max_output_bytes, MAX_PLAN_MODEL_BYTES);
        assert!(enter.description.contains("Prefer using EnterPlanMode"));
        assert!(
            !enter.description.contains("${"),
            "You cannot leave unexpanded template placeholders in the description"
        );
        assert!(
            !enter.description.contains("`find`"),
            "This host does not have find, use embeddedSearchEnabled=false first-level description"
        );
        let exit = ExitPlanModeTool.contract();
        assert_eq!(exit.name, "ExitPlanMode");
        assert_eq!(exit.side_effect_scope, "session");
        assert_eq!(exit.input_schema["required"], json!(["plan"]));
        assert!(
            exit.description
                .contains("Only use this tool when the task requires planning"),
            "The IMPORTANT section of TS must be brought over as is, otherwise the model will also call it on pure research tasks"
        );
    }
    #[test]
    fn approval_questions_match_the_ts_shape() {
        let questions = approval_questions(EXIT_PLAN_MODE_REASON);
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0]["question"], json!(EXIT_PLAN_MODE_REASON));
        assert_eq!(questions[0]["header"], json!("Plan"));
        assert_eq!(
            questions[0]["options"][0]["value"],
            json!(PLAN_APPROVAL_APPROVE)
        );
        assert_eq!(questions[0]["options"][0]["label"], json!("Approve"));
    }
    #[test]
    fn plan_mode_gate_allows_reads_and_blocks_writes_but_never_the_approval_itself() {
        // Read-only tools are allowed in plan mode (including the Glob/Grep/Read and TodoRead family).
        let read_only = ToolContract {
            name: "Read".to_string(),
            description: String::new(),
            input_schema: json!({}),
            read_only: true,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: SCOPE_SESSION,
            timeout_ms: 1000,
            max_output_bytes: 1024,
            max_model_bytes: 1024,
            preview_direction: "head",
        };
        // A write-class tool must be denied in plan mode, **even when** the session is full-access.
        let write = ToolContract {
            destructive: true,
            ..read_only.clone()
        };
        let mcp = ToolContract {
            name: "mcp__github__create_issue".to_string(),
            ..read_only.clone()
        };
        let ctx = |plan_mode: bool| ToolContext {
            workspace_root: PathBuf::from("/ws"),
            full_access: plan_mode, // key point: full_access must not override plan mode
            interactive: true,
            trace_id: "t".to_string(),
            turn_id: "turn".to_string(),
            session_id: "session-1".to_string(),
            model_ref: None,
            automation: None,
            browser: None,
            computer: None,
            offpeak: None,
            sessions: None,
            workflow: None,
            read_state: Arc::new(std::sync::Mutex::new(Default::default())),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),

            todos: crate::tools::todo::new_todo_store(),
            provider_config: None,
            tool_call_id: None,
            subagent: None,
            ask_user: None,
            plan_mode,
            plan: None,
            background: None,
        };
        let registry = ToolRegistry::with_defaults();
        assert!(matches!(
            registry.authorize_contract(&read_only, true, &ctx(true), false, false, false),
            Authorization::Allow
        ));
        assert!(matches!(
            registry.authorize_contract(&mcp, false, &ctx(true), false, false, false),
            Authorization::Allow
        ));
        match registry.authorize_contract(&write, false, &ctx(true), false, false, false) {
            Authorization::Deny(reason) => {
                assert!(
                    reason.contains("read-only, non-destructive"),
                    "reason must be TS: {reason}"
                );
            }
            other => panic!("destructive tools must be denied in plan mode, got {other:?}"),
        }
        // A tool carrying its own approval is not governed by the plan gate (otherwise plan mode could never be exited).
        assert!(matches!(
            registry.authorize_contract(&write, false, &ctx(true), true, false, false),
            Authorization::Allow
        ));
        // Outside plan mode a write tool returns to the original ask path (with an interactive client → ask the user).
        assert!(matches!(
            registry.authorize_contract(&write, false, &ctx(false), false, false, false),
            Authorization::Ask
        ));
        // The same tool fails closed in a session without an interactive client.
        let mut headless = ctx(false);
        headless.interactive = false;
        assert!(matches!(
            registry.authorize_contract(&write, false, &headless, false, false, false),
            Authorization::Deny(_)
        ));
    }

    #[test]
    fn enter_plan_mode_rejects_invalid_input_and_missing_controller() {
        let ctx = test_ctx_no_plan();
        let tool = EnterPlanModeTool;
        let bad = tool.execute(&json!({"extra": true}), &ctx);
        assert!(
            matches!(bad, Err(e) if matches!(e.code, crate::tools::ToolErrorCode::InvalidInput))
        );
        let non_obj = tool.execute(&json!("nope"), &ctx);
        assert!(
            matches!(non_obj, Err(e) if matches!(e.code, crate::tools::ToolErrorCode::InvalidInput))
        );
        let missing_ctrl = tool.execute(&json!({}), &ctx);
        assert!(matches!(
            missing_ctrl,
            Err(e) if matches!(e.code, crate::tools::ToolErrorCode::ExecutionFailed)
                && e.message.contains("unavailable without a session mode controller")
        ));
    }

    #[tokio::test]
    async fn exit_plan_mode_validates_input_before_controller_check() {
        let ctx = test_ctx_no_plan();
        let tool = ExitPlanModeTool;
        let bad = tool.execute(&json!({"plan": " "}), &ctx).await;
        assert!(
            matches!(bad, Err(e) if matches!(e.code, crate::tools::ToolErrorCode::InvalidInput))
        );
        let missing = tool.execute(&json!({}), &ctx).await;
        assert!(
            matches!(missing, Err(e) if matches!(e.code, crate::tools::ToolErrorCode::InvalidInput))
        );
        let no_ctrl = tool.execute(&json!({"plan": "do it"}), &ctx).await;
        assert!(matches!(
            no_ctrl,
            Err(e) if matches!(e.code, crate::tools::ToolErrorCode::ExecutionFailed)
                && e.message.contains("unavailable without a session mode controller")
        ));
    }
}
