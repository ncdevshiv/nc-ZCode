//! 反向请求编排：登记表投影、resolve/snooze 命令、工具执行处的等待。
//!
//! 与 `interaction` 模块的分工：`interaction` 只拥有登记事实；本模块负责
//! 投影、命令收口、以及「工具等用户应答」的异步等待（可被 turn 取消打断）。

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::interaction::{InteractionRegistry, Resolution};
use crate::session::Session;
use crate::{HostServer, RequestOutcome};

/// 工具执行处等待 permission 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionOutcome {
    Allowed,
    Denied,
    Cancelled,
}

/// Result of an `AskUserQuestion` interaction.
#[derive(Debug, Clone, PartialEq)]
pub enum AskUserOutcome {
    Cancelled,
    NoAnswers,
    Answered {
        answers: Value,
        annotations: Option<Value>,
    },
}

/// Result of an `ExitPlanMode` approval interaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanApproval {
    Approved,
    Feedback(String),
    Declined,
    Cancelled,
}

/// Production AskUserQuestion countdown: 60 s hidden grace, 300 s total. The E2E hook
/// (`ZCODE_E2E_ASK_USER_QUESTION_CLOCK_SCALE`) divides both values; production unset.
fn ask_user_clock() -> (u64, u64) {
    let scale = std::env::var("ZCODE_E2E_ASK_USER_QUESTION_CLOCK_SCALE")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(1);
    (60_000 / scale, 300_000 / scale)
}

