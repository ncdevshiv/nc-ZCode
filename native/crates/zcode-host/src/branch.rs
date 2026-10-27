//! 分支定位：把 `conversationRowTarget` 换成一份**截断计划**，并实现 latest-only 守卫。
//!
//! 纯领域模块：只读行集与「轮起始历史位置」表，不碰磁盘、不发帧、不认 host。
//! 语义对齐 `apps/zcode-cli/.../handlers/fork-edit-retry.ts`：
//!
//! - `editUserQuery` = 换文本的 `retryTurn`：截断该轮 → 以新文本重发一轮；
//! - `retryTurn` = 截断 + 重发**原** user prompt，所以原文必须在截断前解析出来
//!   （截断后行集里已经没有它，任何"事后再读一遍"的实现都会拿到空文本）；
//! - 两者都只允许作用于**最后一轮**：历史轮的重跑会静默回退 active branch，
//!   所以守卫直接拒绝，而不是"看起来成功了"。

use serde_json::Value;

/// 一轮的产品事实（由 turnHeader 行与其同 turnId 的后续行聚合）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnAnchor {
    pub turn_id: String,
    /// 该轮 turnHeader 的 rowId —— 即分支截断的 `fromRowId`（删它及其后所有行）。
    pub header_row_id: u64,
    /// 该轮 userInput 行的文本。`retryTurn` 的原 prompt 只能从这里拿。
    pub user_text: Option<String>,
    /// 该轮起始时的模型历史条数 = 历史保留前缀长度。
    /// `None` = transcript 里查不到该轮的起始位置（早于本机制写入的旧数据）。
    pub history_start: Option<usize>,
    /// 该轮是否已经产出 assistantText 行（retryTurn 只作用于有回复的轮）。
    pub has_assistant: bool,
    /// userInput 行的 `origin == "realUser"`（edit 只允许编辑真实用户输入，
    /// 不能编辑引擎注入的 guide/goal 续跑文本）。
    pub is_real_user_query: bool,
}

/// 分支定位失败：每条都有协议级 reasonCode，客户端据此呈现文案。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchFault {
    /// 目标 rowId 不在任何轮里 / 行类型不匹配 —— TS `V4RowTranslationError`。
    TargetUnresolvable,
    /// retryTurn 目标不是最后一轮的 assistant 回复。
    RetryTargetNotLatest,
    /// editUserQuery 目标不是最后一轮的 real user query。
    EditTargetNotLatest,
    /// 行集定位成功，但历史起点缺失：截断会让模型上下文与行集分叉，宁可拒绝。
    HistoryAnchorMissing,
    /// 正文与附件同时为空（TS 在 rewind **之前**校验，避免半程失败）。
    EmptyInput,
}

impl BranchFault {
    pub fn reason_code(self) -> &'static str {
        match self {
            BranchFault::TargetUnresolvable => "fault.command.executionFailed",
            BranchFault::RetryTargetNotLatest => "guard.latestAssistantRetryOnly",
            BranchFault::EditTargetNotLatest => "guard.latestQueryEditOnly",
            BranchFault::HistoryAnchorMissing => "fault.command.executionFailed",
            BranchFault::EmptyInput => "proto.invalidPayload",
        }
    }
}

fn row_id(row: &Value) -> Option<u64> {
    row.get("rowId").and_then(Value::as_u64)
}

fn row_kind(row: &Value) -> Option<&str> {
    row.get("kind").and_then(Value::as_str)
}

/// 按 rowId 取行（rowId 单调且唯一）。
fn row_by_id(rows: &[Value], target: u64) -> Option<&Value> {
    rows.iter().find(|row| row_id(row) == Some(target))
}

/// 行的实体身份（`conversationRowTarget.entityId` 要比对的那个 id）。
///
/// assistant 行的身份是 `assistantResponseId`，tool 行是 `toolCallId`；turnHeader 与
/// userInput 行没有独立实体 id —— rowId 单调且永不复用已足以定位，所以这类行不参与
/// id 比对（传了也不拒），而不是给它编一个假 id。
pub fn row_entity_id(row: &Value) -> Option<&str> {
    row.get("entityId")
        .or_else(|| row.get("assistantResponseId"))
        .or_else(|| row.get("toolCallId"))
        .and_then(Value::as_str)
}

