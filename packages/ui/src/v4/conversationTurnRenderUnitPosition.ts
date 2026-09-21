/**
 * render unit 的位置归一（从 conversationTurnRenderUnits.ts 拆出，该文件受 max-lines 约束）。
 *
 * 物化结果按 turn 局部事实生成；isLastTurn / assistantHistoryDefaultOpen 依赖单元在
 * **保留后序列**中的位置与 sessionPhase，必须在按窗口全量物化之后单独归一。归一是幂等
 * 的：位置相关字段未变时返回同一对象——render unit 缓存正是靠这一点让未变化的轮次保持
 * 标识稳定（见 conversationTurnRenderUnitsCache.ts）。
 */
import type { SessionPhase, TurnHeaderRow } from "@zcode/shared/zcode-protocol-v4";
// 仅类型依赖：ConversationTurnRenderUnit 由 builder 模块导出，运行期无环（import type 擦除）。
import type { ConversationTurnRenderUnit } from "@/v4/conversationTurnRenderUnits.js";

/** 异常终态历史默认展开：header 为权威，冷尾窗缺 header 才回退 sessionPhase。 */
export function shouldForceOpenAbnormalHistory(
  header: TurnHeaderRow | undefined,
  sessionPhase: SessionPhase | undefined,
): boolean {
  if (header) {
    return header.state === "completedInterrupted" || header.state === "failed";
  }
  return sessionPhase === "completedInterrupted" || sessionPhase === "error";
}

export function normalizeRenderUnitPosition(
  unit: ConversationTurnRenderUnit,
  index: number,
  total: number,
  options: { sessionPhase?: SessionPhase },
): ConversationTurnRenderUnit {
  const isLastTurn = index === total - 1;
  const forceOpenHistory = shouldForceOpenAbnormalHistory(unit.header, options.sessionPhase);
  const assistantHistoryDefaultOpen =
    unit.workSegments && unit.workSegments.length > 0
      ? !unit.timelineOnly &&
        (forceOpenHistory ||
          (isLastTurn && unit.workSegments.at(-1)?.workStatus?.state === "running") ||
          (unit.workSegments.length === 1 &&
            unit.latestAssistantTextRow === undefined &&
            unit.assistantWorkRows.length > 0))
      : !unit.timelineOnly &&
        (forceOpenHistory ||
          (isLastTurn && unit.workStatus?.state === "running") ||
          (unit.latestAssistantTextRow === undefined && unit.assistantWorkRows.length > 0));
  const workSegments = unit.workSegments?.map((segment, segmentIndex, segments) =>
    segmentIndex === segments.length - 1
      ? {
          ...segment,
          assistantHistoryDefaultOpen:
            !unit.timelineOnly &&
            (forceOpenHistory ||
              (isLastTurn && segment.workStatus?.state === "running") ||
              (segments.length === 1 &&
                unit.latestAssistantTextRow === undefined &&
                segment.assistantWorkRows.length > 0)),
        }
      : segment,
  );
  if (
    unit.isLastTurn === isLastTurn &&
    unit.assistantHistoryDefaultOpen === assistantHistoryDefaultOpen &&
    workSegments?.at(-1)?.assistantHistoryDefaultOpen ===
      unit.workSegments?.at(-1)?.assistantHistoryDefaultOpen
  ) {
    return unit;
  }
  return {
    ...unit,
    isLastTurn,
    assistantHistoryDefaultOpen,
    ...(workSegments ? { workSegments } : {}),
  };
}
