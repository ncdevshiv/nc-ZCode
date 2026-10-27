//! 只读的会话查询面（`v4/conversation/fileChanges`、`fileRewindPreview`）。
//!
//! 与 `v4/conversation/rowsRange` 同族：只读、无状态、超时重发安全；读的是
//! **会话级权威事实**（turnHeader 行 + 文件台账），不是 renderer 的有界窗口。
//!
//! 陈旧读防护（协议注释点名）：`rowId` 只在一个 log epoch 内有意义，所以两个查询都
//! 要求 `baseLogEpoch` 匹配、`baseRevision` 不超过服务端水位；epoch 不匹配直接
//! `proto.staleLogEpoch`，绝不跨 epoch 拼接数据。
//!
//! `fileChanges.patches` 是**单 hunk/文件**的 unified diff（oldStart/newStart 从 0/1 起）：
//! TS 会按上下文拆多个 hunk；本 host 保证内容正确与行前缀语义，hunk 切分粒度记在
//! crate README 的差异清单。

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::rewind::{self, RewindAction};
use crate::session::Session;
use crate::store::now_ms;
use crate::{session_not_found, HostServer, RpcError, CODE_INVALID_PARAMS};

pub const REASON_STALE_LOG_EPOCH: &str = "proto.staleLogEpoch";
const DIFF_LINE_CAP: usize = 4096;

/// target 解析：rowId + 可选 entityId，落到该行的 turnId。
fn resolve_target_turn(session: &Session, params: &Value) -> Result<String, RpcError> {
    let target = params.get("target").ok_or_else(|| {
        RpcError::new(CODE_INVALID_PARAMS, "params.target: required")
            .with_data(json!({"reasonCode": "proto.invalidPayload"}))
    })?;
    let row_id = target.get("rowId").and_then(Value::as_u64).ok_or_else(|| {
        RpcError::new(CODE_INVALID_PARAMS, "params.target.rowId: required")
            .with_data(json!({"reasonCode": "proto.invalidPayload"}))
    })?;
    let entity = target.get("entityId").and_then(Value::as_str);
    let Some(row) = session
        .rows_snapshot()
        .into_iter()
        .find(|row| row.get("rowId").and_then(Value::as_u64) == Some(row_id))
    else {
        return Err(RpcError::new(
            CODE_INVALID_PARAMS,
            format!("target row {row_id} does not exist"),
        )
        .with_data(json!({"reasonCode": "proto.staleLogEpoch"})));
    };
    if let Some(entity) = entity {
        // entityId 是行的稳定实体身份：变了就等于换了实体（重编号/重放）。
        if row.get("entityId").and_then(Value::as_str) != Some(entity) {
            return Err(RpcError::new(
                CODE_INVALID_PARAMS,
                format!("target row {row_id} entity no longer matches"),
            )
            .with_data(json!({"reasonCode": "proto.staleLogEpoch"})));
        }
    }
    row.get("turnId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            RpcError::new(CODE_INVALID_PARAMS, "target row has no turnId")
                .with_data(json!({"reasonCode": "proto.invalidPayload"}))
        })
}

fn require_session(
    server: &HostServer,
    params: &Value,
) -> Result<std::sync::Arc<Session>, RpcError> {
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new(CODE_INVALID_PARAMS, "params.sessionId: required"))?;
    server
        .session(session_id)
        .ok_or_else(|| session_not_found(session_id))
}

/// 陈旧读防护：epoch 必须一致，baseRevision 不得超前于服务端水位。
fn require_read_watermark(session: &Session, params: &Value) -> Result<(), RpcError> {
    let base_log_epoch = params
        .get("baseLogEpoch")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new(CODE_INVALID_PARAMS, "params.baseLogEpoch: required"))?;
    let base_revision = params
        .get("baseRevision")
        .and_then(Value::as_i64)
        .ok_or_else(|| RpcError::new(CODE_INVALID_PARAMS, "params.baseRevision: required"))?;
    if base_log_epoch != session.log_epoch() {
        return Err(RpcError::new(
            CODE_INVALID_PARAMS,
            "query log epoch is stale; resync and retry",
        )
        .with_data(json!({"reasonCode": REASON_STALE_LOG_EPOCH})));
    }
    if base_revision > session.revision() {
        return Err(RpcError::new(
            CODE_INVALID_PARAMS,
            "query baseRevision is ahead of the server revision",
        )
        .with_data(json!({"reasonCode": zcode_core::protocol::REASON_STALE_REVISION})));
    }
    Ok(())
}

