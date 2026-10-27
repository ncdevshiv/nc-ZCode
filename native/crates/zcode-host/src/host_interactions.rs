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
        envelope: &zcode_core::protocol::CommandEnvelope,
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
        envelope: &zcode_core::protocol::CommandEnvelope,
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
        envelope: &zcode_core::protocol::CommandEnvelope,
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
        envelope: &zcode_core::protocol::CommandEnvelope,
    ) -> RequestOutcome {
        self.accept_with(session, envelope, Vec::new())
    }

    fn persist_ack_for(
        &self,
        session: &Session,
        envelope: &zcode_core::protocol::CommandEnvelope,
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
