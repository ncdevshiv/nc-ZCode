import type { TokenUsageInfo } from "@zcode/contracts";

export interface PersistedTokenUsageBaseline {
  cacheReadTokens: number;
  cacheWriteTokens: number;
  contextUsageTokens?: number;
  inputTokens: number;
  outputTokens: number;
}

export function persistedTokenUsageBaseline(
  tokens: TokenUsageInfo | undefined,
): PersistedTokenUsageBaseline | undefined {
  if (!tokens) return undefined;
  const inputTokens = persistedInputWindowTokens(tokens);
  if (inputTokens === undefined || inputTokens <= 0) return undefined;

  const outputTokens = positiveInteger(tokens.output) ?? 0;
  const cacheReadTokens = nonNegativeInteger(tokens.cache.read) ?? 0;
  const cacheWriteTokens = nonNegativeInteger(tokens.cache.write) ?? 0;
  const totalTokens = positiveInteger(tokens.total);
//History TokenUsageInfo will normalize missing provider outputTokens to 0.
// When total is not available, 0 cannot prove that usage has covered assistant, and assistant must be left for local estimation.
  const contextUsageTokens =
    outputTokens > 0
      ? inputTokens + outputTokens
      : totalTokens !== undefined && totalTokens >= inputTokens
        ? totalTokens
        : undefined;

  return {
    cacheReadTokens,
    cacheWriteTokens,
    contextUsageTokens,
    inputTokens,
    outputTokens,
  };
}

function persistedInputWindowTokens(tokens: TokenUsageInfo): number | undefined {
  const inputTokens = positiveInteger(tokens.input);
  if (inputTokens !== undefined) return inputTokens;

  const totalTokens = positiveInteger(tokens.total);
  if (totalTokens !== undefined) {
    return Math.max(0, totalTokens - (nonNegativeInteger(tokens.output) ?? 0));
  }

  const cacheTokens =
    (nonNegativeInteger(tokens.cache.read) ?? 0) + (nonNegativeInteger(tokens.cache.write) ?? 0);
  return cacheTokens > 0 ? cacheTokens : undefined;
}

function positiveInteger(value: number | undefined): number | undefined {
  if (value === undefined || !Number.isFinite(value)) return undefined;
  const integer = Math.floor(value);
  return integer > 0 ? integer : undefined;
}

function nonNegativeInteger(value: number | undefined): number | undefined {
  if (value === undefined || !Number.isFinite(value)) return undefined;
  const integer = Math.floor(value);
  return integer >= 0 ? integer : undefined;
}