impl HostServer {
    /// `v4/conversation/fileChanges`：某一轮的文件改动汇总 + 每个文件的 diff。
    pub(crate) fn conversation_file_changes(&self, params: &Value) -> Result<Value, RpcError> {
        let session = require_session(self, params)?;
        require_read_watermark(&session, params)?;
        let turn_id = resolve_target_turn(&session, params)?;
        let records = session.file_ledger().for_turn(&turn_id);
        let mut per_path: BTreeMap<String, PathPatch> = BTreeMap::new();
        for record in &records {
            let entry = per_path.entry(record.path.clone()).or_default();
            entry.additions = entry.additions.saturating_add(record.additions);
            entry.deletions = entry.deletions.saturating_add(record.deletions);
            entry.write_count += 1;
            if !entry.tool_names.contains(&record.tool_name) {
                entry.tool_names.push(record.tool_name.clone());
            }
            entry.before = match (&entry.before, &record.before_content) {
                // 第一次改动前的快照才是"回滚目标"，后续写入的 before 已是中间态。
                (Some(existing), _) => Some(existing.clone()),
                (None, Some(before)) => Some(before.clone()),
                (None, None) => None,
            };
            entry.after = record.after_content.clone();
        }
        let files = per_path.len();
        let additions: usize = per_path.values().map(|entry| entry.additions).sum();
        let deletions: usize = per_path.values().map(|entry| entry.deletions).sum();
        let items: Vec<Value> = per_path
            .into_iter()
            .map(|(path, entry)| {
                json!({
                    "path": path,
                    "additions": entry.additions,
                    "deletions": entry.deletions,
                    "writeCount": entry.write_count,
                    "toolNames": entry.tool_names,
                    "patches": diff_hunks(entry.before.as_deref(), entry.after.as_deref()),
                })
            })
            .collect();
        let mut result = json!({
            "files": files,
            "additions": additions,
            "deletions": deletions,
            "items": items,
        });
        // `state` 来自 turnHeader 的 fileChanges 投影（rewind 成功后改写为 reverted）。
        let header_state = session
            .rows_snapshot()
            .into_iter()
            .find(|row| {
                row.get("kind").and_then(Value::as_str) == Some("turnHeader")
                    && row.get("turnId").and_then(Value::as_str) == Some(turn_id.as_str())
            })
            .and_then(|row| {
                row.get("fileChanges")
                    .and_then(|changes| changes.get("state"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        if let Some(state) = header_state {
            result["state"] = json!(state);
        }
        Ok(result)
    }

    /// `v4/conversation/usage`：会话累计 token 与模型请求/失败计数。
    pub(crate) fn conversation_usage(&self, params: &Value) -> Result<Value, RpcError> {
        let session = require_session(self, params)?;
        Ok(session.usage_report())
    }

    /// `v4/conversation/fileRewindPreview`：这一轮能不能安全回滚（零副作用）。
    pub(crate) fn conversation_file_rewind_preview(
        &self,
        params: &Value,
    ) -> Result<Value, RpcError> {
        let session = require_session(self, params)?;
        require_read_watermark(&session, params)?;
        let turn_id = resolve_target_turn(&session, params)?;
        let records = session.file_ledger().for_turn(&turn_id);
        let operations = rewind::operations(&records);
        // 与 `apply` 用同一条读取语义：缺失 = None（删除动作可安全执行），
        // 读失败（二进制/权限）= Err（preview 判 file_read_failed，绝不猜内容）。
        let root = PathBuf::from(&session.workspace_root);
        let read = move |relative: &str| -> Result<Option<String>, String> {
            match std::fs::read_to_string(root.join(relative)) {
                Ok(content) => Ok(Some(content)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.to_string()),
            }
        };
        let preview = rewind::preview(&operations, &read);
        let safe: Vec<Value> = preview
            .safe_files
            .iter()
            .map(|file| {
                json!({
                    "action": match file.action {
                        RewindAction::Restore => "restore",
                        RewindAction::Delete => "delete",
                    },
                    "operationCount": file.operation_count,
                    "path": file.path,
                    "toolNames": file.tool_names,
                })
            })
            .collect();
        let unsafe_files: Vec<Value> = preview
            .unsafe_files
            .iter()
            .map(|file| {
                let mut value = json!({
                    "operationCount": file.operation_count,
                    "path": file.path,
                    "reason": file.reason,
                    "toolNames": file.tool_names,
                });
                if let Some(message) = &file.message {
                    value["message"] = json!(message);
                }
                if let Some(hash) = &file.expected_hash {
                    value["expectedHash"] = json!(hash);
                }
                if let Some(hash) = &file.current_hash {
                    value["currentHash"] = json!(hash);
                }
                value
            })
            .collect();
        let ignored: Vec<Value> = preview
            .ignored_files
            .iter()
            .map(|file| {
                json!({
                    "operationCount": file.operation_count,
                    "path": file.path,
                    "reason": file.reason,
                    "toolNames": file.tool_names,
                })
            })
            .collect();
        Ok(json!({
            "canApply": preview.can_apply,
            "ignoredFiles": ignored,
            "safeFiles": safe,
            "unsafeFiles": unsafe_files,
        }))
    }
}

#[derive(Default)]
struct PathPatch {
    before: Option<String>,
    after: Option<String>,
    additions: usize,
    deletions: usize,
    write_count: usize,
    tool_names: Vec<String>,
}

/// 单 hunk unified diff（0/1 起如 unified diff 惯例：新建文件 oldStart=0）。
fn diff_hunks(before: Option<&str>, after: Option<&str>) -> Vec<Value> {
    let old_lines = crate::file_ledger::split_lines(before.unwrap_or(""));
    let new_lines = crate::file_ledger::split_lines(after.unwrap_or(""));
    if old_lines == new_lines {
        return Vec::new();
    }
    let (old_start, new_start) = (
        if before.is_some() { 1 } else { 0 },
        if after.is_some() { 1 } else { 0 },
    );
    let lines = if old_lines.len() > DIFF_LINE_CAP || new_lines.len() > DIFF_LINE_CAP {
        // 超大文件不做 O(n*m) 对齐：退化为"整段删除 + 整段新增"，内容仍完整。
        let mut lines: Vec<String> = old_lines.iter().map(|line| format!("-{line}")).collect();
        lines.extend(new_lines.iter().map(|line| format!("+{line}")));
        lines
    } else {
        diff_lines(&old_lines, &new_lines)
    };
    vec![json!({
        "oldStart": old_start,
        "oldLines": old_lines.len(),
        "newStart": new_start,
        "newLines": new_lines.len(),
        "lines": lines,
    })]
}

/// 经典 LCS 行对齐；输出 unified diff 的 ` ` / `-` / `+` 前缀行。
fn diff_lines(old_lines: &[&str], new_lines: &[&str]) -> Vec<String> {
    let rows = old_lines.len();
    let columns = new_lines.len();
    let mut table = vec![vec![0usize; columns + 1]; rows + 1];
    for row in (0..rows).rev() {
        for column in (0..columns).rev() {
            table[row][column] = if old_lines[row] == new_lines[column] {
                table[row + 1][column + 1] + 1
            } else {
                table[row + 1][column].max(table[row][column + 1])
            };
        }
    }
    let mut lines = Vec::new();
    let (mut row, mut column) = (0usize, 0usize);
    while row < rows && column < columns {
        if old_lines[row] == new_lines[column] {
            lines.push(format!(" {}", old_lines[row]));
            row += 1;
            column += 1;
        } else if table[row + 1][column] >= table[row][column + 1] {
            lines.push(format!("-{}", old_lines[row]));
            row += 1;
        } else {
            lines.push(format!("+{}", new_lines[column]));
            column += 1;
        }
    }
    lines.extend(old_lines[row..].iter().map(|line| format!("-{line}")));
    lines.extend(new_lines[column..].iter().map(|line| format!("+{line}")));
    lines
}

/// 供 e2e 生成时间戳用（与真实查询无关，只是避免测试里再引 now_ms）。
pub fn _timestamp_now() -> i64 {
    now_ms()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_lines_marks_context_removals_and_additions() {
        let old = vec!["a", "b", "c"];
        let new = vec!["a", "x", "c"];
        assert_eq!(
            diff_lines(&old, &new),
            vec![
                " a".to_string(),
                "-b".to_string(),
                "+x".to_string(),
                " c".to_string()
            ]
        );
    }

    #[test]
    fn created_and_deleted_files_use_zero_starts_and_full_hunks() {
        let created = diff_hunks(None, Some("one\ntwo\n"));
        assert_eq!(created.len(), 1);
        assert_eq!(created[0]["oldStart"], json!(0));
        assert_eq!(created[0]["oldLines"], json!(0));
        assert_eq!(created[0]["lines"], json!(["+one", "+two"]));

        let deleted = diff_hunks(Some("one\n"), None);
        assert_eq!(deleted[0]["newStart"], json!(0));
        assert_eq!(deleted[0]["newLines"], json!(0));
        assert_eq!(deleted[0]["lines"], json!(["-one"]));

        assert!(diff_hunks(Some("same\n"), Some("same\n")).is_empty());
    }
}
