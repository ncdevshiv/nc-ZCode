import type { RuntimeModelTextResult } from "../types.js";
import type { AgentRuntimeInternal } from "../internal.js";
import { toTokenUsageInfo } from "../helpers/index.js";
import {
  countContextPrefixMessages,
  createRuntimeAssistantEntry,
  createRuntimeUserEntry,
  type RuntimeMessageEntry,
} from "../../agent/message-history.js";
import type { RegularTurnLoopState, TurnRequestState } from "./turn-loop-state.js";

const OUTPUT_TOKEN_CONTINUE_PROMPT =
  "Output token limit hit. Resume directly — no apology, no recap of what you were doing. Pick up mid-thought if that is where the cut happened. Break remaining work into smaller pieces.";

export const OUTPUT_TOKEN_LIMIT_ERROR_MESSAGE =
  "The model's response exceeded the output token maximum.";

const MAX_OUTPUT_TOKEN_CONTINUATIONS = 3;
const OUTPUT_LIMIT_RAW_REASONS = new Set([
  "max_tokens",
  "max_output_tokens",
  "model_context_window_exceeded",
]);

type OutputTokenContinuationDecision = "continue" | "exhausted" | "none";

export function classifyOutputTokenContinuation(input: {
  finishReason: string | undefined;
  rawFinishReason: string | undefined;
  toolCallCount: number;
  continuationCount: number;
}): OutputTokenContinuationDecision {
  if (input.toolCallCount > 0) return "none";
  if (!isOutputTokenLimitFinishReason(input.finishReason, input.rawFinishReason)) {
    return "none";
  }
  return input.continuationCount < MAX_OUTPUT_TOKEN_CONTINUATIONS ? "continue" : "exhausted";
}

export function isOutputTokenLimitFinishReason(
  finishReason: string | undefined,
  rawFinishReason: string | undefined,
): boolean {
// Intentional design (not a bug): successfully responded model_context_window_exceeded belongs to Continue.

// Share up to 3 restores even if the content is empty;
// Anthropic allows input not to exceed the window but input + max_tokens to exceed the window; generating a filled window will truncate the successful response.
// This is not equivalent to the request failure caused by the input itself exceeding the window. It cannot be converted into an exception by name and preempted by Reactive Compact.
// Difference in behavior: I tried Reactive Compact when I saw the success stop reason before; now save partial,
//Append Continue, then return to the outer loop to check Micro/Auto Compact, and initiate continuation after compression if necessary.
// If a true over-window exception is thrown during the continuation, Reactive Compact will retry the current continuation after success. If it cannot recover, an error will be reported;
// Compression neither appends additional Continue nor resets the used count. The three limit does not include compression requests and exception retries.
// If the truncation is still successful for the fourth time, an error will be reported according to the output upper limit, and Reactive Compact will not be tried again.
// Therefore, if the server continues to return this mark and local automatic compression is not triggered, it may be exhausted after multiple invalid renewals;
// This trade-off retains the original agreement of continuation first and compression on demand. There is no guarantee that the next continuation will be successful.
  return finishReason === "length" || OUTPUT_LIMIT_RAW_REASONS.has(rawFinishReason ?? "");
}

function createOutputTokenContinuationEntry(): RuntimeMessageEntry {
  return {
    ...createRuntimeUserEntry(OUTPUT_TOKEN_CONTINUE_PROMPT),
    queryScope: "output_token_continuation",
  };
}

function isOutputTokenContinuationEntry(entry: RuntimeMessageEntry): boolean {
  return entry.kind !== "attachment" && entry.queryScope === "output_token_continuation";
}

export function filterOutputTokenContinuationEntries(
  entries: readonly RuntimeMessageEntry[],
): readonly RuntimeMessageEntry[] {
  const firstContinuation = entries.findIndex(isOutputTokenContinuationEntry);
  if (firstContinuation < 0) return entries;
  return entries.filter((entry) => !isOutputTokenContinuationEntry(entry));
}

export function preserveCanonicalContextPrefix(
  currentCanonicalEntries: readonly RuntimeMessageEntry[],
  turnLocalEntries: readonly RuntimeMessageEntry[],
): readonly RuntimeMessageEntry[] {
// Configuration refresh will immediately replace the canonical prefix, while the recovery chain continues to hold the old atomic Turn
// prefix. turn-local Compact will cancel the refresh if it is written back as a whole, so only the converted conversation tail is submitted.
  return [
    ...currentCanonicalEntries.slice(0, countContextPrefixMessages(currentCanonicalEntries)),
    ...turnLocalEntries.slice(countContextPrefixMessages(turnLocalEntries)),
  ];
}

export function appendTurnRequestEntries(
  state: TurnRequestState,
  entries: readonly RuntimeMessageEntry[],
): void {
  if (entries.length === 0) return;
  state.entries = [...state.entries, ...entries];
}

export function commitTurnRequestEntries(
  runtime: AgentRuntimeInternal,
  state: TurnRequestState,
  entries: readonly RuntimeMessageEntry[],
): void {
  if (entries.length === 0) return;
  runtime.messageHistory.addEntries(entries);
  appendTurnRequestEntries(state, entries);
}

export function commitAssistantToTurnRequest(
  runtime: AgentRuntimeInternal,
  state: RegularTurnLoopState,
  result: RuntimeModelTextResult,
  toolCalls: Parameters<typeof createRuntimeAssistantEntry>[1],
): boolean {
  const reasoning = result.reasoning?.filter(hasAssistantReasoningContent);
  const hasAssistantContent =
    state.modelResponse.length > 0 || (reasoning?.length ?? 0) > 0 || (toolCalls?.length ?? 0) > 0;
  if (!hasAssistantContent) return false;
// provider result metadata may report other models; assistant attribution must be used
// Turn the bound Model to avoid the recovery chain reintroducing the old provider-owned model identity.
  const modelRef = { providerId: state.model.providerId, modelId: state.model.modelId };
  commitTurnRequestEntries(runtime, state.turnRequestState, [
    createRuntimeAssistantEntry(
      state.modelResponse,
      toolCalls,
      reasoning,
      modelRef,
      toTokenUsageInfo(result.usage),
    ),
  ]);
  return true;
}

export function hasAssistantReasoningContent(
  reasoning: NonNullable<RuntimeModelTextResult["reasoning"]>[number],
): boolean {
  return reasoning.text.length > 0 || Object.keys(reasoning.providerOptions ?? {}).length > 0;
}

export function appendOutputTokenContinuation(state: TurnRequestState): void {
  appendTurnRequestEntries(state, [createOutputTokenContinuationEntry()]);
  state.outputTokenContinuationCount += 1;
}

export function completeOutputTokenRecovery(state: TurnRequestState): void {
  state.outputTokenContinuationCount = 0;
}

export function finishOutputTokenRecovery(state: TurnRequestState): void {
  completeOutputTokenRecovery(state);
}
