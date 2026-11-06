//! ConversationRow / ConversationDelta 的 JSON 构造，形状逐字段对齐
//! `packages/shared/src/zcode-protocol-v4/rows.ts` 与 `delta.ts`。
//!
//! 本 increment 投影三种行（turnHeader / userInput / assistantText）与五个 delta op
//! 全部：row.removed 由分支截断（editUserQuery / retryTurn）构造。
//! 结构性规则与服务端一致：结构变化换整行（row.upserted），文本增长用
//! row.delta；rowId 单调递增、永不复用。

use serde_json::{json, Value};

pub const OP_ROW_APPENDED: &str = "row.appended";
pub const OP_ROW_UPSERTED: &str = "row.upserted";
pub const OP_ROW_DELTA: &str = "row.delta";
pub const OP_ROW_REMOVED: &str = "row.removed";
pub const OP_STATE_UPDATED: &str = "state.updated";

/// `turnHeader.origin` 的取值（`rows.ts` turnHeaderRowSchema.origin 闭集的本 host 子集）。
/// 普通用户输入轮。
pub const TURN_ORIGIN_USER_INPUT: &str = "userInput";
/// edit/retry 重放轮：TS 用这个 origin 把"重跑"与普通输入轮区分开，UI 不靠文本嗅探。
pub const TURN_ORIGIN_EDIT_RERUN: &str = "editRerun";
/// goal 自动续跑轮（model-only 输入；`turnHeaderRowSchema.origin` 闭集）。
pub const TURN_ORIGIN_GOAL_CONTINUATION: &str = "goalContinuation";
/// 中枢直接启动工作流的 controlOnly 轮（`turnHeaderRowSchema.origin` /
/// `userInputRowSchema.origin` 闭集）。文本仍进 `text`（旧客户端/TUI 的降级
/// 呈现），新客户端用 `workflowLaunch` 元数据画轮尾 run 卡而非气泡。
pub const TURN_ORIGIN_WORKFLOW_LAUNCH: &str = "workflowLaunch";
/// 后台任务完成通知轮：宿主自己发起的 `backgroundResult` 轮，输入是
/// `<task-notification>` XML（模型可见，用户也能看到通知行）。
pub const TURN_ORIGIN_BACKGROUND_RESULT: &str = "backgroundResult";

/// rowBaseFields 的公共部分（`rows.ts` rowBaseFields）。
#[derive(Debug, Clone)]
pub struct RowBase {
    pub row_id: u64,
    pub turn_id: String,
    /// 行的稳定实体身份（`rowBaseFields.entityId`）。命令侧的
    /// `conversationRowTarget.entityId` 就与它比对，所以 append 与终态 upsert
    /// 必须给出同一个值——变了就等于换了实体。
    pub entity_id: String,
    pub created_at: i64,
    pub created_at_seq: u64,
}

pub fn row_base(base: &RowBase) -> Value {
    json!({
        "rowId": base.row_id,
        "turnId": base.turn_id,
        "entityId": base.entity_id,
        "createdAt": base.created_at,
        "createdAtSeq": base.created_at_seq,
    })
}

/// turnHeader 行（`rows.ts` turnHeaderRowSchema 的相关子集）。
/// turnHeader 行的可变字段（8 个位置参数会退化成"调用点靠猜"，所以收成一个结构）。
pub struct TurnHeaderFields<'a> {
    pub origin: &'a str,
    pub state: &'a str,
    pub started_at: i64,
    pub source_command_id: &'a str,
    pub history_round_count: usize,
    /// 本轮的文件变更汇总；运行中的轮传 `None`（还没改完，下发了就是假的）。
    pub file_changes: Option<&'a crate::file_ledger::FileChangeSummary>,
    /// `actions.canRewindFiles`：客户端是否该显示「撤销本轮文件改动」。
    pub can_rewind_files: bool,
    /// `executionKind`：值域 `agent` / `controlOnly`；`None` 不写该键（兼容旧快照）。
    pub execution_kind: Option<&'a str>,
    /// `workflowLaunch`：中枢直接启动轮上在场（活投影来源），与 `originMeta` 并列。
    pub workflow_launch: Option<&'a Value>,
}

