import type {
  ModelInputMessage,
  ModelMessageContent,
  ModelMessageContentBlock,
  ModelReasoningContentBlock,
  ModelId,
  ModelProviderId,
} from "@zcode/contracts";
import { BUILTIN_MODEL_PROVIDER_IDS } from "@zcode/shared";
import { getStatusCode, unwrapRetryError } from "./failure-inspection.js";

const EMPTY_ASSISTANT_CONTENT_FALLBACK = "(no content)";
const REJECTED_REASONING_FALLBACK = "[Thinking removed]";

// The old history retains the builtin identity, and the current selection has been moved to the account identity; Individual/Team
// Different IDs will also be used. These unambiguous identities of the same service are only recognized during reasoning playback, without changing selection or authentication.
// Package display grouping cannot be reused: Start/Off-Peak/API access is not within the compatibility range of this signature.
const REASONING_PROVIDER_GROUPS: readonly (readonly string[])[] = [
  [
    "builtin:zai-coding-plan",
    BUILTIN_MODEL_PROVIDER_IDS.zaiIndividualCodingPlan,
    BUILTIN_MODEL_PROVIDER_IDS.zaiTeamCodingPlan,
  ],
  [
    "builtin:bigmodel-coding-plan",
    BUILTIN_MODEL_PROVIDER_IDS.bigmodelIndividualCodingPlan,
    BUILTIN_MODEL_PROVIDER_IDS.bigmodelTeamCodingPlan,
  ],
];

export function normalizeReasoningHistory(
  messages: ModelInputMessage[],
  targetModel?: { modelId: ModelId; providerId: ModelProviderId },
): ModelInputMessage[] {
  const compatibleHistory = removeCrossModelReasoning(messages, targetModel);
  const withoutOrphans = removeReasoningOnlyAssistants(compatibleHistory);
  const withoutTrailingReasoning = removeTrailingReasoning(withoutOrphans);
  const withoutWhitespaceOnlyAssistants = removeWhitespaceOnlyAssistants(withoutTrailingReasoning);
  return repairEmptyAssistantContent(withoutWhitespaceOnlyAssistants);
}

function removeRejectedReasoning(messages: ModelInputMessage[]): ModelInputMessage[] {
  const filtered = filterReasoningBlocks(messages, isSignedOrRedactedReasoning);
  if (filtered === messages) return messages;

  const result = filtered.slice();
  for (let index = 0; index < filtered.length; index += 1) {
    if (filtered[index] === messages[index]) continue;

    const message = filtered[index]!;
    if (message.role !== "assistant" || !Array.isArray(message.content)) continue;

    let content = message.content.filter(
      (block) => block.type !== "text" || block.text.trim().length > 0,
    );
    if (!hasToolCalls(message) && content.every((block) => block.type === "reasoning")) {
      content = [...content, { type: "text", text: REJECTED_REASONING_FALLBACK }];
    }
    result[index] = { ...message, content };
  }
  return result;
}

export function repairReasoningHistoryAfterSignatureRejection(
  projectedMessages: ModelInputMessage[],
  error: unknown,
): ModelInputMessage[] | undefined {
  if (!isThinkingSignatureRejection(error)) return undefined;

// What the caller passes in is already a copy of the logical request entry after the structure has been normalized. Only signature rejection cleanup is performed here,
// The structure passes cannot be run again, otherwise the newly filled assistant placeholder will be deleted and the round boundaries will be changed.
  const repaired = removeRejectedReasoning(projectedMessages);
  return repaired === projectedMessages ? undefined : repaired;
}

function isThinkingSignatureRejection(error: unknown): boolean {
  const unwrapped = unwrapRetryError(error);
  if (getStatusCode(unwrapped) !== 400) return false;

// Provider does not provide a separate error code for this 400; only confirmed narrowed copy is matched here.
// Avoid mistaking other invalid_requests as modifiable history and try again.
  const message = errorMessage(unwrapped).toLowerCase();
  if (message.includes("signature in thinking block")) return true;

  const namesThinkingBlock =
    message.includes("thinking block") ||
    message.includes("`thinking`") ||
    message.includes("redacted_thinking");
  const namesSignatureFailure =
    message.includes("cannot be modified") || message.includes("invalid signature");
  return namesThinkingBlock && namesSignatureFailure;
}

function removeCrossModelReasoning(
  messages: ModelInputMessage[],
  targetModel: { modelId: ModelId; providerId: ModelProviderId } | undefined,
): ModelInputMessage[] {
  if (!targetModel) return messages;

  return filterReasoningBlocks(messages, (block, message) => {
    if (!message.providerId || !message.modelId) return false;
    if (
      message.modelId === targetModel.modelId &&
      areReasoningProvidersCompatible(message.providerId, targetModel.providerId)
    ) {
      return false;
    }
    return isSignedOrRedactedReasoning(block);
  });
}

function areReasoningProvidersCompatible(
  source: ModelProviderId,
  target: ModelProviderId,
): boolean {
  return (
    source === target ||
    REASONING_PROVIDER_GROUPS.some((group) => group.includes(source) && group.includes(target))
  );
}

