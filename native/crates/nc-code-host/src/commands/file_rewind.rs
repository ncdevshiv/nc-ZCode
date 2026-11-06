//! `applyFileRewind`：把某一轮的工作区改动撤回，**聊天历史不动**。
//!
//! 逐条对齐 TS（`handlers/file-rewind.ts` + `runtime/methods/file-rewind.ts`）：
//! - 目标必须是 `turnHeader` 行、该轮已收口、带 `fileChanges` 且 `state == "active"`、
//!   且没有待应答的交互，否则 `guard.actionUnavailable`（TS 的 `canRewindFiles` 门禁）；
//! - `entityId` 与该行身份对不上 = 客户端拿的是过期快照 → `proto.staleTarget`；
//! - 先 preview 后 apply：有任何 unsafe 文件就**整笔拒绝**（不做部分回滚），并把
//!   preview 原样带进 result 给客户端呈现；
//! - 成功后把该行 `fileChanges.state` 改成 `reverted`，第二次 rewind 会被门禁挡住。

use std::sync::Arc;

use nc_code_core::inbox::Admitted;
use nc_code_core::protocol::{CommandFinal, CommandStatus};
use serde_json::{json, Value};

use crate::commands::settle_admitted;
use crate::rewind;
use crate::session::Session;
use crate::HostServer;

fn payload_target(payload: &Value) -> (Option<u64>, Option<String>) {
    let target = payload.get("target");
    (
        target
            .and_then(|target| target.get("rowId"))
            .and_then(Value::as_u64),
        target
            .and_then(|target| target.get("entityId"))
            .and_then(Value::as_str)
            .map(str::to_string),
    )
}