pub fn turn_header_row(base: &RowBase, fields: TurnHeaderFields<'_>) -> Value {
    let TurnHeaderFields {
        origin,
        state,
        started_at,
        source_command_id,
        history_round_count,
        file_changes,
        can_rewind_files,
        execution_kind,
        workflow_launch,
    } = fields;
    let mut row = row_base(base);
    if let Some(object) = row.as_object_mut() {
        object.insert("kind".to_string(), json!("turnHeader"));
        object.insert("origin".to_string(), json!(origin));
        object.insert("state".to_string(), json!(state));
        object.insert("startedAt".to_string(), json!(started_at));
        object.insert("sourceCommandId".to_string(), json!(source_command_id));
        // turnHeaderRowSchema.historyRoundCount：该轮之前的产品轮次数。UI 用它标注
        // "第 N 轮"，不必回读 transcript（TS 注释：不得由 UI 重猜）。
        object.insert("historyRoundCount".to_string(), json!(history_round_count));
        // executionKind 由 CLI 投影裁决；UI 不得根据输入文本或 duration 反推。
        if let Some(kind) = execution_kind {
            object.insert("executionKind".to_string(), json!(kind));
        }
        if let Some(launch) = workflow_launch {
            object.insert("workflowLaunch".to_string(), launch.clone());
        }
        // fileChanges 只在轮收口时有意义（本轮的写都记完了才汇总）；运行中的轮不下发。
        if let Some(summary) = file_changes {
            object.insert(
                "fileChanges".to_string(),
                json!({
                    "additions": summary.additions,
                    "deletions": summary.deletions,
                    "files": summary.files,
                    // "active" = 还能回滚；rewind 成功后改写为 "reverted"（TS 同语义）。
                    "state": "active",
                }),
            );
        }
        // rowBaseFields.actions：只下发为 true 的键。
        if can_rewind_files {
            object.insert("actions".to_string(), json!({"canRewindFiles": true}));
        }
    }
    row
}

/// `hookInvocationRowSchema`：一次事件的全部 hook 调用（含被准入跳过的）。
/// `executions` 里 `didExecute=false` 表示准入未通过（没有真的跑过进程）。
pub struct HookInvocationFields<'a> {
    pub hook_invocation_id: &'a str,
    pub event: &'a str,
    pub state: &'a str,
    pub started_at: i64,
    pub ended_at: i64,
    pub lane: &'a str,
    pub anchor_tool_call_id: Option<&'a str>,
    pub executions: Vec<Value>,
}

pub fn hook_invocation_row(base: &RowBase, fields: HookInvocationFields<'_>) -> Value {
    let HookInvocationFields {
        hook_invocation_id,
        event,
        state,
        started_at,
        ended_at,
        lane,
        anchor_tool_call_id,
        executions,
    } = fields;
    let mut row = row_base(base);
    if let Some(object) = row.as_object_mut() {
        object.insert("kind".to_string(), json!("hookInvocation"));
        object.insert("hookInvocationId".to_string(), json!(hook_invocation_id));
        object.insert("hookEventName".to_string(), json!(event));
        object.insert("hookCount".to_string(), json!(executions.len()));
        object.insert("state".to_string(), json!(state));
        object.insert("startedAt".to_string(), json!(started_at));
        object.insert("endedAt".to_string(), json!(ended_at));
        object.insert(
            "durationMs".to_string(),
            json!((ended_at - started_at).max(0)),
        );
        object.insert("lane".to_string(), json!(lane));
        if let Some(call_id) = anchor_tool_call_id {
            object.insert("anchorToolCallId".to_string(), json!(call_id));
        }
        object.insert("executions".to_string(), json!(executions));
    }
    row
}

pub fn user_input_row(
    base: &RowBase,
    text: &str,
    source_command_id: &str,
    client_id: &str,
) -> Value {
    user_input_row_with_origin(
        base,
        text,
        "realUser",
        source_command_id,
        Some(client_id),
        None,
        &[],
    )
}

/// userInput 行的完整构造：`origin` 是闭集枚举，`workflowLaunch` 只在
/// `origin === "workflowLaunch"` 的行上在场（与 turnHeader 上同一份元数据）。
/// `attachments` 已由发送层规范化为 {ref, fileName, mime, bytes, previewRef?}，
/// 非空时写进 row（preview/share 授权的唯一真源）。
pub fn user_input_row_with_origin(
    base: &RowBase,
    text: &str,
    origin: &str,
    source_command_id: &str,
    client_id: Option<&str>,
    workflow_launch: Option<&Value>,
    attachments: &[Value],
) -> Value {
    let mut row = row_base(base);
    if let Some(object) = row.as_object_mut() {
        object.insert("kind".to_string(), json!("userInput"));
        object.insert("text".to_string(), json!(text));
        object.insert("origin".to_string(), json!(origin));
        if let Some(launch) = workflow_launch {
            object.insert("workflowLaunch".to_string(), launch.clone());
        }
        object.insert("sourceCommandId".to_string(), json!(source_command_id));
        if let Some(client_id) = client_id {
            object.insert("clientId".to_string(), json!(client_id));
        }
        if !attachments.is_empty() {
            object.insert("attachments".to_string(), json!(attachments));
        }
    }
    row
}

