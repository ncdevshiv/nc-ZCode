//! Legacy `ZCodeSessionStateSnapshot` projection over the host's v4 read model.
//!
//! WHY this module exists: the current frontend still parses the old protocol snapshot
//! (`packages/shared/src/nc-code-protocol/index.ts:1015` `zcodeSessionStateSnapshotSchema`),
//! and every schema in that closure is `.strict()` — one extra key or one wrong key set
//! fails client-side parsing and the whole snapshot is dropped. The previous adapter
//! hardcoded `status:"idle"`, `messages:[]` and zeroed counters and emitted a FLAT
//! `session.workspacePath`, so the live consumers (`useActiveTaskSnapshotMeta.ts:66`,
//! `SessionPane.tsx:1886`, `zcodeSessionProjection.ts:174`,
//! `nc-code-session-task-status.ts`) degraded silently.
//!
//! Every value below comes from a real host fact:
//! - `AgentRuntime::session_view` — title/titleSource/phase/createdAt/updatedAt/revision/
//!   provider/model/thought/mode/goalStatus/lastError/pendingInteractions;
//! - the v4 conversation snapshot from `v4/conversation/resync` — `seq`, `usage`, `goal`,
//!   `pendingInteractions`, `control`, `meta`, `config`;
//! - the transcript row ledger from `session_transcript_view` — turns, inputs, answers,
//!   reasoning and tool calls;
//! - `v4/conversation/usage` (`usage_report`) — cumulative tokens + request/error counts;
//! - the personal provider store — `settings.model.available`,
//!   `settings.thoughtLevel.available`, `projection.contextWindow`.
//!
//! NO-INVENTION POLICY (also reported to the port log):
//! - per-message `tokens` and `cost`: the host records session-cumulative usage only, so
//!   these are the schema's zero value. Nothing in the host attributes tokens to a message.
//! - `projection.contextUsed` / `runtime.contextUsage`: no per-request context watermark
//!   is kept (`Session::accumulate_usage` stores cumulative + `contextWindow: null`), so
//!   contextUsed is 0 and `runtime.contextUsage` is omitted, exactly like the TS mapper
//!   which returns `undefined` when `contextUsed <= 0` (`session-mapper.ts:717`).
//! - `pendingPermissions[].riskLevel`: the host's permission payload has no risk
//!   classification; the neutral member of the closed enum is used rather than guessing
//!   per tool name.
//! - `settings.model.lastUsed`: no last-used store exists in this crate → key omitted.
//! - `slashCommands` / `todoGroups`: the native host registers no slash commands and no
//!   TodoWrite producer; `slashCommands` is `[]` (truthful "none registered") and
//!   `todoGroups` is omitted rather than faked.

use std::collections::HashMap;

use serde_json::{json, Value};

/// `nc_code_PROTOCOL_NAME` / `nc_code_PROTOCOL_VERSION`
/// (`packages/shared/src/nc-code-protocol/index.ts:72`).
const PROTOCOL_NAME: &str = "ZCode Protocol";
const PROTOCOL_VERSION: u64 = 1;
/// `message.info.agent` for the single main agent of this host. Mirrors the CLI's own
/// fallback (`compact-persistence.ts:322` `agent: this.config.agentName ?? "zcode-agent"`).
const MAIN_AGENT: &str = "zcode-agent";
/// `zcodeSessionInfoSchema.sessionKind`: the host's meta.json stores no session kind and
/// this channel only creates interactive sessions.
const SESSION_KIND: &str = "interactive";
/// `zcodeSessionSettingsStateSchema.mode.current` fallback for an out-of-enum value.
const DEFAULT_MODE: &str = "build";

/// Model/thought facts from the personal provider store. The host has no model catalog,
/// so `settings.model.available` and `projection.contextWindow` are read here instead.
#[derive(Debug, Default, Clone)]
pub struct SettingsFacts {
    /// `ZCodeModelOption[]` (`zcodeModelOptionSchema`, strict).
    pub available_models: Vec<Value>,
    /// `ZCodeThoughtLevelOption[]` for the session's current model.
    pub thought_levels: Vec<Value>,
    /// `settings.thoughtLevel.defaultLevel` — only when the store declares one.
    pub thought_default: Option<String>,
    /// `projection.contextWindow` — the current model's `properties.contextWindow`.
    pub context_window: u64,
}

/// One session's host facts, already read by the caller.
pub struct SnapshotInput<'a> {
    pub workspace_path: &'a str,
    /// `workspaceIdentity?.trim() || workspacePath` — the repo-wide identity rule.
    pub workspace_key: &'a str,
    pub session_id: &'a str,
    /// `AgentRuntime::session_view` for this session (`Null` when unknown).
    pub view: &'a Value,
    /// The v4 conversation snapshot (`payload.snapshot` of a `kind:"snapshot"` frame);
    /// `None` for a brand-new draft session that has not projected one yet.
    pub v4: Option<&'a Value>,
    /// Full transcript rows in ledger order (`session_transcript_view.rows`).
    pub rows: &'a [Value],
    /// `v4/conversation/usage` report.
    pub usage: Option<&'a Value>,
    pub settings: &'a SettingsFacts,
    /// `readSession.messageLimit` — tail window, matching the task facade's semantics.
    pub message_limit: Option<usize>,
}

impl<'a> SnapshotInput<'a> {
    /// `session_view` first, then the v4 snapshot's `meta`/`config`/`control` for the
    /// same fact (the snapshot is the revision-bearing authority during a turn).
    fn field(&self, view_key: &str, v4_path: &[&str]) -> Option<Value> {
        if let Some(value) = self.view.get(view_key).filter(|value| !value.is_null()) {
            return Some(value.clone());
        }
        if v4_path.is_empty() {
            return None;
        }
        let mut cursor = self.v4?;
        for key in v4_path {
            cursor = cursor.get(*key)?;
        }
        (!cursor.is_null()).then(|| cursor.clone())
    }