/// 目标实体是否仍是那一行：`entityId` 对不上说明调用方拿的是过期快照，
/// 必须拒绝（宁可让 UI 重新取行，也不能截错分支）。
fn entity_matches(row: &Value, target_entity_id: Option<&str>) -> bool {
    match (
        row_entity_id(row),
        target_entity_id.filter(|id| !id.is_empty()),
    ) {
        (Some(actual), Some(expected)) => actual == expected,
        _ => true,
    }
}

/// 按行序聚合出轮列表。没有 turnHeader 的前导行（理论上不该出现）不计入任何轮，
/// 因此也不会被误当成某一轮的目标。
pub fn turns(rows: &[Value], history_starts: &[(String, usize)]) -> Vec<TurnAnchor> {
    let mut out: Vec<TurnAnchor> = Vec::new();
    for row in rows {
        let turn_id = row.get("turnId").and_then(Value::as_str).unwrap_or("");
        let header_row_id = row_id(row).unwrap_or(0);
        match row_kind(row) {
            Some("turnHeader") => out.push(TurnAnchor {
                turn_id: turn_id.to_string(),
                header_row_id,
                user_text: None,
                history_start: history_starts
                    .iter()
                    .find(|(id, _)| id == turn_id)
                    .map(|(_, start)| *start),
                has_assistant: false,
                is_real_user_query: false,
            }),
            Some("userInput") => {
                if let Some(turn) = out.iter_mut().rev().find(|turn| turn.turn_id == turn_id) {
                    turn.user_text = row.get("text").and_then(Value::as_str).map(str::to_string);
                    turn.is_real_user_query =
                        row.get("origin").and_then(Value::as_str) == Some("realUser");
                }
            }
            Some("assistantText") => {
                if let Some(turn) = out.iter_mut().rev().find(|turn| turn.turn_id == turn_id) {
                    turn.has_assistant = true;
                }
            }
            // toolCall / reasoning / 其他行不改变轮的产品事实。
            _ => {}
        }
    }
    out
}

/// `forkAssistant` 的失败种类，每条都是协议级 reasonCode。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkFault {
    /// 目标行解析不到实体 / 不是 completed assistant 行 / 所属轮未收口
    /// ——TS `guard.forkTargetNotStable`。
    TargetNotStable,
    /// 目标不是该轮**最后一段** assistant——TS
    /// `V4ForkTargetNotLatestSegmentError`，reasonCode 沿用 executionFailed。
    TargetNotLatestSegment,
    /// 该轮的 transcript 边界不自洽（锚点缺失或越界）——TS `guard.forkTargetAmbiguous`。
    TargetAmbiguous,
}

impl ForkFault {
    pub fn reason_code(self) -> &'static str {
        match self {
            ForkFault::TargetNotStable => "guard.forkTargetNotStable",
            ForkFault::TargetNotLatestSegment => "fault.command.executionFailed",
            ForkFault::TargetAmbiguous => "guard.forkTargetAmbiguous",
        }
    }

    pub fn explain(self) -> &'static str {
        match self {
            ForkFault::TargetNotStable => "目标必须属于一个已收口轮次的 completed assistant 行",
            ForkFault::TargetNotLatestSegment => "目标必须是该轮最后一段 assistant 回复",
            ForkFault::TargetAmbiguous => "该轮的 transcript 边界不自洽，无法固定分叉点",
        }
    }
}

/// 分叉边界：child 会话要复制多少父会话内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkBoundary {
    pub turn_id: String,
    /// 复制父行集的前 `row_count` 行（含目标 assistant 行）。
    pub row_count: usize,
    /// 复制父模型历史的前 `history_end` 条（= 该轮结束处的历史长度）。
    pub history_end: usize,
}