/// assistantText 行；`text` 始终为当前全量正文（终态 upsert 的权威载体）。
/// `throughput` 是宿主测得的生成吞吐（`specs/model-catalog-and-tps.md` §3）；
/// 只在终态带上，流式中的行不下发假数据。
pub fn assistant_text_row(
    base: &RowBase,
    response_id: &str,
    text: &str,
    state: &str,
    model: Option<&str>,
    throughput: Option<&Value>,
) -> Value {
    let mut row = row_base(base);
    if let Some(object) = row.as_object_mut() {
        object.insert("kind".to_string(), json!("assistantText"));
        object.insert("assistantResponseId".to_string(), json!(response_id));
        object.insert("text".to_string(), json!(text));
        object.insert("state".to_string(), json!(state));
        if let Some(model) = model {
            object.insert("model".to_string(), json!(model));
        }
        if let Some(throughput) = throughput {
            object.insert("throughput".to_string(), throughput.clone());
        }
    }
    row
}

pub fn row_appended(row: Value) -> Value {
    json!({"op": OP_ROW_APPENDED, "row": row})
}

pub fn row_upserted(row: Value) -> Value {
    json!({"op": OP_ROW_UPSERTED, "row": row})
}

/// 分支截断 op：删除 `from_row_id` **及其后所有**行
/// （`delta.ts` row.removed 原文语义，客户端按 rowId >= fromRowId 作用于已加载集合）。
pub fn row_removed(from_row_id: u64) -> Value {
    json!({"op": OP_ROW_REMOVED, "fromRowId": from_row_id})
}

pub fn row_delta(row_id: u64, path: &str, append: &str) -> Value {
    json!({"op": OP_ROW_DELTA, "rowId": row_id, "path": path, "append": append})
}

/// toolCall 行（`rows.ts` toolCallRowSchema 的相关子集）：必填 toolCallId/
/// toolName/status/inputText；`inputText` 是流式路径（continuous 档可增量，
/// 本 increment 整条一次给出，终态 upsert 携带全量）。error 仅 status=error 携带。
/// `startedAt` / `endedAt` / `approvalInteractionId` 是运行态字段：pendingApproval
/// 行必须带着审批外键，终态行清掉它。
#[allow(clippy::too_many_arguments)]
pub fn tool_call_row(
    base: &RowBase,
    tool_call_id: &str,
    tool_name: &str,
    status: &str,
    input_text: &str,
    error: Option<(&str, &str)>,
    output_text: Option<&str>,
    display: Option<Value>,
    work_id: Option<&str>,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    approval_interaction_id: Option<&str>,
) -> Value {
    let mut row = row_base(base);
    if let Some(object) = row.as_object_mut() {
        object.insert("kind".to_string(), json!("toolCall"));
        object.insert("toolCallId".to_string(), json!(tool_call_id));
        object.insert("toolName".to_string(), json!(tool_name));
        object.insert("status".to_string(), json!(status));
        object.insert("inputText".to_string(), json!(input_text));
        if let Some(started_at) = started_at {
            object.insert("startedAt".to_string(), json!(started_at));
        }
        if let Some(ended_at) = ended_at {
            object.insert("endedAt".to_string(), json!(ended_at));
        }
        if let Some(interaction_id) = approval_interaction_id {
            object.insert("approvalInteractionId".to_string(), json!(interaction_id));
        }
        if let Ok(input) = serde_json::from_str::<Value>(input_text) {
            object.insert("input".to_string(), input);
        }
        if let Some((code, message)) = error {
            object.insert(
                "error".to_string(),
                json!({"code": code, "message": message}),
            );
        }
        if let Some(text) = output_text {
            object.insert("output".to_string(), json!({"text": text}));
        }
        // `toolCallRowSchema.display` is a **row-level** field (the client renders
        // the result card from it); it is not part of `output`.
        if let Some(display) = display {
            object.insert("display".to_string(), display);
        }
        if let Some(work_id) = work_id {
            object.insert("backgrounded".to_string(), json!(true));
            object.insert("workId".to_string(), json!(work_id));
        }
    }
    row
}

/// reasoning 行（`rows.ts` reasoningRowSchema）：思考正文，state 三态，
/// durationMs 仅终态带上。
pub fn reasoning_row(
    base: &RowBase,
    response_id: &str,
    text: &str,
    state: &str,
    duration_ms: Option<i64>,
) -> Value {
    let mut row = row_base(base);
    if let Some(object) = row.as_object_mut() {
        object.insert("kind".to_string(), json!("reasoning"));
        object.insert("assistantResponseId".to_string(), json!(response_id));
        object.insert("text".to_string(), json!(text));
        object.insert("state".to_string(), json!(state));
        if let Some(duration_ms) = duration_ms {
            object.insert("durationMs".to_string(), json!(duration_ms));
        }
    }
    row
}