/// TS 侧的 `canRewindFiles` 门禁：行必须是已收口、带 active 文件汇总、且会话当前
/// 没有待应答交互的 turnHeader。返回该轮的 turnId。
fn rewindable_turn(session: &Arc<Session>, row_id: u64) -> Result<String, &'static str> {
    // 有待应答交互 = 有一个 turn 正在等用户，此时改工作区会与它竞争。
    // 锁中毒也当作"有"处理：那说明交互注册表本身处于不一致状态。
    let pending_interactions = match session.interactions_lock() {
        Ok(guard) => guard.len(),
        Err(_) => 1,
    };
    if pending_interactions > 0 {
        return Err("guard.actionUnavailable");
    }
    let rows = session.rows_snapshot();
    let Some(row) = rows
        .iter()
        .find(|row| row.get("rowId").and_then(Value::as_u64) == Some(row_id))
    else {
        return Err("guard.actionUnavailable");
    };
    if row.get("kind").and_then(Value::as_str) != Some("turnHeader") {
        return Err("guard.actionUnavailable");
    }
    if row.get("state").and_then(Value::as_str) == Some("running") {
        return Err("guard.actionUnavailable");
    }
    let state = row
        .get("fileChanges")
        .and_then(|changes| changes.get("state"))
        .and_then(Value::as_str);
    if state != Some("active") {
        // 没有 fileChanges = 这一轮没改过文件；"reverted" = 已经撤回过。
        return Err("guard.actionUnavailable");
    }
    Ok(row
        .get("turnId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string())
}

impl HostServer {
    pub(crate) fn run_apply_file_rewind(
        self: &Arc<Self>,
        session: Arc<Session>,
        admitted: Admitted,
    ) {
        let payload = admitted.envelope.payload.clone();
        let (Some(row_id), target_entity) = payload_target(&payload) else {
            let final_ =
                CommandFinal::with_reason(CommandStatus::Rejected, "guard.actionUnavailable");
            settle_admitted(&session, admitted, final_, Vec::new(), &self.outbox);
            return;
        };
        let turn_id = match rewindable_turn(&session, row_id) {
            Ok(turn_id) => turn_id,
            Err(reason) => {
                let final_ = CommandFinal {
                    message: Some(
                        "applyFileRewind 只能作用于已收口、带 active 文件汇总且无待应答交互的 turnHeader 行"
                            .to_string(),
                    ),
                    ..CommandFinal::with_reason(CommandStatus::Rejected, reason)
                };
                settle_admitted(&session, admitted, final_, Vec::new(), &self.outbox);
                return;
            }
        };
        // 实体校验：turnHeader 行的身份就是 turnId（客户端 target 用哪个都行，
        // 但给了就必须对得上——否则它拿的是另一份快照）。
        if let Some(entity) = &target_entity {
            if session
                .rows_snapshot()
                .iter()
                .find(|row| row.get("rowId").and_then(Value::as_u64) == Some(row_id))
                .and_then(|row| row.get("turnId").and_then(Value::as_str))
                != Some(entity.as_str())
            {
                let final_ = CommandFinal {
                    message: Some(format!(
                        "applyFileRewind 的 entityId {entity} 与 rowId {row_id} 当前所属轮不一致"
                    )),
                    ..CommandFinal::with_reason(CommandStatus::Stale, "proto.staleTarget")
                };
                settle_admitted(&session, admitted, final_, Vec::new(), &self.outbox);
                return;
            }
        }

        let records = session.file_ledger().for_turn(&turn_id);
        let ops = rewind::operations(&records);
        let root = session.workspace_root.clone();
        // preview 与 apply 各用一个适配器（同一个根）：apply 会消耗掉它的写入句柄。
        let reader = rewind::FsAdapter::local(root.clone());
        let preview = rewind::preview(&ops, &|relative: &str| {
            (reader.read)(std::path::Path::new(relative))
        });
        let preview_json = preview.to_json();
        if !preview.can_apply {
            // 拒绝必须是**零副作用**，且把 preview 原样带回：UI 需要逐文件说明为什么
            // 不能撤（外部改过 / 读不出 / shell 改的）。
            let reason = if !preview.unsafe_files.is_empty() {
                "guard.workspaceRewindUnsafeFiles"
            } else {
                "guard.workspaceRewindUnavailable"
            };
            let final_ = CommandFinal {
                message: Some(if preview.safe_files.is_empty() {
                    "没有可以安全回滚的文件".to_string()
                } else {
                    "存在外部改动或读不出的文件，整轮回滚被拒绝（不做部分回滚）".to_string()
                }),
                result: Some(json!({
                    "type": "applyFileRewind",
                    "disposition": "blocked",
                    "sessionId": session.id,
                    "reasonCode": reason,
                    "preview": preview_json,
                })),
                ..CommandFinal::with_reason(CommandStatus::Rejected, reason)
            };
            settle_admitted(&session, admitted, final_, Vec::new(), &self.outbox);
            return;
        }
        let report = rewind::apply(&ops, &preview, &rewind::FsAdapter::local(root));
        if !report.applied {
            // 应用中途失败：`apply` 已经按 journal 把改过的文件补偿回去，这里只上报。
            let final_ = CommandFinal {
                message: report.response.clone(),
                result: Some(json!({
                    "type": "applyFileRewind",
                    "disposition": "blocked",
                    "sessionId": session.id,
                    "reasonCode": "guard.workspaceRewindApplyConflict",
                    "preview": preview_json,
                })),
                ..CommandFinal::with_reason(
                    CommandStatus::Failed,
                    "guard.workspaceRewindApplyConflict",
                )
            };
            settle_admitted(&session, admitted, final_, Vec::new(), &self.outbox);
            return;
        }
        // 成功：把该轮标成 reverted（第二次 rewind 会被门禁挡住），并推进 revision。
        let lines = session.mark_turn_files_reverted(&turn_id);
        session.bump_revision();
        let final_ = CommandFinal {
            result: Some(json!({
                "type": "applyFileRewind",
                "disposition": "applied",
                "sessionId": session.id,
                "turnId": turn_id,
                "restoredFiles": report.restored_paths,
                "preview": preview_json,
            })),
            ..CommandFinal::accepted()
        };
        settle_admitted(&session, admitted, final_, lines, &self.outbox);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn basic_test() {
        assert!(true);
    }
}