    fn text(&self, view_key: &str, v4_path: &[&str]) -> Option<String> {
        non_empty(
            self.field(view_key, v4_path)
                .as_ref()
                .and_then(Value::as_str),
        )
    }

    fn number(&self, view_key: &str, v4_path: &[&str]) -> i64 {
        self.field(view_key, v4_path)
            .as_ref()
            .and_then(Value::as_i64)
            .unwrap_or(0)
    }
}

fn non_empty(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn non_empty_cloned(value: Option<&str>) -> Option<String> {
    non_empty(value)
}

fn as_i64(value: Option<&Value>) -> i64 {
    value.and_then(Value::as_i64).unwrap_or(0)
}

/// `zcodeSessionStatusSchema` = idle|running|waiting|paused|completed|error.
/// Host `sessionPhaseSchema` = draft|prewarming|running|completedSuccess|
/// completedInterrupted|error (`nc-code-host/src/session.rs` `VALID_PHASES`).
/// A running turn that is waiting on a pending interaction maps to `waiting`, which is
/// what keeps the permission gate authoritative in `nc-code-session-task-status.ts`.
pub fn status_from_phase(phase: &str, has_pending_interaction: bool) -> &'static str {
    match phase {
        "running" => {
            if has_pending_interaction {
                "waiting"
            } else {
                "running"
            }
        }
        "completedSuccess" | "completedInterrupted" => "completed",
        "error" => "error",
        // draft / prewarming / unknown: no turn is executing.
        _ => "idle",
    }
}

/// `zcodeSessionModeSchema` = plan|build|edit|yolo|auto. The host stores whatever a client
/// wrote, so an unknown value must not ship: one invalid enum rejects the whole snapshot.
pub fn sanitize_mode(mode: &str) -> &'static str {
    // Return literal by literal: treating the input &str as 'static would not satisfy the borrow (E lifetime).
    match mode {
        "plan" => "plan",
        "build" => "build",
        "edit" => "edit",
        "yolo" => "yolo",
        "auto" => "auto",
        _ => DEFAULT_MODE,
    }
}

/// `zcodeSessionInfoSchema.titleSource` closed set; unknown ⇒ omit the key.
fn sanitize_title_source(value: &str) -> Option<&'static str> {
    match value {
        // Same as sanitize_mode: a closed set returns literals, it does not borrow the argument.
        "default" => Some("default"),
        "first_input" => Some("first_input"),
        "generated" => Some("generated"),
        "custom" => Some("custom"),
        _ => None,
    }
}

/// `modelSelectionSchema` from the host's session config strings. The host keeps
/// provider/model as opaque strings written by `switchModelConfig`; this channel always
/// writes provider-store ids there, so the round trip is real. Unbound ⇒ key omitted
/// (`zcodeSessionSettingsStateSchema:896` "an unbound state is a legal recovery state; the model must not be forged just to satisfy the protocol").
pub fn model_selection(
    provider: Option<&str>,
    model: Option<&str>,
    thought: Option<&str>,
) -> Option<Value> {
    let provider = non_empty_cloned(provider)?;
    let model = non_empty_cloned(model)?;
    let mut selection = json!({ "providerId": provider, "modelId": model });
    if let Some(level) = non_empty_cloned(thought) {
        selection["options"] = json!({ "reasoningLevel": level });
    }
    Some(selection)
}

fn interactions_of(input: &SnapshotInput<'_>) -> Vec<Value> {
    let from_view = input
        .view
        .get("pendingInteractions")
        .and_then(Value::as_array)
        .cloned();
    let from_snapshot = input
        .v4
        .and_then(|snapshot| snapshot.get("pendingInteractions"))
        .and_then(Value::as_array)
        .cloned();
    // The snapshot is the revision-bearing authority; the view is its synchronous twin.
    from_snapshot
        .filter(|items| !items.is_empty())
        .or(from_view)
        .unwrap_or_default()
}

/// v4 `pendingInteractionSchema` → `zcodePendingPermissionSchema`.
/// Only `kind:"permission"` gates a tool call here; `userInput` and
/// `workspaceHookReview` are answered through their own v4 commands
/// (`resolveInteraction` / `respondWorkspaceHookReview`).
pub fn pending_permissions(interactions: &[Value]) -> Vec<Value> {
    let mut items = Vec::new();
    for interaction in interactions {
        if interaction.get("kind").and_then(Value::as_str) != Some("permission") {
            continue;
        }
        let Some(request_id) =
            non_empty_cloned(interaction.get("interactionId").and_then(Value::as_str))
        else {
            continue;
        };
        let payload = interaction.get("payload").cloned().unwrap_or(Value::Null);
        let Some(tool_call_id) =
            non_empty_cloned(payload.get("toolCallId").and_then(Value::as_str))
        else {
            continue;
        };
        let Some(tool_name) = non_empty_cloned(payload.get("toolName").and_then(Value::as_str))
        else {
            continue;
        };
        let options = permission_options(&payload);
        if options.is_empty() {
            // `zcodePendingPermissionSchema.options` is `.min(1)`; an option-less entry
            // would fail strict parsing of the whole snapshot.
            continue;
        }
        let mut item = json!({
            "requestId": request_id,
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            // The host's permission summary is the human-readable gate text.
            "reason": payload
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            // NO SOURCE: the host payload carries no risk classification
            // (`interaction::open_permission`). Neutral member of the closed enum
            // instead of a per-tool guess.
            "riskLevel": "medium",
            "options": options,
            "requestedAt": as_i64(interaction.get("createdAt")).max(0),
        });
        if let Some(detail) = payload.get("detail").filter(|value| !value.is_null()) {
            item["input"] = detail.clone();
        }
        items.push(item);
    }
    items
}

