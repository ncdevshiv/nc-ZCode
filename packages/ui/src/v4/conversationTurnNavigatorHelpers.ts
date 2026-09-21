import type { ConversationTurnRenderUnit } from "@/v4/conversationTurnRenderUnits.js";

export type ConversationTurnNavigatorAssistantPreviewKind = "empty" | "running" | "text";

export interface ConversationTurnNavigatorItem {
  key: string;
  turnId: string;
  unitIndex: number;
  rowId: number;
  userPreview: string;
  assistantPreview: string;
  assistantPreviewKind: ConversationTurnNavigatorAssistantPreviewKind;
  isRunning: boolean;
}

interface BuildConversationTurnNavigatorItemsOptions {
  assistantEmptyPreview: string;
  assistantRunningPreview: string;
  userFallbackPreview: string;
  maxPreviewChars?: number;
  maxPreviewParagraphs?: number;
}

export interface ConversationTurnNavigatorVirtualItem {
  index: number;
  start: number;
  size: number;
}

interface ResolveConversationTurnNavigatorActiveUnitIndexOptions {
  items: readonly ConversationTurnNavigatorItem[];
  virtualItems: readonly ConversationTurnNavigatorVirtualItem[];
  scrollOffsetPx: number;
  viewportHeightPx: number;
}

export interface ConversationTurnNavigatorQueryPosition {
  rowId: number;
  start: number;
  end: number;
}

interface ResolveConversationTurnNavigatorActiveQueryRowIdOptions {
  positions: readonly ConversationTurnNavigatorQueryPosition[];
  scrollOffsetPx: number;
  viewportHeightPx: number;
}

type ConversationTurnNavigatorBarTone = "idle" | "mid" | "near" | "peak";
type ConversationTurnNavigatorBarColorTone = "focus" | "muted";

interface ConversationTurnNavigatorBarVisualState {
  colorTone: ConversationTurnNavigatorBarColorTone;
  opacity: number;
  scaleX: number;
  tone: ConversationTurnNavigatorBarTone;
}

interface ResolveConversationTurnNavigatorBarVisualStateOptions {
  itemIndex: number;
  visualFocusItemIndex: number | undefined;
}

interface ResolveConversationTurnNavigatorVisualFocusItemIndexOptions {
  activeItemIndex: number;
  interactionItemIndex: number | undefined;
}

const CONVERSATION_TURN_NAVIGATOR_MIN_WIDTH_PX = 864;

export type ConversationTurnNavigatorHydrationResult =
  | { status: "hydrated"; logEpoch: string }
  | { status: "not-enough-queries"; logEpoch: string }
  | { status: "retryable-failure"; logEpoch: string }
  | { status: "stale"; logEpoch: string };

export function shouldHydrateConversationTurnNavigatorDirectory(params: {
  canLoadOlder: boolean;
  containerWidthPx: number;
  hasLoadHandler: boolean;
  loadingOlder: boolean;
  /**
   * 用户是否已对目录表达意图（hover / focus rail）。
   *
   * Bug 根因（2026-09-21 大会话卡顿追踪）：过去只要容器 ≥864px 就无条件补拉全量历史
   * 进 rows.window，等于所有桌面用户一进会话就付 O(会话) 的窗口，此后每个流式帧的
   * 全部推导都按整会话长度计费。宽度只是"rail 可能可见"的必要条件，不是"要读全部
   * 历史"的充分条件：目录在用户真正触碰 rail 前只需覆盖已加载的轮次。
   */
  directoryRequested: boolean;
}): boolean {
  return (
    params.canLoadOlder &&
    !params.loadingOlder &&
    params.hasLoadHandler &&
    params.directoryRequested &&
    params.containerWidthPx >= CONVERSATION_TURN_NAVIGATOR_MIN_WIDTH_PX
  );
}

export function resolveConversationTurnNavigatorHydrationRetryDelayMs(
  failedAttemptCount: number,
): number | null {
  if (failedAttemptCount === 1) return 250;
  if (failedAttemptCount === 2) return 1_000;
  return null;
}

