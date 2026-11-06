import type { AppUsageQueryResult } from "@zcode/contracts";
import type {
  AppUsageHeatmap,
  AppUsageHeatmapCell,
  AppUsageHeatmapWeek,
  AppUsageRange,
  AppUsageSnapshot,
} from "@zcode/shared";

const DAY_MS = 86_400_000;

interface BuildAppUsageOptions {
  range: AppUsageRange;
  timeZone: string;
  tzOffsetMs: number;
  generatedAt: number;
  since: number;
  until: number;
}

/** Use Intl to calculate the offset (ms) of timeZone relative to UTC at atMs. Fallback to 0 if it cannot be parsed. */
export function resolveTzOffsetMs(timeZone: string, atMs: number): number {
  try {
    const dtf = new Intl.DateTimeFormat("en-US", {
      timeZone,
      hourCycle: "h23",
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
    });
    const parts = dtf.formatToParts(new Date(atMs));
    const lookup = (type: string) => Number(parts.find((p) => p.type === type)?.value);
    const asUtc = Date.UTC(
      lookup("year"),
      lookup("month") - 1,
      lookup("day"),
      lookup("hour"),
      lookup("minute"),
      lookup("second"),
    );
    return asUtc - Math.trunc(atMs / 1000) * 1000;
  } catch {
    return 0;
  }
}

function dayIndexToDate(dayIndex: number): string {
// dayIndex*DAY is the time when "local midnight is considered UTC", and its UTC calendar component is the local date.
  return new Date(dayIndex * DAY_MS).toISOString().slice(0, 10);
}

function levelFor(tokens: number, max: number): AppUsageHeatmapCell["level"] {
  if (tokens <= 0 || max <= 0) return 0;
  const ratio = tokens / max;
  if (ratio > 0.75) return 4;
  if (ratio > 0.5) return 3;
  if (ratio > 0.25) return 2;
  return 1;
}

function resolveUsageStartDayIndex(
  result: AppUsageQueryResult,
  opts: BuildAppUsageOptions,
  endDayIndex: number,
): number {
  if (opts.range !== "all") {
    return Math.floor((opts.since + opts.tzOffsetMs) / DAY_MS);
  }

  const dayIndexes = [
    ...result.days.map((day) => day.dayIndex),
    ...result.dayModels.map((dayModel) => dayModel.dayIndex),
  ];
  if (dayIndexes.length === 0) {
    return endDayIndex;
  }
  return Math.min(...dayIndexes);
}