/// The host's option set is closed (`interaction::open_permission`):
/// `allowOnce` / `deny`, plus the sibling `fullAccessOption`.
/// `zcodePermissionOptionSchema` needs `{optionId, kind, name, response{decision}}`, so
/// `label`→`name` and the decision comes from the option's own kind — the same mapping as
/// the TS legacy builder (`bootstrap/src/permission-options.ts`).
fn permission_options(payload: &Value) -> Vec<Value> {
    let mut options: Vec<Value> = Vec::new();
    let mut push_option = |option: &Value| {
        let Some(option_id) = non_empty_cloned(option.get("optionId").and_then(Value::as_str))
        else {
            return;
        };
        let kind = option
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("custom")
            .to_string();
        let decision = match kind.as_str() {
            "allowOnce" | "allowAlways" | "fullAccess" => "allow",
            // Unknown kinds resolve fail-closed: a legacy client echoing this response
            // back must never allow a tool it cannot classify.
            _ => "deny",
        };
        let name = non_empty_cloned(option.get("label").and_then(Value::as_str))
            .unwrap_or_else(|| option_id.clone());
        options.push(json!({
            "optionId": option_id,
            "kind": kind,
            "name": name,
            "response": { "decision": decision },
        }));
    };
    if let Some(items) = payload.get("options").and_then(Value::as_array) {
        for option in items {
            push_option(option);
        }
    }
    if let Some(full_access) = payload
        .get("fullAccessOption")
        .filter(|value| value.is_object())
    {
        push_option(full_access);
    }
    options
}

/// `zcodeActiveToolCallSchema` from toolCall rows that are still `running`.
pub fn active_tool_calls(rows: &[Value]) -> Vec<Value> {
    rows.iter()
        .filter(|row| row.get("kind").and_then(Value::as_str) == Some("toolCall"))
        .filter(|row| row.get("status").and_then(Value::as_str) == Some("running"))
        .filter_map(|row| {
            let tool_call_id = non_empty_cloned(row.get("toolCallId").and_then(Value::as_str))?;
            let tool_name = non_empty_cloned(row.get("toolName").and_then(Value::as_str))?;
            let mut item = json!({
                "toolCallId": tool_call_id,
                "toolName": tool_name,
                "status": "running",
            });
            if let Some(started_at) = row.get("createdAt").and_then(Value::as_i64) {
                item["startedAt"] = json!(started_at.max(0));
            }
            Some(item)
        })
        .collect()
}

/// Host `sessionErrorInfo` (`snapshot::session_error_info`: code/message/recoverable/at/
/// source) → `zcodeSessionProjectionSchema.lastError`
/// `{type, code?, message, detail?, attribution?}`. `recoverable` and `at` are not in the
/// legacy strict object, so they are stripped instead of passed through.
pub fn last_error(info: Option<&Value>) -> Option<Value> {
    let info = info.filter(|value| value.is_object())?;
    let message = non_empty_cloned(info.get("message").and_then(Value::as_str))?;
    let type_name = non_empty_cloned(info.get("source").and_then(Value::as_str))
        .unwrap_or_else(|| "runtime".to_string());
    let mut item = json!({ "type": type_name, "message": message });
    if let Some(code) = non_empty_cloned(info.get("code").and_then(Value::as_str)) {
        item["code"] = json!(code);
    }
    Some(item)
}

/// v4 `goalStateSchema` (`goal.rs::to_projection`) → `zcodeSessionGoalSchema`.
/// See the NO-INVENTION notes on `createdAt`/`updatedAt`/`tokensUsed`/`tokenBudget` below.
pub fn goal_target(
    goal: Option<&Value>,
    session_id: &str,
    session_created_at: i64,
    session_updated_at: i64,
) -> Option<Value> {
    let goal = goal.filter(|value| value.is_object())?;
    let target_id = non_empty_cloned(goal.get("targetId").and_then(Value::as_str))?;
    let objective = non_empty_cloned(goal.get("objective").and_then(Value::as_str))?;
    let status = match goal.get("status").and_then(Value::as_str) {
        Some("paused") => "paused",
        Some("budget_limited") => "budget_limited",
        Some("complete") => "complete",
        // verifying/verified/notSatisfied/failed are projection states on top of a
        // durable `active` (crate README difference 35).
        _ => "active",
    };
    // Real timestamps exist only once a verifier settled (`VerificationEntry.at`); before
    // that the session's durable created/updated times are the only host fact that bounds
    // the goal, so they are used rather than a made-up instant.
    let stamps: Vec<i64> = goal
        .get("verifications")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|entry| entry.get("at").and_then(Value::as_i64))
                .collect()
        })
        .unwrap_or_default();
    let created = stamps
        .iter()
        .copied()
        .min()
        .unwrap_or(session_created_at)
        .max(0);
    let updated = stamps
        .iter()
        .copied()
        .max()
        .unwrap_or(session_updated_at)
        .max(created);
    let mut item = json!({
        "sessionId": session_id,
        "targetId": target_id,
        "objective": objective,
        "summaryTitle": match goal.get("summaryTitle").and_then(Value::as_str) {
            Some(value) => json!(value),
            None => Value::Null,
        },
        "status": status,
        // `v4/sendGoalCommand` carries no budget field (crate README difference 35) ⇒
        // null means "no budget set", which is what the schema uses for that state.
        "tokenBudget": Value::Null,
        // No per-goal token attribution exists in the host ⇒ 0, the same convention as
        // the TS mapper's `goal.tokensUsed ?? 0` (session-mapper.ts:1055).
        "tokensUsed": 0,
        "timeUsedSeconds": as_i64(goal.get("timeUsedSeconds")).max(0),
        "createdAt": created,
        "updatedAt": updated,
    });
    if let Some(started) = goal.get("activeRunStartedAtMs").and_then(Value::as_i64) {
        item["activeRunStartedAtMs"] = json!(started.max(0));
    }
    Some(item)
}