const DEFAULT_MAX_PREVIEW_CHARS = 220;
const DEFAULT_MAX_PREVIEW_PARAGRAPHS = 2;

function normalizePreviewParagraphs(text: string, maxParagraphs: number): string[] {
  return text
    .trim()
    .split(/\n\s*\n/u)
    .map((paragraph) => paragraph.replace(/\s+/gu, " ").trim())
    .filter(Boolean)
    .slice(0, Math.max(1, maxParagraphs));
}

function truncatePreview(text: string, maxChars: number): string {
  const normalizedMaxChars = Math.max(8, maxChars);
  if (text.length <= normalizedMaxChars) {
    return text;
  }
  return `${text.slice(0, normalizedMaxChars - 3).trimEnd()}...`;
}

function buildPreviewText({
  texts,
  fallback,
  maxPreviewChars,
  maxPreviewParagraphs,
}: {
  texts: readonly string[];
  fallback: string;
  maxPreviewChars: number;
  maxPreviewParagraphs: number;
}): string {
  const paragraphs = normalizePreviewParagraphs(texts.join("\n\n"), maxPreviewParagraphs);
  if (paragraphs.length === 0) {
    return fallback;
  }
  return truncatePreview(paragraphs.join("\n"), maxPreviewChars);
}

function buildAssistantPreview(
  unit: ConversationTurnRenderUnit,
  options: Required<BuildConversationTurnNavigatorItemsOptions>,
): {
  assistantPreview: string;
  assistantPreviewKind: ConversationTurnNavigatorAssistantPreviewKind;
} {
  if (unit.assistantTextRows.length > 0) {
    return {
      assistantPreview: buildPreviewText({
        texts: unit.assistantTextRows.map((row) => row.text),
        fallback: options.assistantEmptyPreview,
        maxPreviewChars: options.maxPreviewChars,
        maxPreviewParagraphs: options.maxPreviewParagraphs,
      }),
      assistantPreviewKind: "text",
    };
  }

  if (unit.isRunning) {
    return {
      assistantPreview: options.assistantRunningPreview,
      assistantPreviewKind: "running",
    };
  }

  return {
    assistantPreview: options.assistantEmptyPreview,
    assistantPreviewKind: "empty",
  };
}

/**
 * Bug 根因（2026-09-21 大会话卡顿追踪）：目录项过去每次都对全部 turn 重算 preview——
 * buildAssistantPreview 拼接整轮 assistant 全文后跑空白归一正则，userPreview 同样逐 query
 * 正则化。流式每帧 renderUnits 换标识，整份目录（含全部历史文本）随之重算，成本随会话
 * 总字符数增长，是渲染侧单帧最大的常数项。
 *
 * 按 unit 对象标识缓存每轮的 item 草稿（不含 unitIndex——位置逐帧可变，草稿不依赖它）。
 * unit 标识不变即该轮内容未变（render unit 缓存保证），草稿可原样复用；运行中的轮每帧
 * 换标识，自然重算。WeakMap 随 unit 回收，不跨会话累积。
 */
type ConversationTurnNavigatorItemDraft = Omit<ConversationTurnNavigatorItem, "unitIndex">;

interface ConversationTurnNavigatorUnitCacheEntry {
  optionsKey: string;
  drafts: ConversationTurnNavigatorItemDraft[];
}

const navigatorItemDraftsByUnit = new WeakMap<
  ConversationTurnRenderUnit,
  ConversationTurnNavigatorUnitCacheEntry
>();

