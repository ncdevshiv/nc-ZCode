import { CompactTrigger } from "@zcode/contracts";
import { ESTIMATED_TOKEN_CHAR_DIVISOR } from "@zcode/shared";
import type {
  CompactBoundaryPayload,
  CompactPhase,
  CompactPreservedSegment,
  CompactReason,
  CompactTrigger as CompactTriggerValue,
  MessageId,
  ModelMessageContent,
  TraceContext,
} from "@zcode/contracts";
import { modelMessageContentBlockToText, modelMessageContentToText } from "@zcode/contracts";
import { groupByAssistantStartedRounds } from "./rounds.js";

const EMPTY_TOOL_CALL_INPUT_JSON = "{}";

export interface CompactModelMessage {
  role: string;
  content: ModelMessageContent;
  toolCalls?: readonly {
    name: string;
    input: unknown;
  }[];
}

export interface TokenUsageLike {
  inputTokens?: number;
  outputTokens?: number;
  totalTokens?: number;
  reasoningTokens?: number;
  cacheReadTokens?: number;
  cacheWriteTokens?: number;
}

export interface BuildManualCompactBoundaryInput {
  autoCompactThreshold?: number;
  boundaryId: string;
  compactReason?: CompactReason;
  customInstructions?: string;
  keptMessageCount?: number;
  lastSummarizedMessageId?: MessageId;
  phase?: CompactPhase;
  postCompactTokenCount?: number;
  preservedSegment?: CompactPreservedSegment;
  preCompactTokenCount: number;
  summarizedMessageCount: number;
  summaryMessageId: MessageId;
  traceContext: TraceContext;
  trigger?: CompactTriggerValue;
  truePostCompactTokenCount?: number;
  willRetriggerNextTurn?: boolean;
}

export const MAX_COMPACT_PROMPT_TOO_LONG_RETRIES = 3;
export const COMPACT_PROMPT_TOO_LONG_RETRY_MARKER =
  "[earlier conversation truncated for compaction retry]";
export const COMPACT_PROMPT_TOO_LONG_USER_MESSAGE =
  "Conversation too long to compact automatically. Try /compact again after narrowing the active context.";

export function getMessagesToSummarize(
  messages: readonly CompactModelMessage[],
): CompactModelMessage[] {
  return messages
    .filter((message) => !isContextPrefixMessage(message))
    .map((message) => ({ ...message }));
}

export function hasEnoughMessagesToCompact(messages: readonly CompactModelMessage[]): boolean {
  const messagesToSummarize = getMessagesToSummarize(messages);
  return (
    groupMessagesByCompactRound(messagesToSummarize).length >= 2 &&
    messagesToSummarize.some((message) => message.role === "assistant")
  );
}

export function buildManualCompactBoundary(
  input: BuildManualCompactBoundaryInput,
): CompactBoundaryPayload {
  return {
    boundaryId: input.boundaryId,
    trigger: input.trigger ?? CompactTrigger.Manual,
    phase: input.phase,
    compactReason: input.compactReason,
    summarySource: "model",
    preCompactTokenCount: input.preCompactTokenCount,
    postCompactTokenCount: input.postCompactTokenCount,
    truePostCompactTokenCount: input.truePostCompactTokenCount,
    autoCompactThreshold: input.autoCompactThreshold,
    willRetriggerNextTurn: input.willRetriggerNextTurn,
    summarizedMessageCount: input.summarizedMessageCount,
    keptMessageCount: input.keptMessageCount ?? 0,
    lastSummarizedMessageId: input.lastSummarizedMessageId,
    preservedSegment: input.preservedSegment,
    summaryMessageIds: [input.summaryMessageId],
    customInstructions: input.customInstructions !== undefined,
    traceId: input.traceContext.traceId,
    turnId: input.traceContext.turnId,
  };
}

export function estimateMessageTokens(messages: readonly CompactModelMessage[]): number {
  return messages.reduce((total, message) => {
    let estimatedCharacterCount = modelMessageContentToTokenEstimateText(message.content).length;
// assistant toolCalls are saved independently outside content. The old estimate only reads content.
// Large tool input will be sent to the provider in its entirety, but will be counted as 0 in auto compact and preflight.
    for (const toolCall of message.toolCalls ?? []) {
      estimatedCharacterCount += (
        toolCall.name + stringifyToolCallInputForTokenEstimate(toolCall.input)
      ).length;
    }
    return total + Math.ceil(estimatedCharacterCount / ESTIMATED_TOKEN_CHAR_DIVISOR);
  }, 0);
}

function stringifyToolCallInputForTokenEstimate(input: unknown): string {
  try {
    return JSON.stringify(input ?? {}) ?? EMPTY_TOOL_CALL_INPUT_JSON;
  } catch {
// tool_use degrades to a JSON representation of an empty object for estimation by the estimator when parsing fails.
// Model inputs to ZCode may still contain unknown content; abnormal inputs must not cause local budget estimates to break compact.
    return EMPTY_TOOL_CALL_INPUT_JSON;
  }
}

function modelMessageContentToTokenEstimateText(content: ModelMessageContent): string {
  if (typeof content === "string") return content;

// modelMessageContentToText is a "visible text" projection, which will intentionally hide reasoning;
// compact fallback treats it as the provider context volume, causing reasoning to be all counted as 0 when there is no usage anchor.
// Token estimation uses independent projection to avoid changing the semantics of existing consumers such as text, memory, error copy, etc.
  return content
    .map((block) =>
      block.type === "reasoning" ? block.text : modelMessageContentBlockToText(block),
    )
    .filter(Boolean)
    .join("\n\n");
}

function isContextPrefixMessage<T extends CompactModelMessage>(message: T): boolean {
  return (
    message.role === "system" ||
    (message.role === "user" &&
      modelMessageContentToText(message.content).trimStart().startsWith("<system-reminder>"))
  );
}

function groupMessagesByCompactRound<T extends CompactModelMessage>(messages: readonly T[]): T[][] {
  return groupByAssistantStartedRounds(messages, (message) => message.role);
}

export function getUsageTotalTokens(usage?: TokenUsageLike): number {
  const inputTokens =
    usage?.inputTokens ?? (usage?.cacheReadTokens ?? 0) + (usage?.cacheWriteTokens ?? 0);
  return usage?.totalTokens ?? inputTokens + (usage?.outputTokens ?? 0);
}

export function createCompactBoundaryId(
  randomUUID: () => string = () => crypto.randomUUID(),
): string {
  return `compact_${randomUUID()}`;
}