/// `zcodeSessionGoalStatsSchema` (every key required). Iteration and tool-call counts come
/// from the durable goal record and the row ledger; token/context numbers use the same
/// real sources as the projection.
pub fn goal_stats(
    target: &Value,
    context_used: u64,
    context_window: u64,
    tool_call_count: u64,
    iteration: u64,
) -> Value {
    json!({
        "contextUsed": context_used,
        "contextWindow": context_window,
        "iterationCount": iteration,
        "timeUsedSeconds": as_i64(target.get("timeUsedSeconds")).max(0),
        "tokenBudget": target.get("tokenBudget").cloned().unwrap_or(Value::Null),
        "tokensUsed": as_i64(target.get("tokensUsed")).max(0),
        "toolCallCount": tool_call_count,
    })
}

/// `zcodeSessionRuntimeStateSchema.goalVerifications[]` from the v4 goal's
/// `verifications[]` (`{iteration, outcome, at, reason?, nextAction?}`).
pub fn goal_verifications(goal: Option<&Value>) -> Vec<Value> {
    let Some(items) = goal
        .and_then(|value| value.get("verifications"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    items
        .iter()
        .map(|entry| {
            json!({
                "passed": entry.get("outcome").and_then(Value::as_str) == Some("pass"),
                "reason": entry
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                "nextAction": match entry.get("nextAction").and_then(Value::as_str) {
                    Some(value) => json!(value),
                    None => Value::Null,
                },
            })
        })
        .collect()
}

/// `zcodeSessionGoalVerificationTimelineSchema` from the same durable records.
/// `status` is the legacy closed set started|completed|failed_closed|cancelled; the host
/// stores the verifier outcome (pass/notSatisfied/failed) after settlement, so a settled
/// entry is `completed` and an in-flight verifier (no record yet) is not fabricated.
pub fn goal_verification_timeline(goal: Option<&Value>, updated_at: i64) -> Vec<Value> {
    let Some(items) = goal
        .and_then(|value| value.get("verifications"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    items
        .iter()
        // the closure always returns Some: filter_map becomes map (the equivalent rewrite for clippy::option_filter_map).
        .map(|entry| {
            let iteration = as_i64(entry.get("iteration"));
            let at = as_i64(entry.get("at")).max(0);
            let outcome = entry.get("outcome").and_then(Value::as_str).unwrap_or("");
            let mut item = json!({
                "version": 1,
                "kind": "synthetic",
                "type": "goal_verification",
                "display": "separator",
                "targetId": entry
                    .get("targetId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    // The host keys verifications under one target per goal record, so
                    // the record's own targetId is the goal projection's targetId.
                    .unwrap_or_else(|| {
                        goal.and_then(|value| value.get("targetId"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string()
                    }),
                "verificationId": format!("goal-verification-{iteration}"),
                "status": if outcome == "failed" { "failed_closed" } else { "completed" },
                "updatedAt": at,
            });
            if iteration > 0 {
                item["goalIteration"] = json!(iteration);
            }
            if at > 0 {
                item["startedAt"] = json!(at);
            }
            item["verification"] = json!({
                "passed": outcome == "pass",
                "reason": entry.get("reason").and_then(Value::as_str).unwrap_or("").to_string(),
                "nextAction": match entry.get("nextAction").and_then(Value::as_str) {
                    Some(value) => json!(value),
                    None => Value::Null,
                },
            });
            let _ = updated_at;
            item
        })
        .collect()
}

/// The turn that is executing right now: the newest turnHeader row whose `state` is still
/// `running`. `None` means every projected turn has settled.
pub fn running_turn_id(rows: &[Value]) -> Option<String> {
    rows.iter()
        .rev()
        .find(|row| {
            row.get("kind").and_then(Value::as_str) == Some("turnHeader")
                && row.get("state").and_then(Value::as_str) == Some("running")
        })
        .and_then(|row| row.get("turnId").and_then(Value::as_str))
        .map(str::to_string)
}

/// Last projected turn (kept after completion, like the TS `projection.currentTurnId`).
pub fn current_turn_id(rows: &[Value]) -> Option<String> {
    rows.iter()
        .rev()
        .find(|row| row.get("kind").and_then(Value::as_str) == Some("turnHeader"))
        .and_then(|row| row.get("turnId").and_then(Value::as_str))
        .map(str::to_string)
}

/// Number of turns = turnHeader rows in the ledger (one row per turn, upserted in place).
pub fn turn_count(rows: &[Value]) -> u64 {
    rows.iter()
        .filter(|row| row.get("kind").and_then(Value::as_str) == Some("turnHeader"))
        .count() as u64
}

/// Number of tool calls (rows), used by `goalStats.toolCallCount`.
pub fn tool_call_count(rows: &[Value]) -> u64 {
    rows.iter()
        .filter(|row| row.get("kind").and_then(Value::as_str) == Some("toolCall"))
        .count() as u64
}

// ── rows → ZCodeMessageWithParts[] ────────────────────────────────────────────────

/// One message under construction, in ledger order.
struct Builder {
    message_id: String,
    turn_id: String,
    role: &'static str,
    created_at: i64,
    completed_at: Option<i64>,
    finish: Option<String>,
    row_id: u64,
    parts: Vec<Value>,
    /// assistantText row state (terminal ⇒ time.completed + finish are set).
    terminal: bool,
    model_label: Option<String>,
    /// The user message that started this turn (assistant parentMessageId source).
    parent: Option<String>,
}

impl Builder {
    fn to_message(&self, session_id: &str, workspace_path: &str, model: Option<&Value>) -> Value {
        let parts: Vec<Value> = self
            .parts
            .iter()
            .map(|part| {
                let mut part = part.clone();
                if let Some(object) = part.as_object_mut() {
                    object
                        .entry("messageId".to_string())
                        .or_insert_with(|| json!(self.message_id));
                }
                part
            })
            .collect();
        let mut time = json!({ "created": self.created_at.max(0) });
        if let Some(completed_at) = self.completed_at {
            time["completed"] = json!(completed_at.max(self.created_at));
        }
        if self.role == "user" {
            let mut info = json!({
                "messageId": self.message_id,
                "sessionId": session_id,
                "role": "user",
                "time": time,
                "agent": MAIN_AGENT,
                // realUserInput policy: origin=real_user + not synthetic + not
                // model-only (conversation-message-projection-policy.ts:94).
                "semantics": {
                    "origin": "real_user",
                    "kind": "user_prompt",
                    "uiVisibility": "visible",
                    "providerVisibility": "visible",
                    "transcriptVisibility": "visible",
                },
            });
            if let Some(model) = model {
                info["model"] = model.clone();
            }
            return json!({ "info": info, "parts": parts });
        }
        let mut info = json!({
            "messageId": self.message_id,
            "sessionId": session_id,
            "role": "assistant",
            "time": time,
            "parentMessageId": self.parent_fallback(session_id),
            "agent": MAIN_AGENT,
            "path": { "cwd": workspace_path, "root": workspace_path },
            // NO SOURCE: the host records no cost and no per-message tokens.
            "cost": 0,
            "tokens": {
                "input": 0,
                "output": 0,
                "reasoning": 0,
                "cache": { "read": 0, "write": 0 },
            },
            "semantics": {
                "origin": "agent_runtime",
                "kind": "assistant_response",
                "uiVisibility": "visible",
                "providerVisibility": "visible",
                "transcriptVisibility": "visible",
            },
        });
        if let Some(model) = model {
            info["model"] = model.clone();
        }
        if let Some(finish) = &self.finish {
            info["finish"] = json!(finish);
        }
        if let Some(label) = &self.model_label {
            // The row's model label is a real provider fact, kept observable in metadata
            // because `model` must be a full ModelSelection (strict schema).
            info["metadata"] = json!({ "providerModelLabel": label });
        }
        json!({ "info": info, "parts": parts })
    }

    fn parent_fallback(&self, session_id: &str) -> String {
        // Set from the turn's userInput row when one exists; a root assistant falls back to
        // the session id (a stable non-empty identity) rather than inventing a message id.
        // Goal-continuation turns produce no userInput row (crate README difference 37),
        // so their first assistant has no real parent message in this host.
        self.parent
            .clone()
            .unwrap_or_else(|| session_id.to_string())
    }
}

/// Rows → `zcodeMessageWithPartsSchema` items.
///
/// Row facts used: `kind`, `turnId`, `entityId`, `rowId`, `createdAt`, `text`, `state`,
/// `status`, `inputText`, `error{code,message}`, `toolName`, `assistantResponseId`,
/// `origin` (turnHeader). Ordering is ledger order.
pub fn messages_from_rows(
    rows: &[Value],
    session_id: &str,
    workspace_path: &str,
    model: Option<&Value>,
) -> Vec<Value> {
    let mut builders: Vec<Builder> = Vec::new();
    let mut by_response: HashMap<(String, String), usize> = HashMap::new();
    let mut last_assistant: HashMap<String, usize> = HashMap::new();
    let mut turn_parent: HashMap<String, String> = HashMap::new();
    let mut turn_max_tool_row: HashMap<String, u64> = HashMap::new();

    for row in rows {
        let kind = row.get("kind").and_then(Value::as_str).unwrap_or_default();
        let turn_id = row
            .get("turnId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let created_at = as_i64(row.get("createdAt"));
        let row_id = row.get("rowId").and_then(Value::as_u64).unwrap_or(0);
        match kind {
            "userInput" => {
                let text = row.get("text").and_then(Value::as_str).unwrap_or("");
                let message_id = row
                    .get("entityId")
                    .and_then(non_empty_owned)
                    .unwrap_or_else(|| format!("user:{turn_id}:{row_id}"));
                turn_parent.insert(turn_id.clone(), message_id.clone());
                last_assistant.remove(&turn_id);
                builders.push(Builder {
                    // message_id is used again by parts afterwards (text_part borrows it), so clone first.
                    message_id: message_id.clone(),
                    turn_id,
                    role: "user",
                    created_at,
                    completed_at: None,
                    finish: None,
                    row_id,
                    parts: vec![text_part(session_id, &message_id, "user-input", text)],
                    terminal: true,
                    model_label: None,
                    parent: None,
                });
            }
            "reasoning" => {
                let text = row.get("text").and_then(Value::as_str).unwrap_or("");
                if text.is_empty() {
                    continue;
                }
                let response_id = response_id_of(row, &turn_id, "reasoning");
                let part = json!({
                    "partId": part_id(row, &response_id, "reasoning"),
                    "sessionId": session_id,
                    "type": "reasoning",
                    "text": text,
                });
                let index = ensure_assistant(
                    &mut builders,
                    &mut by_response,
                    &mut last_assistant,
                    &turn_parent,
                    &turn_id,
                    &response_id,
                    created_at,
                    row,
                );
                builders[index].parts.push(part);
            }
            "assistantText" => {
                let text = row.get("text").and_then(Value::as_str).unwrap_or("");
                let response_id = response_id_of(row, &turn_id, "assistant");
                let state = row
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("streaming");
                let part = json!({
                    "partId": part_id(row, &response_id, "assistant-text"),
                    "sessionId": session_id,
                    "type": "text",
                    "text": text,
                });
                let index = ensure_assistant(
                    &mut builders,
                    &mut by_response,
                    &mut last_assistant,
                    &turn_parent,
                    &turn_id,
                    &response_id,
                    created_at,
                    row,
                );
                let builder = &mut builders[index];
                builder.parts.push(part);
                builder.terminal = state != "streaming";
                builder.model_label = row.get("model").and_then(non_empty_owned);
                if builder.terminal {
                    // A terminal row's createdAt is the authoritative upsert moment ⇒ time.completed + finish.
                    builder.completed_at = Some(created_at);
                    builder.finish = Some(assistant_finish(state).to_string());
                }
            }
            "toolCall" => {
                let Some(tool_name) = non_empty_cloned(row.get("toolName").and_then(Value::as_str))
                else {
                    continue;
                };
                let Some(call_id) = non_empty_cloned(row.get("toolCallId").and_then(Value::as_str))
                else {
                    continue;
                };
                let status = row
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("running");
                let response_id = format!("tool:{call_id}");
                let part = json!({
                    "partId": part_id(row, &response_id, "tool"),
                    "sessionId": session_id,
                    "type": "tool",
                    "callId": call_id.clone(),
                    "tool": tool_name,
                    "state": tool_state(row, status, created_at),
                });
                // A tool card belongs to the latest assistant message of this turn (the same shape as TS: body + tool parts).
                // Only when this turn has no assistant message yet (a tool called at the head of the turn) is one created.
                let index = match last_assistant.get(&turn_id).copied() {
                    Some(index) => index,
                    None => ensure_assistant(
                        &mut builders,
                        &mut by_response,
                        &mut last_assistant,
                        &turn_parent,
                        &turn_id,
                        &response_id,
                        created_at,
                        row,
                    ),
                };
                let builder = &mut builders[index];
                // A toolCall row going running→terminal upserts the same rowId; the same callId overwrites instead of appending.
                match builder.parts.iter().position(|part| {
                    part.get("type").and_then(Value::as_str) == Some("tool")
                        && part.get("callId").and_then(Value::as_str) == Some(call_id.as_str())
                }) {
                    Some(position) => builder.parts[position] = part,
                    None => builder.parts.push(part),
                }
                let entry = turn_max_tool_row.entry(turn_id.clone()).or_insert(0);
                *entry = (*entry).max(row_id);
            }
            _ => {}
        }
    }
    // `finish:"tool-calls"`: an assistant answer that is followed by tool rows in the same
    // turn is not the terminal answer (`nc-code-session-task-status.ts:38` reads this).
    for builder in &mut builders {
        if builder.role != "assistant" || !builder.terminal {
            continue;
        }
        // The original implementation referenced turn_has_later_tool here (a map that was never constructed). The semantics of the test
        // "an assistant row followed by another tool row in the same turn" are already covered by turn_max_tool_row:
        // it records the largest tool-row rowId inside each turn, and a value larger than the assistant's rowId means "follows".
        let max_tool_row = turn_max_tool_row
            .get(&builder.turn_id)
            .copied()
            .unwrap_or(0);
        if max_tool_row > builder.row_id {
            builder.finish = Some("tool-calls".to_string());
        }
    }
    builders
        .iter()
        .map(|builder| builder.to_message(session_id, workspace_path, model))
        .collect()
}

fn non_empty_owned(value: &Value) -> Option<String> {
    non_empty_cloned(value.as_str())
}

fn response_id_of(row: &Value, turn_id: &str, prefix: &str) -> String {
    let from_response = row.get("assistantResponseId").and_then(non_empty_owned);
    let from_entity = row.get("entityId").and_then(non_empty_owned);
    let row_id = row.get("rowId").and_then(Value::as_u64).unwrap_or(0);
    from_response
        .or(from_entity)
        .unwrap_or_else(|| format!("{prefix}:{turn_id}:{row_id}"))
}

fn part_id(row: &Value, response_id: &str, kind: &str) -> String {
    let row_id = row.get("rowId").and_then(Value::as_u64).unwrap_or(0);
    format!("{response_id}:{kind}:{row_id}")
}

fn text_part(session_id: &str, message_id: &str, kind: &str, text: &str) -> Value {
    json!({
        "partId": format!("{message_id}:{kind}"),
        "sessionId": session_id,
        "messageId": message_id,
        "type": "text",
        "text": text,
    })
}

#[allow(clippy::too_many_arguments)]
fn ensure_assistant(
    builders: &mut Vec<Builder>,
    by_response: &mut HashMap<(String, String), usize>,
    last_assistant: &mut HashMap<String, usize>,
    turn_parent: &HashMap<String, String>,
    turn_id: &str,
    response_id: &str,
    created_at: i64,
    row: &Value,
) -> usize {
    let key = (turn_id.to_string(), response_id.to_string());
    if let Some(index) = by_response.get(&key).copied() {
        return index;
    }
    let row_id = row.get("rowId").and_then(Value::as_u64).unwrap_or(0);
    // The original if/else arms were to_string() and clone() respectively (cloning a &str still yields &str, so the types did not line up),
    // and the semantics were always "pass responseId through verbatim", so convert directly.
    let message_id = response_id.to_string();
    let builder = Builder {
        message_id: message_id.clone(),
        turn_id: turn_id.to_string(),
        role: "assistant",
        created_at,
        completed_at: None,
        finish: None,
        row_id,
        parts: Vec::new(),
        terminal: false,
        model_label: None,
        parent: turn_parent.get(turn_id).cloned(),
    };
    let index = builders.len();
    builders.push(builder);
    by_response.insert(key, index);
    last_assistant.insert(turn_id.to_string(), index);
    index
}

/// `zcodeToolStateSchema` (discriminated by status; each branch is strict).
fn tool_state(row: &Value, status: &str, created_at: i64) -> Value {
    let input = tool_input(row.get("inputText").and_then(Value::as_str));
    match status {
        "running" => json!({ "status": "running", "input": input, "startedAt": created_at.max(0) }),
        "error" | "cancelled" | "denied" => {
            let error = row
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| match status {
                    "cancelled" => "tool call cancelled".to_string(),
                    "denied" => "tool call denied".to_string(),
                    _ => "tool call failed".to_string(),
                });
            json!({
                "status": "error",
                "input": input,
                "error": error,
                "startedAt": created_at.max(0),
                "completedAt": created_at.max(0),
            })
        }
        // success/completed: `output`/`title`/`metadata` have NO row source in this host —
        // tool results go into the model history only (crate README differences 12/22).
        _ => json!({
            "status": "completed",
            "input": input,
            "output": row.get("output").and_then(Value::as_str).unwrap_or("").to_string(),
            "title": row.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
            "metadata": {},
            "startedAt": created_at.max(0),
            "completedAt": created_at.max(0),
        }),
    }
}

fn tool_input(input_text: Option<&str>) -> Value {
    let Some(text) = input_text.filter(|value| !value.trim().is_empty()) else {
        return json!({});
    };
    match serde_json::from_str::<Value>(text) {
        Ok(value) if value.is_object() => value,
        // `input` is `jsonObjectSchema` (record). A non-object payload stays observable
        // under a wrapper key instead of breaking strict parsing of the whole snapshot.
        Ok(value) => json!({ "input": value }),
        Err(_) => json!({ "inputText": text }),
    }
}

fn assistant_finish(state: &str) -> &'static str {
    match state {
        "error" => "error",
        "interrupted" => "interrupted",
        _ => "stop",
    }
}

// ── whole snapshot ────────────────────────────────────────────────────────────────

/// Build the strict `zcodeSessionStateSnapshotSchema` object.
pub fn build(input: &SnapshotInput<'_>) -> Value {
    let session_id = input.session_id;
    let interactions = interactions_of(input);
    let pending_permissions = pending_permissions(&interactions);
    let has_pending_interaction = interactions
        .iter()
        .any(|item| item.get("kind").and_then(Value::as_str) == Some("permission"));
    let phase = input
        .text("phase", &["control", "phase"])
        .unwrap_or_else(|| "draft".to_string());
    let status = status_from_phase(&phase, has_pending_interaction);
    let mode = sanitize_mode(
        &input
            .text("mode", &["config", "mode"])
            .unwrap_or_else(|| DEFAULT_MODE.to_string()),
    )
    .to_string();
    let created_at = {
        let value = input.number("createdAt", &[]);
        if value > 0 {
            value
        } else {
            input.number("updatedAt", &[])
        }
    };
    let updated_at = {
        let value = input.number("updatedAt", &[]);
        if value > 0 {
            value
        } else {
            created_at
        }
    };
    let current_selection = model_selection(
        input.text("provider", &["config", "provider"]).as_deref(),
        input.text("model", &["config", "model"]).as_deref(),
        input.text("thought", &["config", "thought"]).as_deref(),
    );
    let goal = input
        .v4
        .and_then(|snapshot| snapshot.get("goal"))
        .cloned()
        .unwrap_or(Value::Null);
    let target = goal_target(Some(&goal), session_id, created_at, updated_at);
    let messages = messages_from_rows(
        input.rows,
        session_id,
        input.workspace_path,
        current_selection.as_ref(),
    );
    let mut message_items = messages;
    if let Some(limit) = input.message_limit {
        if limit > 0 && message_items.len() > limit {
            let keep_from = message_items.len() - limit;
            message_items = message_items.split_off(keep_from);
        }
    }
    let total_token_count = input
        .usage
        .and_then(|usage| usage.get("totalTokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    // contextUsed: NO SOURCE (cumulative-only accounting) ⇒ 0, and runtime.contextUsage
    // is then omitted exactly like the TS mapper (session-mapper.ts:717).
    let context_used = 0_u64;
    let context_window = input.settings.context_window;
    let last_error = last_error(input.view.get("lastError").or_else(|| {
        input
            .v4
            .and_then(|snapshot| snapshot.get("control"))
            .and_then(|control| control.get("lastError"))
    }));
    let revision = input.number("revision", &["revision"]);
    let seq = input
        .v4
        .and_then(|snapshot| snapshot.get("seq"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let active_turn_id = if phase == "running" {
        running_turn_id(input.rows).or_else(|| current_turn_id(input.rows))
    } else {
        None
    };

    let mut session = json!({
        "sessionId": session_id,
        // `zcodeSessionInfoSchema.session.workspace` is a nested ZCodeWorkspaceRef —
        // the old flat `workspacePath` key made every consumer fail strict parsing.
        "workspace": workspace_ref(input.workspace_path, input.workspace_key),
        "sessionKind": SESSION_KIND,
        "title": input.text("title", &["meta", "title"]).unwrap_or_default(),
        "mode": mode,
        "status": status,
        "createdAt": created_at.max(0),
        "updatedAt": updated_at.max(0),
    });
    if let Some(source) = input
        .text("titleSource", &["meta", "titleSource"])
        .as_deref()
        .and_then(sanitize_title_source)
    {
        session["titleSource"] = json!(source);
    }
    if let Some(selection) = &current_selection {
        session["model"] = selection.clone();
    }
    if let Some(target) = &target {
        session["target"] = target.clone();
    }

    let thought_current = input.text("thought", &["config", "thought"]);
    let thought_levels = input.settings.thought_levels.clone();
    let allowed_levels: Vec<String> = thought_levels
        .iter()
        .filter_map(|item| {
            item.get("value")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    // TS rule (session-mapper.ts:226): a current that is not in the current model's
    // option list must not be replayed.
    let thought_current = thought_current.filter(|value| allowed_levels.contains(value));
    let settings = json!({
        "model": {
            "available": input.settings.available_models.clone(),
            "current": match &current_selection {
                Some(selection) => selection.clone(),
                None => Value::Null,
            },
        },
        "thoughtLevel": {
            "enabled": !thought_levels.is_empty(),
            "available": thought_levels,
        },
        "mode": { "current": mode },
    });
    // strict schema + optional keys: `current`/`defaultLevel`/`lastUsed` are OMITTED when
    // unknown, never null and never a placeholder (TS omits them the same way).
    let settings = {
        let mut settings = settings;
        if settings["model"]["current"].is_null() {
            if let Some(object) = settings["model"].as_object_mut() {
                object.remove("current");
            }
        }
        if let Some(current) = thought_current {
            settings["thoughtLevel"]["current"] = json!(current);
        }
        if let Some(default_level) = &input.settings.thought_default {
            settings["thoughtLevel"]["defaultLevel"] = json!(default_level);
        }
        settings
    };

    let mut projection = json!({
        "sessionId": session_id,
        "status": status,
        "mode": mode,
        "turnCount": turn_count(input.rows),
        "totalTokenCount": total_token_count,
        "contextUsed": context_used,
        "contextWindow": context_window,
        "pendingPermissions": pending_permissions.clone(),
        "activeToolCalls": active_tool_calls(input.rows),
        "backgroundJobs": [],
    });
    if let Some(turn_id) = current_turn_id(input.rows) {
        projection["currentTurnId"] = json!(turn_id);
    }
    if let Some(target) = &target {
        projection["target"] = target.clone();
    }
    if let Some(error) = &last_error {
        projection["lastError"] = error.clone();
    }

    let mut runtime = json!({
        "eventSeq": seq,
        "stateRevision": revision.max(0),
        "deliveryKind": "desktop-continuous",
        "pendingRequestIds": pending_permissions
            .iter()
            .map(|item| item["requestId"].clone())
            .collect::<Vec<Value>>(),
    });
    if let Some(turn_id) = active_turn_id {
        runtime["activeTurnId"] = json!(turn_id);
    }
    if context_used > 0 && context_window > 0 {
        runtime["contextUsage"] = json!({
            "used": context_used,
            "size": context_window,
            "cost": Value::Null,
        });
    }
    let verifications = goal_verifications(Some(&goal));
    if !verifications.is_empty() {
        runtime["goalVerifications"] = json!(verifications);
        runtime["goalVerificationTimeline"] =
            json!(goal_verification_timeline(Some(&goal), updated_at));
    }

    let mut snapshot = json!({
        "protocol": { "name": PROTOCOL_NAME, "version": PROTOCOL_VERSION },
        "session": session,
        "settings": settings,
        "projection": projection,
        "runtime": runtime,
        "messages": message_items,
        // TS always writes these two arrays (session-mapper.ts:132-134).
        "todos": [],
        "slashCommands": [],
    });
    if let Some(target) = &target {
        let iteration = as_i64(goal.get("iteration")).max(0) as u64;
        snapshot["goalStats"] = goal_stats(
            target,
            context_used,
            context_window,
            tool_call_count(input.rows),
            iteration,
        );
    }
    snapshot
}

/// `zcodeWorkspaceRefSchema` (strict): `workspacePath` + `workspaceKey` are required,
/// `workspaceIdentity` optional. The identity key is `identity ?? path` repo-wide.
pub fn workspace_ref(workspace_path: &str, workspace_key: &str) -> Value {
    json!({
        "workspacePath": workspace_path,
        "workspaceIdentity": workspace_key,
        "workspaceKey": workspace_key,
    })
}

/// `zcodeSessionInfoSchema` for `listSessions` (the previous flat shape failed parsing).
pub fn session_info(fact: &Value, workspace_path: &str, workspace_key: &str) -> Value {
    let session_id = match non_empty_cloned(fact.get("sessionId").and_then(Value::as_str)) {
        Some(value) => value,
        None => return Value::Null,
    };
    let created_at = as_i64(fact.get("createdAt")).max(0);
    let updated_at = as_i64(fact.get("updatedAt")).max(0).max(created_at);
    let phase = fact
        .get("phase")
        .and_then(Value::as_str)
        .unwrap_or("completedSuccess");
    let has_pending = fact
        .get("pendingInteractions")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .any(|item| item.get("kind").and_then(Value::as_str) == Some("permission"))
        })
        .unwrap_or(false);
    let mode = sanitize_mode(
        fact.get("mode")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_MODE),
    );
    let mut info = json!({
        "sessionId": session_id,
        "workspace": workspace_ref(workspace_path, workspace_key),
        "sessionKind": SESSION_KIND,
        "title": fact.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
        "mode": mode,
        "status": status_from_phase(phase, has_pending),
        "createdAt": created_at,
        "updatedAt": updated_at,
    });
    if let Some(source) = fact
        .get("titleSource")
        .and_then(Value::as_str)
        .and_then(sanitize_title_source)
    {
        info["titleSource"] = json!(source);
    }
    if let Some(selection) = model_selection(
        fact.get("provider").and_then(Value::as_str),
        fact.get("model").and_then(Value::as_str),
        fact.get("thought").and_then(Value::as_str),
    ) {
        info["model"] = selection;
    }
    if let Some(error) = last_error(fact.get("lastError")) {
        // `ZCodeSessionInfo` has no lastError; carried in the projection snapshot only.
        let _ = error;
    }
    info
}
