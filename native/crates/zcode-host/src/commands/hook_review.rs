//! workspace hook 评审命令组：`respondWorkspaceHookReview` /
//! `toggleWorkspaceHookReviewItem` / `revokeWorkspaceHookTrust` /
//! `requestWorkspaceHookReview`。
//!
//! 四条命令都作用在**同一份不可变评审快照**上（sessionId + workspaceIdentity +
//! bundleDigest + flow generation）。校验失败一律 rejected 且**不写任何东西**；
//! 状态变更（信任/撤销/开关）之后必须投影 `workspaceHookAdmission` +
//! `pendingInteractions`（评审交互可能被收口或被 supersede）。

use std::sync::Arc;

use serde_json::{json, Value};
use zcode_core::inbox::Admitted;
use zcode_core::protocol::{CommandFinal, CommandStatus};

use crate::commands::settle_admitted;
use crate::session::Session;
use crate::store::now_ms;
use crate::workspace_hooks::review_target_from_payload;
use crate::HostServer;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HookReviewKind {
    Respond,
    Toggle,
    Revoke,
    Request,
}

fn rejected(reason: &'static str) -> CommandFinal {
    CommandFinal::with_reason(CommandStatus::Rejected, reason)
}

fn invalid_payload() -> CommandFinal {
    rejected("proto.invalidPayload")
}

fn string_array(value: Option<&Value>) -> Option<Vec<String>> {
    let items = value?.as_array()?;
    if items.is_empty() {
        return None;
    }
    items
        .iter()
        .map(|item| item.as_str().map(str::to_string))
        .collect()
}

impl HostServer {
    pub(crate) fn run_hook_review_command(
        &self,
        session: &Arc<Session>,
        admitted: Admitted,
        kind: HookReviewKind,
    ) {
        let payload = admitted.envelope.payload.clone();
        let (final_, lines) = match kind {
            HookReviewKind::Respond => respond(session, &payload),
            HookReviewKind::Toggle => toggle(session, &payload),
            HookReviewKind::Revoke => revoke(session, &payload),
            HookReviewKind::Request => request(session, &payload),
        };
        settle_admitted(session, admitted, final_, lines, &self.outbox);
    }
}

fn respond(session: &Arc<Session>, payload: &Value) -> (CommandFinal, Vec<String>) {
    let Some(target) = review_target_from_payload(payload) else {
        return (invalid_payload(), Vec::new());
    };
    let decision = &payload["decision"];
    if decision["action"].as_str() != Some("trust_selected") {
        return (invalid_payload(), Vec::new());
    }
    let Some(review_item_ids) = string_array(decision.get("reviewItemIds")) else {
        return (invalid_payload(), Vec::new());
    };
    match session.hooks().respond(&target, &review_item_ids) {
        Ok(outcome) => {
            // 部分授权后还有待审项 → 立刻换一代重开（旧代不能再授权剩余条目）；
            // 全部授权 → pending 清零，交互被收口。
            session.refresh_hook_review(now_ms(), true);
            let final_ = CommandFinal {
                result: Some(json!({
                    "type": "respondWorkspaceHookReview",
                    "reviewItemIds": outcome.review_item_ids,
                })),
                ..CommandFinal::accepted()
            };
            (final_, session.project_hook_state_update())
        }
        Err(error) => {
            session.sync_hook_review();
            // 失败也可能收口了 flow（策略拒绝 → cancelled），投影要跟上。
            let lines = session.project_hook_state_update();
            (rejected(error.reason_code()), lines)
        }
    }
}