/// `forkAssistant` 的稳定目标解析。
///
/// TS 用 transcript 的 message anchor（productTurnId / orderedMessageIds /
/// boundaryMessageId）固定轮边界；本 host 的 transcript 只有行集 + 每轮的历史锚点，
/// 所以等价保证由这三条承担：目标行可解析、必须是该轮**最后一段已完成的** assistant 行、
/// 且该轮的历史区间自洽（起点 < 终点 ≤ 历史总长）。任一条不成立就拒绝，
/// 不做"猜一个边界"的降级。
pub fn fork_target(
    rows: &[Value],
    history_starts: &[(String, usize)],
    history_len: usize,
    target_row_id: u64,
    target_entity_id: Option<&str>,
) -> Result<ForkBoundary, ForkFault> {
    let Some(row) = row_by_id(rows, target_row_id) else {
        return Err(ForkFault::TargetNotStable);
    };
    if !entity_matches(row, target_entity_id)
        || row_kind(row) != Some("assistantText")
        || row.get("state").and_then(Value::as_str) != Some("complete")
    {
        return Err(ForkFault::TargetNotStable);
    }
    let turn_id = row
        .get("turnId")
        .and_then(Value::as_str)
        .ok_or(ForkFault::TargetNotStable)?
        .to_string();
    let turn_rows: Vec<&Value> = rows
        .iter()
        .filter(|row| row.get("turnId").and_then(Value::as_str) == Some(turn_id.as_str()))
        .collect();
    let header = turn_rows
        .iter()
        .find(|row| row_kind(row) == Some("turnHeader"))
        .ok_or(ForkFault::TargetNotStable)?;
    if !matches!(
        header.get("state").and_then(Value::as_str),
        Some("completedSuccess") | Some("completedInterrupted")
    ) {
        return Err(ForkFault::TargetNotStable);
    }
    // 中间段落 fork 会丢掉父会话已经产出的后续回复，所以只允许轮尾最后一段。
    let last_assistant = turn_rows
        .iter()
        .filter(|row| row_kind(row) == Some("assistantText"))
        .filter_map(|row| row_id(row))
        .next_back();
    if last_assistant != Some(target_row_id) {
        return Err(ForkFault::TargetNotLatestSegment);
    }
    let start = history_starts
        .iter()
        .find(|(id, _)| *id == turn_id)
        .map(|(_, start)| *start)
        .ok_or(ForkFault::TargetAmbiguous)?;
    // 历史终点 = **后一轮**的起点（锚点里比本轮起点大的最小值）；没有后一轮就是整段历史。
    // WHY 不是"任一其它轮"：fork 一个早期轮时，行集里更靠前的轮也算"其它轮"，
    // 拿它的起点当终点会得到 start >= end 的假越界，把好目标误判成 ambiguous。
    let history_end = history_starts
        .iter()
        .map(|(_, start)| *start)
        .filter(|candidate| *candidate > start)
        .min()
        .unwrap_or(history_len);
    let row_count = rows
        .iter()
        .position(|row| row_id(row) == Some(target_row_id))
        .map(|index| index + 1)
        .ok_or(ForkFault::TargetNotStable)?;
    if start >= history_end || history_end > history_len {
        return Err(ForkFault::TargetAmbiguous);
    }
    Ok(ForkBoundary {
        turn_id,
        row_count,
        history_end,
    })
}

/// `retryTurn`：目标必须是**最后一轮**里的一条 assistantText 行。
///
/// 逐条对齐 TS：先按 rowId 解析行（解析不到 = `V4RowTranslationError` →
/// `fault.command.executionFailed`），再判"是不是最后一轮的 assistant 回复"
/// （`guard.latestAssistantRetryOnly`）。历史轮的 retry 会静默回退 active branch，
/// 所以宁可拒绝。
pub fn retry_target(
    rows: &[Value],
    history_starts: &[(String, usize)],
    target_row_id: u64,
    target_entity_id: Option<&str>,
) -> Result<TurnAnchor, BranchFault> {
    let Some(row) = row_by_id(rows, target_row_id) else {
        return Err(BranchFault::TargetUnresolvable);
    };
    if !entity_matches(row, target_entity_id) {
        return Err(BranchFault::TargetUnresolvable);
    }
    let all = turns(rows, history_starts);
    let Some(last) = all.last() else {
        return Err(BranchFault::TargetUnresolvable);
    };
    let is_latest_assistant = row_kind(row) == Some("assistantText")
        && row.get("turnId").and_then(Value::as_str) == Some(last.turn_id.as_str())
        && last.has_assistant;
    if !is_latest_assistant {
        return Err(BranchFault::RetryTargetNotLatest);
    }
    require_history_anchor(last.clone())
}

