//! 队列编排（host 层）：busy 入队、CAS 队列命令、autoDrain 提升。
//!
//! 与 `queue` 模块的分工：`queue` 只拥有排队事实与投影；本模块负责
//! 「何时排队 / 何时提升 / 与 CommandInbox 的 admission 交互」。
//! 拆分目的：`lib.rs` 只做方法路由，队列逻辑集中在这里，可独立测试。

use std::sync::Arc;

use serde_json::{json, Value};
use zcode_core::protocol::{CommandEnvelope, REASON_STALE_REVISION};

use crate::session::Session;
use crate::{session_not_found, HostServer, RequestOutcome, RpcError, CODE_INVALID_PARAMS};

impl HostServer {
    /// 输入型命令（sendText / sendGoalCommand）的排队判定：turn 在跑，或已有排队
    /// 输入（严格 FIFO，不许插队）。
    pub(crate) fn should_queue_input(session: &Session) -> bool {
        session.is_running() || session.queue_len() > 0
    }

    /// 入队并返回 admission ack（`inputAccepted` / delivery=queue）+ 队列投影。
    /// 幂等：同 commandId 已在队列中 → 只回放 ack，不重复入队。
    /// 注意：queued ack **不落盘**——queue 是不持久化事实；落盘会让 drain 的
    /// admission 被 inbox 当成 duplicate 而不执行。
    pub(crate) fn enqueue_text_input(
        &self,
        session: &Arc<Session>,
        envelope: &CommandEnvelope,
        text: String,
    ) -> RequestOutcome {
        if !session.queue_has_command(&envelope.command_id) {
            let disallow = envelope
                .payload
                .get("toolDisallowlist")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                });
            session.enqueue_input(
                envelope.clone(),
                text,
                envelope.client_id.clone(),
                crate::store::now_ms(),
                disallow,
            );
        }
        let revision_at_decision = session.revision();
        session.bump_revision();
        let mut post = session.project_queue_update_deferred();
        post.extend(session.project_input_routing_update_deferred());
        let ack = json!({
            "commandId": envelope.command_id,
            "status": "accepted",
            "revisionAtDecision": revision_at_decision,
            "result": {
                "type": "inputAccepted",
                "delivery": "queue",
                "inputId": envelope.command_id,
            },
        });
        RequestOutcome {
            response: ack,
            post_frames: post,
        }
    }

    /// autoDrain 提升：串行执行队首直到队列空 / 暂停 / 有新 turn 在跑。
    /// 每次提升都走真实 CommandInbox admission（不绕过幂等与 CAS）。
    pub(crate) async fn drain_queue(self: &Arc<Self>, session: Arc<Session>) {
        loop {
            if session.is_running() || !session.queue_auto_drain() || session.queue_len() == 0 {
                return;
            }
            let Some(item) = session.pop_queue_front() else {
                return;
            };
            session.bump_revision();
            for line in session.project_queue_update() {
                let _ = self.outbox.send(line);
            }
            for line in session.project_input_routing_update() {
                let _ = self.outbox.send(line);
            }
            let inbox = session.inbox.clone();
            let outcome =
                tokio::task::spawn_blocking(move || inbox.handle_envelope(item.envelope)).await;
            match outcome {
                Ok(zcode_core::inbox::Outcome::Execute(admitted)) => {
                    // 队列项按**原命令类型**执行：sendGoalCommand 排在队列里也要走
                    // goal 执行器，不能因为"从队列来"就退化成普通文本轮。
                    // Box::pin：drain → execute → run_turn_command → drain 的类型级
                    // 递归必须在这里断开（drain_after=false 保证运行时不会再进来）。
                    Box::pin(crate::commands::execute_with(
                        self,
                        Arc::clone(&session),
                        *admitted,
                        false,
                    ))
                    .await;
                }
                // duplicate/stale：该项不执行，继续下一项（不阻塞队列）。
                Ok(zcode_core::inbox::Outcome::Ack(_)) => continue,
                Err(error) => {
                    eprintln!("[zcode-host] queue drain admission failed: {error}");
                    return;
                }
            }
        }
    }

    /// 启动一个已出队的条目（sendQueuedNow）：admission → turn → drain 后续。
    pub(crate) fn spawn_queue_promotion(
        self: &Arc<Self>,
        session: &Arc<Session>,
        envelope: CommandEnvelope,
    ) {
        let server = Arc::clone(self);
        let session = Arc::clone(session);
        tokio::spawn(async move {
            let inbox = session.inbox.clone();
            let outcome =
                tokio::task::spawn_blocking(move || inbox.handle_envelope(envelope)).await;
            match outcome {
                Ok(zcode_core::inbox::Outcome::Execute(admitted)) => {
                    crate::commands::execute_with(&server, Arc::clone(&session), *admitted, false)
                        .await;
                }
                Ok(zcode_core::inbox::Outcome::Ack(_)) => {}
                Err(error) => {
                    eprintln!("[zcode-host] queue promotion admission failed: {error}");
                }
            }
            server.drain_queue(session).await;
        });
    }

    // ── 同步队列命令（CAS 由 dispatcher 侧显式校验；不排 inbox 以免被 turn 阻塞）──

    pub(crate) fn queue_edit_command(
        &self,
        session: &Arc<Session>,
        envelope: &CommandEnvelope,
    ) -> RequestOutcome {
        if let Some(stale) = Self::stale_ack(session, envelope) {
            return RequestOutcome::empty(stale);
        }
        let Some(item_id) = envelope.payload.get("queueItemId").and_then(Value::as_str) else {
            return self.reject(session, envelope, "proto.invalidPayload");
        };
        let new_text = envelope
            .payload
            .get("newText")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match session.queue_edit(item_id, new_text) {
            Ok(()) => {
                let revision_at_decision = session.revision();
                session.bump_revision();
                let mut post = session.project_queue_update_deferred();
                post.extend(session.project_input_routing_update_deferred());
                self.accept(session, envelope, revision_at_decision, post)
            }
            Err(error) => self.reject(session, envelope, error.reason_code()),
        }
    }

    pub(crate) fn queue_reorder_command(
        &self,
        session: &Arc<Session>,
        envelope: &CommandEnvelope,
    ) -> RequestOutcome {
        if let Some(stale) = Self::stale_ack(session, envelope) {
            return RequestOutcome::empty(stale);
        }
        let Some(item_id) = envelope.payload.get("queueItemId").and_then(Value::as_str) else {
            return self.reject(session, envelope, "proto.invalidPayload");
        };
        let before = envelope
            .payload
            .get("beforeQueueItemId")
            .and_then(Value::as_str);
        let moved = session.queue_reorder(item_id, before);
        let revision_at_decision = session.revision();
        let post = if moved {
            session.bump_revision();
            let mut post = session.project_queue_update_deferred();
            post.extend(session.project_input_routing_update_deferred());
            post
        } else {
            Vec::new()
        };
        // 未命中 = noop 但仍 accepted（TS warn-and-continue）。
        self.accept(session, envelope, revision_at_decision, post)
    }

    pub(crate) fn queue_delete_command(
        &self,
        session: &Arc<Session>,
        envelope: &CommandEnvelope,
    ) -> RequestOutcome {
        if let Some(stale) = Self::stale_ack(session, envelope) {
            return RequestOutcome::empty(stale);
        }
        let Some(item_id) = envelope.payload.get("queueItemId").and_then(Value::as_str) else {
            return self.reject(session, envelope, "proto.invalidPayload");
        };
        match session.queue_remove(item_id) {
            Some(_) => {
                let revision_at_decision = session.revision();
                session.bump_revision();
                let mut post = session.project_queue_update_deferred();
                post.extend(session.project_input_routing_update_deferred());
                self.accept(session, envelope, revision_at_decision, post)
            }
            None => self.noop(session, envelope, "queue.itemMissing"),
        }
    }

    pub(crate) fn queue_send_now_command(
        self: &Arc<Self>,
        session: &Arc<Session>,
        envelope: &CommandEnvelope,
    ) -> RequestOutcome {
        if let Some(stale) = Self::stale_ack(session, envelope) {
            return RequestOutcome::empty(stale);
        }
        let Some(item_id) = envelope.payload.get("queueItemId").and_then(Value::as_str) else {
            return self.reject(session, envelope, "proto.invalidPayload");
        };
        // 顺序对齐 TS：先解析队列项（缺失 → 执行失败），再检查提升占用。
        if session.queue_item(item_id).is_none() {
            return self.reject(session, envelope, "fault.command.executionFailed");
        }
        if session.is_running() {
            return self.reject(session, envelope, "guard.queuePromotionBusy");
        }
        let Some(item) = session.queue_remove(item_id) else {
            return self.reject(session, envelope, "fault.command.executionFailed");
        };
        let revision_at_decision = session.revision();
        session.bump_revision();
        let mut post = session.project_queue_update_deferred();
        post.extend(session.project_input_routing_update_deferred());
        let outcome = self.accept(session, envelope, revision_at_decision, post);
        self.spawn_queue_promotion(session, item.envelope);
        outcome
    }

    // ── ack 构造（同步命令共用）──

    fn stale_ack(session: &Session, envelope: &CommandEnvelope) -> Option<Value> {
        if envelope.base_revision != Some(session.revision()) {
            return Some(json!({
                "commandId": envelope.command_id,
                "status": "stale",
                "revisionAtDecision": session.revision(),
                "reasonCode": REASON_STALE_REVISION,
            }));
        }
        None
    }

    fn accept(
        &self,
        session: &Session,
        envelope: &CommandEnvelope,
        revision_at_decision: i64,
        post_frames: Vec<String>,
    ) -> RequestOutcome {
        let ack = json!({
            "commandId": envelope.command_id,
            "status": "accepted",
            "revisionAtDecision": revision_at_decision,
        });
        self.persist_command_ack(&session.id, &envelope.command_id, &ack);
        RequestOutcome {
            response: ack,
            post_frames,
        }
    }

    fn reject(
        &self,
        session: &Session,
        envelope: &CommandEnvelope,
        reason_code: &str,
    ) -> RequestOutcome {
        let ack = json!({
            "commandId": envelope.command_id,
            "status": "rejected",
            "revisionAtDecision": session.revision(),
            "reasonCode": reason_code,
        });
        self.persist_command_ack(&session.id, &envelope.command_id, &ack);
        RequestOutcome::empty(ack)
    }

    fn noop(
        &self,
        session: &Session,
        envelope: &CommandEnvelope,
        reason_code: &str,
    ) -> RequestOutcome {
        let ack = json!({
            "commandId": envelope.command_id,
            "status": "noop",
            "revisionAtDecision": session.revision(),
            "reasonCode": reason_code,
        });
        self.persist_command_ack(&session.id, &envelope.command_id, &ack);
        RequestOutcome::empty(ack)
    }

    // ── 路由辅助 ──

    pub(crate) fn queue_session(
        &self,
        envelope: &CommandEnvelope,
    ) -> Result<Arc<Session>, RpcError> {
        let session_id = envelope.session_id.as_deref().ok_or_else(|| {
            RpcError::new(CODE_INVALID_PARAMS, "queue commands require sessionId")
        })?;
        self.session(session_id)
            .ok_or_else(|| session_not_found(session_id))
    }
}