function filterReasoningBlocks(
  messages: ModelInputMessage[],
  shouldRemove: (block: ModelReasoningContentBlock, message: ModelInputMessage) => boolean,
): ModelInputMessage[] {
  let result: ModelInputMessage[] | undefined;

  for (let index = 0; index < messages.length; index += 1) {
    const message = messages[index]!;
    if (message.role !== "assistant" || !Array.isArray(message.content)) continue;

    const content = message.content.filter(
      (block) => block.type !== "reasoning" || !shouldRemove(block, message),
    );
    if (content.length === message.content.length) continue;

    result ??= messages.slice();
// Only the reasoning block is filtered here, and the message order and structure are preserved; structural repair is handled by an independent stage.
    result[index] = { ...message, content };
  }

  return result ?? messages;
}

function removeReasoningOnlyAssistants(messages: ModelInputMessage[]): ModelInputMessage[] {
  const result = messages.filter((message) => {
    if (message.role !== "assistant" || hasToolCalls(message)) return true;
    if (!Array.isArray(message.content) || message.content.length === 0) return true;
    return !message.content.every((block) => block.type === "reasoning");
  });
  return result.length === messages.length ? messages : result;
}

function removeTrailingReasoning(messages: ModelInputMessage[]): ModelInputMessage[] {
  const last = messages.at(-1);
  if (last?.role !== "assistant" || !Array.isArray(last.content)) return messages;

  let end = last.content.length;
  while (end > 0 && last.content[end - 1]?.type === "reasoning") {
    end -= 1;
  }
  if (end === last.content.length) return messages;

  const result = messages.slice();
  result[result.length - 1] = {
    ...last,
    content: last.content.slice(0, end),
  };
  return result;
}

function removeWhitespaceOnlyAssistants(messages: ModelInputMessage[]): ModelInputMessage[] {
  const filtered = messages.filter((message) => {
    if (message.role !== "assistant" || hasToolCalls(message)) return true;
    if (!Array.isArray(message.content) || message.content.length === 0) return true;
    return !message.content.every(
      (block) =>
        block.type === "text" &&
        (block.text.trim().length === 0 || block.text.trim() === EMPTY_ASSISTANT_CONTENT_FALLBACK),
    );
  });
  if (filtered.length === messages.length) return messages;
  return mergeAdjacentUserMessages(filtered);
}

function repairEmptyAssistantContent(messages: ModelInputMessage[]): ModelInputMessage[] {
  let result: ModelInputMessage[] | undefined;

  for (let index = 0; index < messages.length - 1; index += 1) {
    const message = messages[index]!;
    if (
      message.role !== "assistant" ||
      !Array.isArray(message.content) ||
      message.content.length > 0 ||
      hasToolCalls(message)
    ) {
      continue;
    }

    result ??= messages.slice();
    result[index] = {
      ...message,
      content: [{ type: "text", text: EMPTY_ASSISTANT_CONTENT_FALLBACK }],
    };
  }

  return result ?? messages;
}

function mergeAdjacentUserMessages(messages: ModelInputMessage[]): ModelInputMessage[] {
  const result: ModelInputMessage[] = [];

  for (const message of messages) {
    const previous = result.at(-1);
    if (!canMergeAdjacentUserMessages(previous, message)) {
      result.push(message);
      continue;
    }

    const merged: ModelInputMessage = {
      role: "user",
      content: mergeUserContent(previous.content, message.content),
    };
    const cacheControl = message.cacheControl ?? previous.cacheControl;
    if (cacheControl) merged.cacheControl = { ...cacheControl };
    result[result.length - 1] = merged;
  }

  return result;
}

function canMergeAdjacentUserMessages(
  previous: ModelInputMessage | undefined,
  next: ModelInputMessage,
): previous is ModelInputMessage & { role: "user" } {
  if (previous?.role !== "user" || next.role !== "user") return false;
  return !isToolResultUserMessage(previous) && !isToolResultUserMessage(next);
}

function isToolResultUserMessage(message: ModelInputMessage): boolean {
  return message.role === "user" && Boolean(message.toolCallId || message.toolName);
}

function mergeUserContent(
  previous: ModelMessageContent,
  next: ModelMessageContent,
): ModelMessageContentBlock[] {
  const previousBlocks = contentAsBlocks(previous);
  const nextBlocks = contentAsBlocks(next);
  const previousLast = previousBlocks.at(-1);
  const nextFirst = nextBlocks[0];

  if (previousLast?.type === "text" && nextFirst?.type === "text") {
    previousBlocks[previousBlocks.length - 1] = {
      ...previousLast,
      text: `${previousLast.text}\n`,
    };
  }

  return [...previousBlocks, ...nextBlocks];
}

function contentAsBlocks(content: ModelMessageContent): ModelMessageContentBlock[] {
  return typeof content === "string" ? [{ type: "text", text: content }] : [...content];
}

function isSignedOrRedactedReasoning(block: ModelReasoningContentBlock): boolean {
  const anthropic = anthropicOptions(block.providerOptions);
  return (
    (typeof anthropic.signature === "string" && anthropic.signature.length > 0) ||
    typeof anthropic.redactedData === "string"
  );
}

function hasToolCalls(message: ModelInputMessage): boolean {
  return (message.toolCalls?.length ?? 0) > 0;
}

function anthropicOptions(providerOptions: unknown): Record<string, unknown> {
  if (!providerOptions || typeof providerOptions !== "object" || Array.isArray(providerOptions)) {
    return {};
  }
  const anthropic = (providerOptions as Record<string, unknown>).anthropic;
  return anthropic && typeof anthropic === "object" && !Array.isArray(anthropic)
    ? (anthropic as Record<string, unknown>)
    : {};
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (error && typeof error === "object" && "message" in error) {
    return String((error as { message?: unknown }).message ?? "");
  }
  return String(error ?? "");
}