/// The product shape of `AskUserQuestion.questions`: option `value` falls back to
/// `label`, `preview` is kept only when present, and `multiSelect` is emitted only
/// when true (TS's `normalizeAskUserQuestionPayloadOption`).
fn normalized_questions(input: &Value) -> Value {
    let questions = crate::interaction::questions_payload(input);
    Value::Array(
        questions
            .into_iter()
            .map(|question| {
                let mut normalized = json!({
                    "question": question.get("question").cloned().unwrap_or(Value::Null),
                    "header": question.get("header").cloned().unwrap_or(Value::Null),
                    "options": question
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|options| {
                            options
                                .iter()
                                .map(|option| {
                                    let label = option.get("label").cloned().unwrap_or(Value::Null);
                                    let value = option
                                        .get("value")
                                        .cloned()
                                        .unwrap_or_else(|| label.clone());
                                    let mut item = json!({
                                        "label": label,
                                        "description": option.get("description").cloned().unwrap_or(Value::Null),
                                        "value": value,
                                    });
                                    if let Some(preview) = option.get("preview") {
                                        item["preview"] = preview.clone();
                                    }
                                    item
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default(),
                });
                if question.get("multiSelect").and_then(Value::as_bool) == Some(true) {
                    normalized["multiSelect"] = json!(true);
                }
                normalized
            })
            .collect(),
    )
}

/// Extract and normalise `content.answers` (the modern shape) from a v4 answer.
/// When the client sends the single-question compatibility shape
/// (`content.answer` / `content.answer_0` / top-level `answer`), it is keyed by
/// `fallback_question` so the model still sees `"question"="answer"`.
fn answers_from_answer(answer: &Value, fallback_question: Option<&str>) -> (Value, Option<Value>) {
    let content = answer.get("content").filter(|value| value.is_object());
    let annotations = content
        .and_then(|value| value.get("annotations"))
        .cloned()
        .or_else(|| answer.get("annotations").cloned());
    let raw = content
        .and_then(|value| value.get("answers"))
        .or_else(|| answer.get("answers"));
    let mut normalized = serde_json::Map::new();
    if let Some(raw) = raw.and_then(Value::as_object) {
        for (question, value) in raw {
            if let Some(text) = normalize_answer_value(Some(value)) {
                normalized.insert(question.clone(), json!(text));
            }
        }
    }
    if normalized.is_empty() {
        if let Some(question) = fallback_question {
            let single = content
                .and_then(|value| value.get("answer").or_else(|| value.get("answer_0")))
                .or_else(|| answer.get("answer"))
                .or_else(|| answer.get("freeText"))
                .or_else(|| answer.get("optionId"));
            if let Some(text) = normalize_answer_value(single) {
                normalized.insert(question.to_string(), json!(text));
            }
        }
    }
    (Value::Object(normalized), annotations)
}

/// Plan approval: approve is the only pass value; any other non-empty answer is
/// feedback used as the denial reason; no answer is a decline.
fn approval_outcome(answer: &Value) -> PlanApproval {
    let (answers, _) =
        answers_from_answer(answer, Some(crate::tools::plan::PLAN_APPROVAL_QUESTION));
    let raw = answers
        .get(crate::tools::plan::PLAN_APPROVAL_QUESTION)
        .cloned()
        .or_else(|| answer.get("freeText").cloned())
        .or_else(|| answer.get("optionId").cloned());
    match normalize_answer_value(raw.as_ref()).as_deref() {
        Some(crate::tools::plan::PLAN_APPROVAL_APPROVE) => PlanApproval::Approved,
        Some(feedback) => PlanApproval::Feedback(feedback.to_string()),
        None => PlanApproval::Declined,
    }
}

/// Normalise one answer value the way TS's `normalizeUserInputAnswer` does:
/// arrays are joined with `", "`, each entry trimmed, empty entries dropped; a
/// string is trimmed; an empty result is `None`.
pub fn normalize_answer_value(value: Option<&Value>) -> Option<String> {
    let render = |item: &Value| -> Option<String> {
        item.as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    match value? {
        Value::String(_) => render(value?),
        Value::Array(items) => {
            let joined = items
                .iter()
                .filter_map(render)
                .collect::<Vec<_>>()
                .join(", ");
            (!joined.is_empty()).then_some(joined)
        }
        Value::Null => None,
        other => render(&Value::String(other.to_string())),
    }
}

impl Session {
    /// `pendingInteractions`：broker 交互（permission/userInput）+ workspace hook 评审。
    /// 评审不是 broker 交互（用 `respondWorkspaceHookReview` 而非 resolveInteraction
    /// 应答），但它同样占用 pending 面，UI 靠它渲染审核入口。
    pub fn interactions_projection(&self) -> Value {
        let mut list = self
            .interactions_registry()
            .list_projection()
            .as_array()
            .cloned()
            .unwrap_or_default();
        if let Some(payload) = self.hook_review_interaction() {
            list.push(json!({
                "interactionId": payload["interactionId"],
                "kind": "workspaceHookReview",
                "anchorRowId": Value::Null,
                "createdAt": payload["createdAt"],
                "payload": payload,
            }));
        }
        Value::Array(list)
    }

    fn interactions_registry(&self) -> std::sync::MutexGuard<'_, InteractionRegistry> {
        match self.interactions_lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// `pendingInteractions` 变更投影（revision-bearing patch）+ revision 前移。
    pub fn project_interactions_update(&self) -> Vec<String> {
        let list = self.interactions_projection();
        let patch = json!({
            "pendingInteractions": list,
            "revision": self.bump_revision(),
        });
        self.project_deltas(vec![crate::rows::state_updated(patch)], false)
    }

    pub fn full_access_granted(&self) -> bool {
        self.full_access_flag()
    }

    /// permission 交互关闭后投影（resolved/cancelled 都走同一条）。
    fn close_interaction(&self, interaction_id: &str) {
        let removed = self.interactions_registry().remove(interaction_id);
        self.take_waiter(interaction_id);
        if removed.is_some() {
            self.emit_frames(self.project_interactions_update());
        }
    }

    /// 打开 permission 交互并等待应答；turn 取消 → Cancelled 且关闭交互。
    /// headless（无订阅者）→ Denied（fail-closed，不产生无人可答的登记）。
    pub async fn request_permission(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        summary: &str,
        detail: &Value,
    ) -> PermissionOutcome {
        if !self.has_subscribers() {
            return PermissionOutcome::Denied;
        }
        let anchor = self.tool_row_id(tool_call_id);
        let (sender, mut receiver) = oneshot::channel::<Value>();
        let interaction_id = self.interactions_registry().open_permission(
            tool_call_id,
            tool_name,
            summary,
            detail,
            anchor,
            crate::store::now_ms(),
            !self.full_access_flag(),
        );
        self.register_waiter(&interaction_id, sender);
        self.emit_frames(self.project_interactions_update());
        self.emit_frames(self.project_tool_pending_approval(tool_call_id, &interaction_id));

        loop {
            tokio::select! {
                answer = &mut receiver => {
                    return match answer {
                        Ok(answer) => match Resolution::permission_from(&answer) {
                            (true, grant_full_access) => {
                                if grant_full_access {
                                    self.set_full_access_flag(true);
                                }
                                PermissionOutcome::Allowed
                            }
                            (false, _) => PermissionOutcome::Denied,
                        },
                        Err(_) => PermissionOutcome::Cancelled,
                    };
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    if self.is_cancelled() {
                        self.close_interaction(&interaction_id);
                        return PermissionOutcome::Cancelled;
                    }
                }
            }
        }
    }

    /// AskUserQuestion: open one userInput interaction with the product payload and
    /// wait for `resolveInteraction`. Headless/cancelled never fabricates an answer.
    pub async fn request_user_answers(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        input: &Value,
    ) -> AskUserOutcome {
        if !self.has_subscribers() {
            return AskUserOutcome::NoAnswers;
        }
        let payload = json!({
            "kind": "userInput",
            "toolName": tool_name,
            "toolCallId": tool_call_id,
            "prompt": "AskUserQuestion pauses execution to collect answers from the user",
            "freeText": true,
            "schema": {"toolName": tool_name},
            "questions": normalized_questions(input),
        });
        let anchor = self.tool_row_id(tool_call_id);
        let now = crate::store::now_ms();
        let (hidden_ms, deadline_ms) = ask_user_clock();
        let auto = json!({
            "state": "hiddenGrace",
            "startedAt": now,
            "visibleAt": now.saturating_add(hidden_ms as i64),
            "deadlineAt": now.saturating_add(deadline_ms as i64),
        });
        let (sender, mut receiver) = oneshot::channel::<Value>();
        let interaction_id =
            self.interactions_registry()
                .open_user_input_payload(payload, anchor, now, Some(auto));
        self.register_waiter(&interaction_id, sender);
        self.emit_frames(self.project_interactions_update());
        loop {
            tokio::select! {
                answer = &mut receiver => {
                    let Ok(answer) = answer else {
                        return AskUserOutcome::Cancelled;
                    };
                    let first_question = crate::interaction::questions_payload(input)
                        .first()
                        .and_then(|question| question.get("question"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let (answers, annotations) =
                        answers_from_answer(&answer, first_question.as_deref());
                    if answers.as_object().map(|items| items.is_empty()).unwrap_or(true) {
                        return AskUserOutcome::NoAnswers;
                    }
                    return AskUserOutcome::Answered { answers, annotations };
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    if self.is_cancelled() {
                        self.close_interaction(&interaction_id);
                        return AskUserOutcome::Cancelled;
                    }
                    let now = crate::store::now_ms();
                    let mut changed = false;
                    let mut expired = false;
                    if let Ok(mut registry) = self.interactions_lock() {
                        if let Some(auto) = registry.auto_resolution(&interaction_id) {
                            match auto["state"].as_str() {
                                Some("hiddenGrace")
                                    if auto["visibleAt"].as_i64().unwrap_or(i64::MAX) <= now =>
                                {
                                    registry.set_auto_resolution(
                                        &interaction_id,
                                        json!({
                                            "state": "visibleCountdown",
                                            "startedAt": auto["startedAt"],
                                            "visibleAt": auto["visibleAt"],
                                            "deadlineAt": auto["deadlineAt"],
                                        }),
                                    );
                                    changed = true;
                                }
                                Some("visibleCountdown")
                                    if auto["deadlineAt"].as_i64().unwrap_or(i64::MAX) <= now =>
                                {
                                    expired = true;
                                }
                                _ => {}
                            }
                        }
                    }
                    if changed {
                        self.emit_frames(self.project_interactions_update());
                    }
                    if expired {
                        self.close_interaction(&interaction_id);
                        return AskUserOutcome::NoAnswers;
                    }
                }
            }
        }
    }

    /// ExitPlanMode: one approval question; approve is the only option, free-text is
    /// returned as feedback. A headless host declines (a plan nobody approved is not
    /// a plan).
    pub async fn request_plan_approval(&self, tool_call_id: &str, input: &Value) -> PlanApproval {
        if !self.has_subscribers() {
            return PlanApproval::Declined;
        }
        let reason = "ExitPlanMode changes session mode after user plan approval";
        let payload = json!({
            "kind": "userInput",
            "toolName": "ExitPlanMode",
            "toolCallId": tool_call_id,
            "prompt": reason,
            "freeText": true,
            "schema": {"interaction": "plan_approval", "toolName": "ExitPlanMode"},
            "input": input,
            "questions": crate::tools::plan::approval_questions(reason),
        });
        let anchor = self.tool_row_id(tool_call_id);
        let (sender, mut receiver) = oneshot::channel::<Value>();
        let interaction_id = self.interactions_registry().open_user_input_payload(
            payload,
            anchor,
            crate::store::now_ms(),
            None,
        );
        self.register_waiter(&interaction_id, sender);
        self.emit_frames(self.project_interactions_update());
        loop {
            tokio::select! {
                answer = &mut receiver => {
                    let Ok(answer) = answer else {
                        return PlanApproval::Cancelled;
                    };
                    return approval_outcome(&answer);
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    if self.is_cancelled() {
                        self.close_interaction(&interaction_id);
                        return PlanApproval::Cancelled;
                    }
                }
            }
        }
    }

    /// resolve：先到先得；返回是否真正命中（未知/已应答 = false，幂等）。
    pub fn resolve_interaction(&self, interaction_id: &str, answer: &Value) -> bool {
        let resolution = self.interactions_registry().resolve(interaction_id, answer);
        let Some(resolution) = resolution else {
            return false;
        };
        if let Resolution::Permission {
            grant_full_access: true,
            ..
        } = resolution
        {
            self.set_full_access_flag(true);
        }
        if let Some(sender) = self.take_waiter(interaction_id) {
            let _ = sender.send(answer.clone());
        }
        true
    }

    /// snooze：只对带倒计时的交互生效。
    pub fn snooze_interaction(&self, interaction_id: &str, now: i64) -> bool {
        self.interactions_registry().snooze(interaction_id, now)
    }

    /// toolCall 行的 rowId（permission 的 anchorRowId）。
    pub fn tool_row_id(&self, tool_call_id: &str) -> Option<u64> {
        self.rows_snapshot()
            .iter()
            .find(|row| {
                row.get("kind").and_then(Value::as_str) == Some("toolCall")
                    && row.get("toolCallId").and_then(Value::as_str) == Some(tool_call_id)
            })
            .and_then(|row| row.get("rowId").and_then(Value::as_u64))
    }
}

impl Resolution {
    /// permission 应答语义（未知 option 一律拒绝，fail-closed）。
    fn permission_from(answer: &Value) -> (bool, bool) {
        match answer.get("optionId").and_then(Value::as_str) {
            Some("allowOnce") => (true, false),
            Some("fullAccess") => (true, true),
            _ => (false, false),
        }
    }
}

impl HostServer {
    /// resolveInteraction：未知/晚到仍 accepted（幂等成功，TS 同语义）。
    pub(crate) fn resolve_interaction_command(
        &self,
        session: &Arc<Session>,
        envelope: &nc_code_core::protocol::CommandEnvelope,
    ) -> RequestOutcome {
        let Some(interaction_id) = envelope
            .payload
            .get("interactionId")
            .and_then(Value::as_str)
        else {
            return self.accept_without_frames(session, envelope);
        };
        let answer = envelope
            .payload
            .get("answer")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let resolved = session.resolve_interaction(interaction_id, &answer);
        let post = if resolved {
            session.project_interactions_update()
        } else {
            Vec::new()
        };
        self.accept_with(session, envelope, post)
    }

    /// snoozeInteractionAutoResolution：无倒计时/未知 id 幂等 accepted。
    pub(crate) fn snooze_interaction_command(
        &self,
        session: &Arc<Session>,
        envelope: &nc_code_core::protocol::CommandEnvelope,
    ) -> RequestOutcome {
        let Some(interaction_id) = envelope
            .payload
            .get("interactionId")
            .and_then(Value::as_str)
        else {
            return self.accept_without_frames(session, envelope);
        };
        let snoozed = session.snooze_interaction(interaction_id, crate::store::now_ms());
        let post = if snoozed {
            session.project_interactions_update()
        } else {
            Vec::new()
        };
        self.accept_with(session, envelope, post)
    }

    fn accept_with(
        &self,
        session: &Session,
        envelope: &nc_code_core::protocol::CommandEnvelope,
        post_frames: Vec<String>,
    ) -> RequestOutcome {
        let ack = json!({
            "commandId": envelope.command_id,
            "status": "accepted",
            "revisionAtDecision": session.revision(),
        });
        self.persist_ack_for(session, envelope, &ack);
        RequestOutcome {
            response: ack,
            post_frames,
        }
    }

    fn accept_without_frames(
        &self,
        session: &Session,
        envelope: &nc_code_core::protocol::CommandEnvelope,
    ) -> RequestOutcome {
        self.accept_with(session, envelope, Vec::new())
    }

    fn persist_ack_for(
        &self,
        session: &Session,
        envelope: &nc_code_core::protocol::CommandEnvelope,
        ack: &Value,
    ) {
        if let Err(error) = session.persist_ack(&envelope.command_id, ack) {
            eprintln!(
                "[zcode-host] persist_ack failed for {}: {error}",
                envelope.command_id
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_answer_trims_and_drops_empties() {
        assert_eq!(
            normalize_answer_value(Some(&json!("  hi  "))),
            Some("hi".to_string())
        );
        assert_eq!(normalize_answer_value(Some(&json!("   "))), None);
        assert_eq!(normalize_answer_value(None), None);
        assert_eq!(normalize_answer_value(Some(&json!(null))), None);
    }

    #[test]
    fn normalize_answer_joins_arrays_like_ts() {
        let joined = normalize_answer_value(Some(&json!([" a ", "", "b", "  "])));
        assert_eq!(joined, Some("a, b".to_string()));
        let empty = normalize_answer_value(Some(&json!(["", " "])));
        assert_eq!(empty, None);
    }

    #[test]
    fn normalized_questions_fallback_value_to_label() {
        let input = json!({
            "questions": [{
                "question": "Q?",
                "header": "H",
                "options": [
                    {"label": "Opt A"},
                    {"label": "Opt B", "value": "b", "preview": "prev"},
                ],
                "multiSelect": true,
            }]
        });
        let normalized = normalized_questions(&input);
        let q = &normalized[0];
        assert_eq!(q["question"], "Q?");
        assert_eq!(q["options"][0]["value"], "Opt A");
        assert_eq!(q["options"][1]["value"], "b");
        assert_eq!(q["options"][1]["preview"], "prev");
        assert_eq!(q["multiSelect"], true);
        // multiSelect omitted when false
        let input2 = json!({
            "questions": [{ "question": "Q", "header": "H", "options": [{"label": "X"}], "multiSelect": false }]
        });
        let n2 = normalized_questions(&input2);
        assert!(n2[0].get("multiSelect").is_none());
    }

    #[test]
    fn permission_and_plan_outcomes_are_distinct() {
        assert_ne!(PermissionOutcome::Allowed, PermissionOutcome::Denied);
        assert_ne!(PermissionOutcome::Denied, PermissionOutcome::Cancelled);
        assert_eq!(format!("{:?}", PlanApproval::Approved), "Approved");
        assert_eq!(
            format!("{:?}", PlanApproval::Feedback("x".into())),
            "Feedback(\"x\")"
        );
    }

    #[test]
    fn approval_outcome_only_approve_is_pass() {
        let question = crate::tools::plan::PLAN_APPROVAL_QUESTION;
        let approve = json!({
            "content": {
                "answers": { (question): "approve" }
            }
        });
        assert_eq!(approval_outcome(&approve), PlanApproval::Approved);
        let feedback = json!({
            "content": {
                "answers": { (question): "needs more" }
            }
        });
        assert_eq!(
            approval_outcome(&feedback),
            PlanApproval::Feedback("needs more".to_string())
        );
        let empty = json!({});
        assert_eq!(approval_outcome(&empty), PlanApproval::Declined);
    }
}