function buildConversationTurnNavigatorItemDrafts(
  unit: ConversationTurnRenderUnit,
  options: Required<BuildConversationTurnNavigatorItemsOptions>,
): ConversationTurnNavigatorItemDraft[] {
  // provider/store 的物理 role=user 还包含 background/goal/mailbox
  // 等系统上下文；目录代表用户主动 query，只能使用投影明确裁决的 realUser。
  const realUserInputs = unit.visibleUserInputs.filter((row) => row.origin === "realUser");
  if (unit.timelineOnly || realUserInputs.length === 0) {
    return [];
  }

  // 导航项按 query 拆分，但 hover 的 assistant 摘要保持旧产品语义：
  // 取所属 product turn 的文本结果，不在 renderer 猜测 guide 回复分段。
  const { assistantPreview, assistantPreviewKind } = buildAssistantPreview(unit, options);
  return realUserInputs.map((row, queryIndex) => ({
    // 不能以 product turn 为目录粒度，并把同一 turn 的 steer query
    // 全部拼进一个 preview。目录真正导航的是用户可见 query，必须用稳定 row
    // 身份逐条建项，turnId 只负责把虚拟列表先定位到所属容器。
    key: `${unit.key}:query:${row.entityId ?? row.rowId}`,
    turnId: unit.turnId,
    rowId: row.rowId,
    userPreview: buildPreviewText({
      texts: [row.text],
      fallback: options.userFallbackPreview,
      maxPreviewChars: options.maxPreviewChars,
      maxPreviewParagraphs: options.maxPreviewParagraphs,
    }),
    assistantPreview,
    assistantPreviewKind,
    // 同一 running product turn 可能已有多个已结束 guide segment；只有最后一条
    // query 仍代表当前工作，避免所有旧 query 一起呈现 running 强调。
    isRunning: unit.isRunning && queryIndex === realUserInputs.length - 1,
  }));
}

export function buildConversationTurnNavigatorItems(
  units: readonly ConversationTurnRenderUnit[],
  options: BuildConversationTurnNavigatorItemsOptions,
): ConversationTurnNavigatorItem[] {
  const resolvedOptions: Required<BuildConversationTurnNavigatorItemsOptions> = {
    ...options,
    maxPreviewChars: options.maxPreviewChars ?? DEFAULT_MAX_PREVIEW_CHARS,
    maxPreviewParagraphs: options.maxPreviewParagraphs ?? DEFAULT_MAX_PREVIEW_PARAGRAPHS,
  };
  // 语言/上限变化必须作废草稿缓存：intl 文案是草稿的一部分。
  // 从 options 自身派生键而不是手数列名：BuildConversationTurnNavigatorItemsOptions 将来
  // 新增选项时无需回来同步，漏同步会让旧草稿带着过期文案继续 served。
  // 该类型所有取值都是原始值，Object.entries 序列化稳定；sort 让键序不参与比较，
  // 调用方以不同字面量顺序传同值 options 时不会无故击穿缓存。
  const optionsKey = JSON.stringify(
    Object.entries(resolvedOptions).sort(([left], [right]) => left.localeCompare(right)),
  );

  const items: ConversationTurnNavigatorItem[] = [];
  units.forEach((unit, unitIndex) => {
    const cached = navigatorItemDraftsByUnit.get(unit);
    let drafts: ConversationTurnNavigatorItemDraft[];
    if (cached !== undefined && cached.optionsKey === optionsKey) {
      drafts = cached.drafts;
    } else {
      drafts = buildConversationTurnNavigatorItemDrafts(unit, resolvedOptions);
      navigatorItemDraftsByUnit.set(unit, { optionsKey, drafts });
    }
    for (const draft of drafts) {
      items.push({ ...draft, unitIndex });
    }
  });
  return items;
}

function resolveFiniteNonNegative(value: number): number {
  return Number.isFinite(value) ? Math.max(0, value) : 0;
}