export function buildAppUsageSnapshot(
  result: AppUsageQueryResult,
  opts: BuildAppUsageOptions,
): AppUsageSnapshot {
  const { totals, turnTotals, toolTotals } = result;

// The inputTokens of the usage library are already total input, and the cache field is just breakdown.
// The denominator of the hit rate cannot be added with cacheRead/cacheCreation, otherwise the hit rate will be lowered.
  const cacheDenom =
    totals.inputTokens > 0
      ? totals.inputTokens
      : totals.cacheCreationTokens + totals.cacheReadTokens;
  const cacheHitRate = cacheDenom > 0 ? totals.cacheReadTokens / cacheDenom : 0;
  const modelErrorRate =
    totals.modelRequestCount > 0 ? totals.modelErrorCount / totals.modelRequestCount : 0;
  const toolErrorRate =
    toolTotals.toolCallCount > 0 ? toolTotals.toolErrorCount / toolTotals.toolCallCount : 0;

// Token mapping by day, used for activeDays / streak / heatmap
  const dayTokenMap = new Map<
    number,
    { totalTokens: number; turnCount: number; toolCallCount: number }
  >();
  for (const d of result.days) {
    dayTokenMap.set(d.dayIndex, {
      totalTokens: d.totalTokens,
      turnCount: d.turnCount,
      toolCallCount: d.toolCallCount,
    });
  }

  const endDayIndex = Math.floor((opts.until + opts.tzOffsetMs) / DAY_MS);
  const startDayIndex = resolveUsageStartDayIndex(result, opts, endDayIndex);

  let activeDays = 0;
  let currentStreakDays = 0;
  let longestStreakDays = 0;
  let runningStreakDays = 0;
  let streakBroken = false;
  for (let di = endDayIndex; di >= startDayIndex; di--) {
    const tokens = dayTokenMap.get(di)?.totalTokens ?? 0;
    if (tokens > 0) {
      activeDays++;
      runningStreakDays++;
      longestStreakDays = Math.max(longestStreakDays, runningStreakDays);
      if (!streakBroken) currentStreakDays++;
    } else if (!streakBroken) {
      streakBroken = true;
      runningStreakDays = 0;
    } else {
      runningStreakDays = 0;
    }
  }

  const maxTokens = result.days.reduce((m, d) => Math.max(m, d.totalTokens), 0);

// heatmap: from startDayIndex to endDayIndex, sliced ​​by 7-day week (consistent with existing GitHub formula)
  const weeks: AppUsageHeatmapWeek[] = [];
  let week: Array<AppUsageHeatmapCell | null> = [];
  for (let di = startDayIndex; di <= endDayIndex; di++) {
    const day = dayTokenMap.get(di);
    week.push({
      date: dayIndexToDate(di),
      level: levelFor(day?.totalTokens ?? 0, maxTokens),
      totalTokens: day?.totalTokens ?? 0,
      turnCount: day?.turnCount ?? 0,
      toolCallCount: day?.toolCallCount ?? 0,
    });
    if (week.length === 7) {
      weeks.push({ weekIndex: weeks.length, days: week });
      week = [];
    }
  }
  if (week.length > 0) {
    while (week.length < 7) week.push(null);
    weeks.push({ weekIndex: weeks.length, days: week });
  }

  const heatmap: AppUsageHeatmap = {
    startDate: dayIndexToDate(startDayIndex),
    endDate: dayIndexToDate(endDayIndex),
    maxTokens,
    weeks,
  };

// trend: aggregate dayModels by day
  const dailyMap = new Map<number, Map<string | null, number>>();
  for (const dm of result.dayModels) {
    const inner = dailyMap.get(dm.dayIndex) ?? new Map<string | null, number>();
    inner.set(dm.modelId, (inner.get(dm.modelId) ?? 0) + dm.totalTokens);
    dailyMap.set(dm.dayIndex, inner);
  }
  const dailyModelUsage: AppUsageSnapshot["dailyModelUsage"] = [];
  for (let di = startDayIndex; di <= endDayIndex; di++) {
    const inner = dailyMap.get(di);
    dailyModelUsage.push({
      date: dayIndexToDate(di),
      models: inner
        ? [...inner.entries()].map(([modelId, totalTokens]) => ({ modelId, totalTokens }))
        : [],
    });
  }

// Model ranking + favorite
  const totalModelTokens = result.models.reduce((s, m) => s + m.totalTokens, 0);
  const models = result.models.map((m) => ({
    modelId: m.modelId,
    totalTokens: m.totalTokens,
    inputTokens: m.inputTokens,
    outputTokens: m.outputTokens,
    requestCount: m.requestCount,
    share: totalModelTokens > 0 ? m.totalTokens / totalModelTokens : 0,
  }));
  const favoriteModel =
    models.length > 0
      ? { modelId: models[0].modelId, totalTokens: models[0].totalTokens, share: models[0].share }
      : null;

  const tools = result.tools.map((t) => ({
    toolName: t.toolName,
    callCount: t.callCount,
    errorCount: t.errorCount,
    errorRate: t.callCount > 0 ? t.errorCount / t.callCount : 0,
    avgDurationMs: t.avgDurationMs,
  }));

  return {
    range: opts.range,
    generatedAt: opts.generatedAt,
    timeZone: opts.timeZone,
    source: "agent-db",
    summary: {
      totalTokens: totals.totalTokens,
      inputTokens: totals.inputTokens,
      outputTokens: totals.outputTokens,
      reasoningTokens: totals.reasoningTokens,
      cacheCreationTokens: totals.cacheCreationTokens,
      cacheReadTokens: totals.cacheReadTokens,
      cacheHitRate,
      totalSessions: turnTotals.totalSessions,
      totalTurns: turnTotals.totalTurns,
      toolCallCount: toolTotals.toolCallCount,
      toolErrorRate,
      modelErrorRate,
      avgTimeToFirstTokenMs: totals.avgTimeToFirstTokenMs,
      avgTurnDurationMs: turnTotals.avgTurnDurationMs,
      activeDays,
      currentStreakDays,
      longestSessionMs: turnTotals.longestSessionMs,
      longestStreakDays,
      peakDayTokens: maxTokens,
      favoriteModel,
    },
    heatmap,
    dailyModelUsage,
    models,
    tools,
  };
}