/// `editUserQuery`：目标必须是**最后一轮**的 realUser userInput 行。
pub fn edit_target(
    rows: &[Value],
    history_starts: &[(String, usize)],
    target_row_id: u64,
    target_entity_id: Option<&str>,
) -> Result<TurnAnchor, BranchFault> {
    let Some(row) = row_by_id(rows, target_row_id) else {
        return Err(BranchFault::TargetUnresolvable);
    };
    if !entity_matches(row, target_entity_id) {
        return Err(BranchFault::TargetUnresolvable);
    }
    let all = turns(rows, history_starts);
    let Some(last) = all.last() else {
        return Err(BranchFault::TargetUnresolvable);
    };
    let is_latest_query = row_kind(row) == Some("userInput")
        && row.get("turnId").and_then(Value::as_str) == Some(last.turn_id.as_str())
        && last.is_real_user_query;
    if !is_latest_query {
        return Err(BranchFault::EditTargetNotLatest);
    }
    require_history_anchor(last.clone())
}

fn require_history_anchor(anchor: TurnAnchor) -> Result<TurnAnchor, BranchFault> {
    if anchor.history_start.is_some() {
        Ok(anchor)
    } else {
        Err(BranchFault::HistoryAnchorMissing)
    }
}

/// TS 的输入校验：正文与附件都为空 → 在截断**之前**拒绝。
pub fn require_prompt_input(text: &str, has_attachments: bool) -> Result<(), BranchFault> {
    if text.trim().is_empty() && !has_attachments {
        Err(BranchFault::EmptyInput)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 一轮三行：turnHeader → userInput → assistantText（rowId 由 row_base 递增）。
    fn turn(
        turn_id: &str,
        header_row_id: u64,
        text: &str,
        origin: &str,
        assistant: bool,
    ) -> Vec<Value> {
        let mut rows = vec![
            json!({"rowId": header_row_id, "turnId": turn_id, "kind": "turnHeader", "state": "completedSuccess"}),
            json!({"rowId": header_row_id + 1, "turnId": turn_id, "kind": "userInput", "text": text, "origin": origin}),
        ];
        if assistant {
            rows.push(json!({"rowId": header_row_id + 2, "turnId": turn_id, "kind": "assistantText", "state": "complete"}));
        }
        rows
    }

    fn two_turns() -> (Vec<Value>, Vec<(String, usize)>) {
        let mut rows = turn("t1", 0, "first question", "realUser", true);
        rows.extend(turn("t2", 3, "second question", "realUser", true));
        let starts = vec![("t1".to_string(), 0usize), ("t2".to_string(), 2usize)];
        (rows, starts)
    }

    #[test]
    fn turns_groups_rows_by_turn_header() {
        let (rows, starts) = two_turns();
        let all = turns(&rows, &starts);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].turn_id, "t1");
        assert_eq!(all[0].history_start, Some(0));
        assert_eq!(all[1].history_start, Some(2));
        assert_eq!(all[1].user_text.as_deref(), Some("second question"));
        assert!(all
            .iter()
            .all(|turn| turn.has_assistant && turn.is_real_user_query));
    }

    #[test]
    fn rows_before_any_header_belong_to_no_turn() {
        let rows = vec![
            json!({"rowId": 0, "turnId": "orphan", "kind": "assistantText"}),
            json!({"rowId": 1, "turnId": "t1", "kind": "turnHeader"}),
        ];
        let all = turns(&rows, &[]);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].header_row_id, 1);
    }

    #[test]
    fn retry_targets_only_the_latest_assistant_turn() {
        let (rows, starts) = two_turns();
        // 第一轮的 assistant 行（rowId 2）→ 守卫拒绝：重跑历史轮会回退 active branch。
        assert_eq!(
            retry_target(&rows, &starts, 2, None),
            Err(BranchFault::RetryTargetNotLatest)
        );
        // 最后一轮的 assistant 行（rowId 5）→ 通过，并带回原 prompt 与历史起点。
        let anchor = retry_target(&rows, &starts, 5, None).expect("latest retry target");
        assert_eq!(anchor.turn_id, "t2");
        assert_eq!(anchor.header_row_id, 3);
        assert_eq!(anchor.user_text.as_deref(), Some("second question"));
        assert_eq!(anchor.history_start, Some(2));
    }

    #[test]
    fn unknown_target_row_is_unresolvable_not_misguarded() {
        let (rows, starts) = two_turns();
        // 迟到/编造的 rowId 不能被当成"最后一轮"：必须报解析失败（TS V4RowTranslationError）。
        assert_eq!(
            retry_target(&rows, &starts, 999, None),
            Err(BranchFault::TargetUnresolvable)
        );
        assert_eq!(
            edit_target(&rows, &starts, 999, None),
            Err(BranchFault::TargetUnresolvable)
        );
        // 存在但类型不对：retry 指到 userInput 行、edit 指到 turnHeader 行，都是守卫拒绝。
        assert_eq!(
            retry_target(&rows, &starts, 4, None),
            Err(BranchFault::RetryTargetNotLatest)
        );
        assert_eq!(
            edit_target(&rows, &starts, 3, None),
            Err(BranchFault::EditTargetNotLatest)
        );
    }

    #[test]
    fn retry_rejects_a_turn_without_an_assistant_reply() {
        let mut rows = turn("t1", 0, "only question", "realUser", true);
        rows.extend(turn("t2", 3, "no reply yet", "realUser", false));
        let starts = vec![("t1".to_string(), 0usize), ("t2".to_string(), 2usize)];
        assert_eq!(
            retry_target(&rows, &starts, 3, None),
            Err(BranchFault::RetryTargetNotLatest)
        );
    }

    #[test]
    fn edit_targets_only_the_latest_real_user_query() {
        let (rows, starts) = two_turns();
        assert_eq!(
            edit_target(&rows, &starts, 1, None),
            Err(BranchFault::EditTargetNotLatest)
        );
        let anchor = edit_target(&rows, &starts, 4, None).expect("latest edit target");
        assert_eq!(anchor.user_text.as_deref(), Some("second question"));

        // 引擎注入的续跑文本（origin != realUser）不可编辑。
        let mut rows = turn("t1", 0, "user", "realUser", true);
        rows.extend(turn("t2", 3, "goal continuation", "goalContinuation", true));
        let starts = vec![("t1".to_string(), 0usize), ("t2".to_string(), 2usize)];
        assert_eq!(
            edit_target(&rows, &starts, 4, None),
            Err(BranchFault::EditTargetNotLatest)
        );
    }

    #[test]
    fn stale_entity_id_is_rejected() {
        let (mut rows, starts) = two_turns();
        // 给最后一轮的 assistant 行补上实体 id（真实投影里由 assistantResponseId 提供）。
        rows.last_mut().expect("row")["assistantResponseId"] = json!("resp-real");
        assert_eq!(
            retry_target(&rows, &starts, 5, Some("resp-other")),
            Err(BranchFault::TargetUnresolvable),
            "调用方拿着过期实体时必须拒绝，不能截错分支"
        );
        assert!(retry_target(&rows, &starts, 5, Some("resp-real")).is_ok());
        // 没有实体 id 的行不参与比对（rowId 单调已足够定位）。
        assert!(edit_target(&rows, &starts, 4, Some("anything")).is_ok());
    }

    #[test]
    fn missing_history_anchor_fails_closed() {
        let (rows, _) = two_turns();
        // 旧 transcript（本机制之前写入）没有历史起点：宁可拒绝截断。
        assert_eq!(
            retry_target(&rows, &[], 5, None),
            Err(BranchFault::HistoryAnchorMissing)
        );
    }

    #[test]
    fn empty_prompt_input_is_rejected_before_any_truncation() {
        assert_eq!(
            require_prompt_input("  ", false),
            Err(BranchFault::EmptyInput)
        );
        assert_eq!(require_prompt_input("", true), Ok(()));
        assert_eq!(require_prompt_input("hello", false), Ok(()));
    }

    fn forkable_rows() -> (Vec<Value>, Vec<(String, usize)>) {
        // 两轮，每轮 header/user/assistant 三段，历史各 2 条。
        let mut rows = turn("t1", 0, "first", "realUser", true);
        rows.extend(turn("t2", 3, "second", "realUser", true));
        for row in &mut rows {
            if row["kind"] == json!("turnHeader") {
                let state = "completedSuccess";
                row["state"] = json!(state);
            }
            if row["kind"] == json!("assistantText") {
                row["state"] = json!("complete");
            }
        }
        let starts = vec![("t1".to_string(), 0usize), ("t2".to_string(), 2usize)];
        (rows, starts)
    }

    #[test]
    fn fork_targets_the_turn_tail_and_cuts_history_at_the_next_turn() {
        let (rows, starts) = forkable_rows();
        // t1 的 assistant 行（rowId 2）：下一轮从历史第 2 条起，所以父历史只取 2 条。
        let boundary = fork_target(&rows, &starts, 4, 2, None).expect("stable fork target");
        assert_eq!(boundary.turn_id, "t1");
        assert_eq!(boundary.row_count, 3);
        assert_eq!(boundary.history_end, 2);
        // 最后一轮（rowId 5）：历史取满。
        let last = fork_target(&rows, &starts, 4, 5, None).expect("last turn forks too");
        assert_eq!((last.row_count, last.history_end), (6, 4));
    }

    #[test]
    fn fork_rejects_unstable_targets() {
        let (rows, starts) = forkable_rows();
        // 不存在的行。
        assert_eq!(
            fork_target(&rows, &starts, 4, 42, None),
            Err(ForkFault::TargetNotStable)
        );
        // 指到 userInput 行（不是 assistant 段）。
        assert_eq!(
            fork_target(&rows, &starts, 4, 1, None),
            Err(ForkFault::TargetNotStable)
        );
        // 实体对不上：调用方拿的是过期快照。
        let mut with_entity = rows.clone();
        with_entity[2]["entityId"] = json!("resp-cmd-t1");
        assert_eq!(
            fork_target(&with_entity, &starts, 4, 2, Some("resp-other")),
            Err(ForkFault::TargetNotStable)
        );
    }

    #[test]
    fn fork_rejects_a_mid_turn_segment() {
        let (mut rows, starts) = forkable_rows();
        // 同轮多段回复：只有最后一段可 fork。把新段插在 t2 assistant 之后。
        rows.insert(
            6,
            json!({"rowId": 6, "turnId": "t2", "kind": "assistantText", "state": "complete"}),
        );
        // rowId 5 现在不是最后一段（6 才是）。
        assert_eq!(
            fork_target(&rows, &starts, 4, 5, None),
            Err(ForkFault::TargetNotLatestSegment)
        );
        assert!(fork_target(&rows, &starts, 4, 6, None).is_ok());
    }

    #[test]
    fn fork_rejects_a_running_turn_and_a_missing_history_anchor() {
        let (mut rows, starts) = forkable_rows();
        rows.iter_mut()
            .find(|row| row["kind"] == json!("turnHeader") && row["turnId"] == json!("t2"))
            .expect("t2 header")["state"] = json!("running");
        assert_eq!(
            fork_target(&rows, &starts, 4, 5, None),
            Err(ForkFault::TargetNotStable)
        );
        let (rows, _) = forkable_rows();
        assert_eq!(
            fork_target(&rows, &[], 4, 5, None),
            Err(ForkFault::TargetAmbiguous),
            "没有历史锚点就固定不了分叉点，宁可拒绝"
        );
        assert_eq!(
            fork_target(&rows, &[("t2".to_string(), 9)], 4, 5, None),
            Err(ForkFault::TargetAmbiguous),
            "锚点越界同样是不自洽"
        );
    }

    #[test]
    fn fork_reason_codes_match_the_ts_vocabulary() {
        assert_eq!(
            ForkFault::TargetNotStable.reason_code(),
            "guard.forkTargetNotStable"
        );
        assert_eq!(
            ForkFault::TargetAmbiguous.reason_code(),
            "guard.forkTargetAmbiguous"
        );
        assert_eq!(
            ForkFault::TargetNotLatestSegment.reason_code(),
            "fault.command.executionFailed"
        );
    }

    #[test]
    fn fault_reason_codes_stay_inside_the_protocol_vocabulary() {
        assert_eq!(
            BranchFault::RetryTargetNotLatest.reason_code(),
            "guard.latestAssistantRetryOnly"
        );
        assert_eq!(
            BranchFault::EditTargetNotLatest.reason_code(),
            "guard.latestQueryEditOnly"
        );
        assert_eq!(
            BranchFault::EmptyInput.reason_code(),
            "proto.invalidPayload"
        );
        assert_eq!(
            BranchFault::TargetUnresolvable.reason_code(),
            "fault.command.executionFailed"
        );
        assert_eq!(
            BranchFault::HistoryAnchorMissing.reason_code(),
            "fault.command.executionFailed"
        );
    }
}