pub fn state_updated(patch: Value) -> Value {
    json!({"op": OP_STATE_UPDATED, "patch": patch})
}

/// 终态一致性断言（黄金不变量的 Rust 侧表现）：过滤+合并后的 delta 序列 apply 到
/// 初始行集合上，必须得到与终态 snapshot 完全一致的行集合。e2e 与单元测试共用。
pub fn apply_deltas(rows: &mut Vec<Value>, deltas: &[Value]) {
    for delta in deltas {
        match delta.get("op").and_then(Value::as_str) {
            Some(OP_ROW_APPENDED) => {
                if let Some(row) = delta.get("row") {
                    rows.push(row.clone());
                }
            }
            Some(OP_ROW_UPSERTED) => {
                if let Some(row) = delta.get("row") {
                    let row_id = row.get("rowId").and_then(Value::as_u64);
                    if let Some(row_id) = row_id {
                        if let Some(slot) = rows.iter_mut().find(|candidate| {
                            candidate.get("rowId").and_then(Value::as_u64) == Some(row_id)
                        }) {
                            *slot = row.clone();
                            continue;
                        }
                    }
                    rows.push(row.clone());
                }
            }
            Some(OP_ROW_DELTA) => {
                let Some(row_id) = delta.get("rowId").and_then(Value::as_u64) else {
                    continue;
                };
                let path = delta.get("path").and_then(Value::as_str).unwrap_or("");
                let append = delta.get("append").and_then(Value::as_str).unwrap_or("");
                if let Some(row) = rows.iter_mut().find(|candidate| {
                    candidate.get("rowId").and_then(Value::as_u64) == Some(row_id)
                }) {
                    let current = row.get(path).and_then(Value::as_str).unwrap_or("");
                    let merged = format!("{current}{append}");
                    if let Some(object) = row.as_object_mut() {
                        object.insert(path.to_string(), json!(merged));
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(row_id: u64) -> RowBase {
        RowBase {
            row_id,
            turn_id: "turn-1".to_string(),
            entity_id: "resp-1".to_string(),
            created_at: 1_700_000_000_000,
            created_at_seq: row_id,
        }
    }

    /// 每一行都必须带稳定实体身份：`conversationRowTarget.entityId` 靠它拒绝
    /// 过期快照（rowId 在截断/分叉后指向别的实体）。
    #[test]
    fn every_row_builder_emits_a_stable_entity_id() {
        let base = base(3);
        let rows = [
            turn_header_row(
                &base,
                TurnHeaderFields {
                    origin: TURN_ORIGIN_USER_INPUT,
                    state: "running",
                    started_at: 1,
                    source_command_id: "cmd-1",
                    history_round_count: 0,
                    file_changes: None,
                    can_rewind_files: false,
                    execution_kind: None,
                    workflow_launch: None,
                },
            ),
            user_input_row(&base, "q", "cmd-1", "client-1"),
            assistant_text_row(&base, "resp-1", "text", "complete", None, None),
            tool_call_row(
                &base, "call-1", "Write", "success", "{}", None, None, None, None, None, None, None,
            ),
            reasoning_row(&base, "resp-1", "thinking", "complete", None),
        ];
        for row in &rows {
            assert_eq!(
                row["entityId"].as_str(),
                Some("resp-1"),
                "行构造器必须把 RowBase 的实体身份原样写进行里: {row}"
            );
        }
    }

    #[test]
    fn delta_then_upsert_round_trip_is_stable() {
        let mut rows = vec![user_input_row(&base(1), "hi", "cmd-1", "client-1")];
        let mut streamed = vec![row_appended(assistant_text_row(
            &base(2),
            "resp-1",
            "",
            "streaming",
            Some("m"),
            None,
        ))];
        streamed.push(row_delta(2, "text", "he"));
        streamed.push(row_delta(2, "text", "llo"));
        apply_deltas(&mut rows, &streamed);
        assert_eq!(rows[1]["text"], json!("hello"));

        // 终态 upsert 携带全量正文；再次 apply 不改变结果（幂等）。
        let final_row =
            assistant_text_row(&base(2), "resp-1", "hello", "complete", Some("m"), None);
        apply_deltas(&mut rows, &[row_upserted(final_row.clone())]);
        assert_eq!(rows[1], final_row);
    }

    #[test]
    fn upsert_replaces_by_row_id_without_duplicating() {
        let mut rows = vec![assistant_text_row(
            &base(1),
            "r",
            "a",
            "streaming",
            None,
            None,
        )];
        apply_deltas(
            &mut rows,
            &[row_upserted(assistant_text_row(
                &base(1),
                "r",
                "ab",
                "complete",
                None,
                None,
            ))],
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["text"], json!("ab"));
        assert_eq!(rows[0]["state"], json!("complete"));
    }
}