fn toggle(session: &Arc<Session>, payload: &Value) -> (CommandFinal, Vec<String>) {
    let Some(target) = review_target_from_payload(payload) else {
        return (invalid_payload(), Vec::new());
    };
    let Some(review_item_id) = payload
        .get("reviewItemId")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
    else {
        return (invalid_payload(), Vec::new());
    };
    let Some(enabled) = payload.get("enabled").and_then(Value::as_bool) else {
        return (invalid_payload(), Vec::new());
    };
    match session.hooks().toggle(&target, review_item_id, enabled) {
        Ok(()) => {
            // 开关改变 bundle 摘要：pending flow 必须 supersede，旧代应答随之失效。
            session.ensure_hook_review(now_ms());
            let final_ = CommandFinal {
                result: Some(json!({
                    "type": "toggleWorkspaceHookReviewItem",
                    "reviewItemId": review_item_id,
                    "enabled": enabled,
                })),
                ..CommandFinal::accepted()
            };
            (final_, session.project_hook_state_update())
        }
        Err(error) => (rejected(error.reason_code()), Vec::new()),
    }
}

fn revoke(session: &Arc<Session>, payload: &Value) -> (CommandFinal, Vec<String>) {
    let identity = payload
        .get("workspaceIdentity")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let digests: Vec<String> =
        if let Some(review_item_ids) = string_array(payload.get("reviewItemIds")) {
            // flow target 变体：先校验 generation/快照，再把 reviewItemId 解析成摘要。
            let Some(target) = review_target_from_payload(payload) else {
                return (invalid_payload(), Vec::new());
            };
            if let Err(error) = session.hooks().validate_review_target(&target) {
                return (rejected(error.reason_code()), Vec::new());
            }
            let Some(snapshot) = session.hooks().snapshot() else {
                return (rejected("workspace_hooks_snapshot_mismatch"), Vec::new());
            };
            let mut resolved = Vec::new();
            for review_item_id in review_item_ids {
                match snapshot
                    .hooks
                    .iter()
                    .find(|entry| entry.review_item_id == review_item_id)
                {
                    Some(entry) => resolved.push(entry.hook_declaration_digest.clone()),
                    None => return (rejected("workspace_hooks_snapshot_mismatch"), Vec::new()),
                }
            }
            resolved
        } else if let Some(digests) = string_array(payload.get("hookDeclarationDigests")) {
            if digests
                .iter()
                .any(|digest| !crate::workspace_hooks::flow::is_sha256(digest))
            {
                return (invalid_payload(), Vec::new());
            }
            digests
        } else {
            return (invalid_payload(), Vec::new());
        };
    match session.hooks().revoke(identity, &digests) {
        Ok(flow_was_pending) => {
            if flow_was_pending {
                // 撤销改变条目状态：重开评审（新 generation），旧代应答失效。
                session.refresh_hook_review(now_ms(), true);
            }
            let final_ = CommandFinal {
                result: Some(json!({
                    "type": "revokeWorkspaceHookTrust",
                    "hookDeclarationDigests": digests,
                })),
                ..CommandFinal::accepted()
            };
            (final_, session.project_hook_state_update())
        }
        Err(error) => (rejected(error.reason_code()), Vec::new()),
    }
}

fn request(session: &Arc<Session>, payload: &Value) -> (CommandFinal, Vec<String>) {
    let identity = payload
        .get("workspaceIdentity")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let bundle_digest = payload
        .get("bundleDigest")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let current = session.hooks().snapshot();
    let matches = current.as_ref().is_some_and(|snapshot| {
        snapshot.workspace_identity == identity && snapshot.bundle_digest == bundle_digest
    });
    if !matches {
        return (rejected("workspace_hooks_snapshot_mismatch"), Vec::new());
    }
    // 无待审项 = 安全 no-op；已有 pending flow 时复用（不重置 deadline）。
    let opened = session.ensure_hook_review(now_ms());
    let final_ = CommandFinal {
        result: Some(json!({
            "type": "requestWorkspaceHookReview",
            "opened": opened,
        })),
        ..CommandFinal::accepted()
    };
    let lines = if opened {
        session.project_hook_state_update()
    } else {
        Vec::new()
    };
    (final_, lines)
}