export function resolveConversationTurnNavigatorActiveUnitIndex({
  items,
  virtualItems,
  scrollOffsetPx,
  viewportHeightPx,
}: ResolveConversationTurnNavigatorActiveUnitIndexOptions): number | undefined {
  if (items.length === 0) {
    return undefined;
  }

  const itemByUnitIndex = new Map(items.map((item) => [item.unitIndex, item]));
  const viewportStart = resolveFiniteNonNegative(scrollOffsetPx);
  const viewportEnd = viewportStart + Math.max(1, resolveFiniteNonNegative(viewportHeightPx));

  let activeUnitIndex: number | undefined;
  let activeDistance = Number.POSITIVE_INFINITY;
  for (const virtualItem of virtualItems) {
    const item = itemByUnitIndex.get(virtualItem.index);
    if (!item) {
      continue;
    }
    const rowStart = resolveFiniteNonNegative(virtualItem.start);
    const rowEnd = rowStart + Math.max(1, resolveFiniteNonNegative(virtualItem.size));
    if (rowEnd < viewportStart || rowStart > viewportEnd) {
      continue;
    }
    const distanceToViewportStart = rowStart <= viewportStart ? 0 : rowStart - viewportStart;
    if (distanceToViewportStart < activeDistance) {
      activeUnitIndex = item.unitIndex;
      activeDistance = distanceToViewportStart;
    }
  }

  if (activeUnitIndex !== undefined) {
    return activeUnitIndex;
  }

  const topVirtualIndex = virtualItems.find((item) => {
    const rowStart = resolveFiniteNonNegative(item.start);
    const rowEnd = rowStart + Math.max(1, resolveFiniteNonNegative(item.size));
    return rowEnd >= viewportStart && rowStart <= viewportEnd;
  })?.index;
  if (topVirtualIndex === undefined) {
    return items[0]?.unitIndex;
  }

  return (
    items.find((item) => item.unitIndex >= topVirtualIndex)?.unitIndex ??
    items.findLast((item) => item.unitIndex <= topVirtualIndex)?.unitIndex ??
    items[0]?.unitIndex
  );
}

export function resolveConversationTurnNavigatorActiveQueryRowId({
  positions,
  scrollOffsetPx,
  viewportHeightPx,
}: ResolveConversationTurnNavigatorActiveQueryRowIdOptions): number | undefined {
  if (positions.length === 0) return undefined;

  const viewportStart = resolveFiniteNonNegative(scrollOffsetPx);
  const viewportEnd = viewportStart + Math.max(1, resolveFiniteNonNegative(viewportHeightPx));
  const normalized = positions
    .map((position) => {
      const start = resolveFiniteNonNegative(position.start);
      return {
        rowId: position.rowId,
        start,
        end: Math.max(start, resolveFiniteNonNegative(position.end)),
      };
    })
    .sort((left, right) => left.start - right.start || left.rowId - right.rowId);

  const visible = normalized.filter(
    (position) => position.end >= viewportStart && position.start <= viewportEnd,
  );
  if (visible.length > 0) {
    return visible.reduce((nearest, candidate) =>
      Math.abs(candidate.start - viewportStart) < Math.abs(nearest.start - viewportStart)
        ? candidate
        : nearest,
    ).rowId;
  }

  return (
    normalized.findLast((position) => position.start <= viewportStart)?.rowId ??
    normalized.find((position) => position.start > viewportStart)?.rowId
  );
}

export function resolveConversationTurnNavigatorBarVisualState({
  itemIndex,
  visualFocusItemIndex,
}: ResolveConversationTurnNavigatorBarVisualStateOptions): ConversationTurnNavigatorBarVisualState {
  if (visualFocusItemIndex === undefined) {
    return { colorTone: "muted", opacity: 0.58, scaleX: 1, tone: "idle" };
  }

  const distance = Math.abs(itemIndex - visualFocusItemIndex);
  if (distance === 0) {
    return { colorTone: "focus", opacity: 1, scaleX: 2.6, tone: "peak" };
  }
  if (distance === 1) {
    return { colorTone: "muted", opacity: 0.86, scaleX: 1.7, tone: "near" };
  }
  if (distance === 2) {
    return { colorTone: "muted", opacity: 0.72, scaleX: 1.25, tone: "mid" };
  }
  return { colorTone: "muted", opacity: 0.58, scaleX: 1, tone: "idle" };
}

export function resolveConversationTurnNavigatorVisualFocusItemIndex({
  interactionItemIndex,
}: ResolveConversationTurnNavigatorVisualFocusItemIndexOptions): number | undefined {
  return interactionItemIndex;
}
