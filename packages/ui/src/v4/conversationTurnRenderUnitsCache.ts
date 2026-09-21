/**
 * render unit 记忆化缓存（spec: specs/large-session-scaling.md）。
 *
 * Bug 根因（2026-09-21 大会话卡顿追踪）：buildConversationTurnRenderUnits 过去每个 30ms
 * 流式帧都对整窗所有 turn 全量物化（过滤、分段、workSegments 重建）。帧帧新建
 * rows.window 使全部 unit 换标识，memo(ConversationTurnGroup) 永远命中不了——即使该
 * turn 的内容逐字节未变，与会话长度成正比的工作量也每帧重付一次。
 *
 * 本缓存是同一纯函数的记忆化，不引入第二份事实：物化结果只依赖
 * （该 turn 的行对象标识序列、isLastTurn、isRunning、sessionPhase，以及仅对运行中轮
 * 生效的 nowMs——resolveConversationTurnWorkDurationMs 与 segment duration 都以
 * isRunning 为前提）。输入不变即复用上一次的 unit 对象，未变化的 turn 因此保持标识
 * 稳定，memo 边界可以命中；整窗结果逐元素未变时连数组标识一起保持，下游 useMemo
 * （live tail 拆分、目录项、查询行集合）也随之全部命中。
 *
 * 缓存按订阅方（timeline / share timeline）各持一份，随 sessionKey 重建，不跨会话共享。
 */
import type { ConversationRow, SessionPhase } from "@zcode/shared/zcode-protocol-v4";
// 仅类型依赖：ConversationTurnRenderUnit 由 builder 模块导出，运行期无环（import type 擦除）。
import type { ConversationTurnRenderUnit } from "@/v4/conversationTurnRenderUnits.js";

export interface ConversationTurnRenderUnitsCacheKey {
  /** 该 turn 在窗口中的全部行（含 header），按窗口序；指纹只信任行对象标识。 */
  rows: readonly ConversationRow[];
  isLastTurn: boolean;
  isRunning: boolean;
  /** 仅运行中轮消费 nowMs（见文件头）；已完成轮固定为 undefined，每秒 tick 不波及它们。 */
  nowMs: number | undefined;
  /** 只可能被缺 header 的轮次读取（冷尾窗回退）；有 header 时为 undefined。 */
  sessionPhase: SessionPhase | undefined;
}

interface ConversationTurnRenderUnitCacheEntry extends ConversationTurnRenderUnitsCacheKey {
  unit: ConversationTurnRenderUnit;
}

export interface ConversationTurnRenderUnitsCache {
  byTurn: Map<string, ConversationTurnRenderUnitCacheEntry>;
  /** 上一次整窗结果；逐元素未变时原样返回，保持数组标识稳定。 */
  units: ConversationTurnRenderUnit[] | null;
}

export function createConversationTurnRenderUnitsCache(): ConversationTurnRenderUnitsCache {
  return { byTurn: new Map(), units: null };
}

/** 超长会话后淘汰最旧条目（Map 保持插入序 ≈ 最近物化序）；命中失败只会退化为重建。 */
const RENDER_UNIT_CACHE_MAX_TURNS = 2048;

function sameRowSequence(
  left: readonly ConversationRow[],
  right: readonly ConversationRow[],
): boolean {
  if (left.length !== right.length) return false;
  for (let index = 0; index < left.length; index += 1) {
    if (left[index] !== right[index]) return false;
  }
  return true;
}

export function readCachedTurnUnit(
  cache: ConversationTurnRenderUnitsCache,
  turnId: string,
  key: ConversationTurnRenderUnitsCacheKey,
): ConversationTurnRenderUnit | undefined {
  const entry = cache.byTurn.get(turnId);
  if (entry === undefined) return undefined;
  if (
    entry.isLastTurn !== key.isLastTurn ||
    entry.isRunning !== key.isRunning ||
    entry.sessionPhase !== key.sessionPhase ||
    entry.nowMs !== key.nowMs
  ) {
    return undefined;
  }
  if (!sameRowSequence(entry.rows, key.rows)) return undefined;
  // LRU：命中即刷新插入序，避免“热的旧轮次被淘汰、冷的新轮次留下”。
  // 只改 Map 内部顺序，entry 与 unit 对象标识不变，不影响任何调用方的相等性判断。
  cache.byTurn.delete(turnId);
  cache.byTurn.set(turnId, entry);
  return entry.unit;
}

export function writeCachedTurnUnit(
  cache: ConversationTurnRenderUnitsCache,
  turnId: string,
  key: ConversationTurnRenderUnitsCacheKey,
  unit: ConversationTurnRenderUnit,
): void {
  cache.byTurn.set(turnId, { ...key, unit });
  while (cache.byTurn.size > RENDER_UNIT_CACHE_MAX_TURNS) {
    const oldest = cache.byTurn.keys().next();
    if (oldest.done) break;
    cache.byTurn.delete(oldest.value);
  }
}

/** 逐元素未变时返回上一次的数组（标识稳定）；否则由调用方写入新结果。 */
export function readCachedUnitsArray(
  cache: ConversationTurnRenderUnitsCache,
  units: readonly ConversationTurnRenderUnit[],
): ConversationTurnRenderUnit[] | undefined {
  const cached = cache.units;
  if (cached === null || cached.length !== units.length) return undefined;
  return cached.every((unit, index) => unit === units[index]) ? cached : undefined;
}

export function writeCachedUnitsArray(
  cache: ConversationTurnRenderUnitsCache,
  units: ConversationTurnRenderUnit[],
): void {
  cache.units = units;
}
