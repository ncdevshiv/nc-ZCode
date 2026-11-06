import {
  CompactPhase,
  CompactReason,
  CompactTrigger,
  countContextPrefixMessages,
  isCoreError,
  maybeLocalMicrocompactMessages,
} from "../deps.js";
import type { LocalMicrocompactPolicyConfig } from "../deps.js";
import {
  cloneModelMessageContent,
  cloneRuntimeMessageEntry,
  invalidateRuntimeTokenUsage,
  isRuntimeAttachmentEntry,
  type RuntimeMessageEntry,
} from "../../agent/message-history.js";
import { buildProviderRequestMessages } from "./provider-request-messages.js";

export { buildPostCompactReadStateReminderEntries } from "./compact-post-reminders.js";
export { countCompactPreservedRuntimeMessages } from "./compact-preservation.js";
export {
  estimateRuntimeEntryTokens,
  getRuntimeEntriesToSummarize,
  hasEnoughRuntimeEntriesToCompact,
  selectCompactEntries,
  selectCompactEntriesAfterPromptTooLong,
  selectCompactEntriesForInitialPromptTooLong,
  truncateCompactSummaryRequestEntriesAfterPromptTooLong,
} from "./compact-selection.js";
export type { CompactEntrySelection } from "./compact-selection.js";

interface RuntimeMicrocompactResult {
  decision: ReturnType<typeof maybeLocalMicrocompactMessages>["decision"];
  entries: readonly RuntimeMessageEntry[];
  payload?: ReturnType<typeof maybeLocalMicrocompactMessages>["payload"];
}

export function defaultCompactPhaseForTrigger(trigger: CompactTrigger): CompactPhase {
  switch (trigger) {
    case CompactTrigger.Auto:
      return CompactPhase.PreRequest;
    case CompactTrigger.Reactive:
      return CompactPhase.Reactive;
    case CompactTrigger.Manual:
    case CompactTrigger.Partial:
    case CompactTrigger.SessionMemory:
      return CompactPhase.StandaloneTurn;
  }
}

export function defaultCompactReasonForTrigger(trigger: CompactTrigger): CompactReason {
  switch (trigger) {
    case CompactTrigger.Auto:
      return CompactReason.ContextLimit;
    case CompactTrigger.Reactive:
      return CompactReason.ProviderOverflow;
    case CompactTrigger.Manual:
    case CompactTrigger.Partial:
      return CompactReason.UserRequested;
    case CompactTrigger.SessionMemory:
      return CompactReason.ContextLimit;
  }
}

export function compactFailureReasonFromError(error: unknown): string {
  if (isCoreError(error)) {
    return error.type;
  }
  return error instanceof Error && error.message.length > 0 ? error.message : "unknown";
}

export function buildPostCompactRuntimeEntries(
  activeEntries: readonly RuntimeMessageEntry[],
  summaryEntry: RuntimeMessageEntry,
  options: {
    postCompactReminderEntries?: readonly RuntimeMessageEntry[];
    preservedEntries?: readonly RuntimeMessageEntry[];
  } = {},
): RuntimeMessageEntry[] {
// After compaction, only the prefix of the metadata tag is retained to avoid users' literal <system-reminder> being mistakenly retained by text rules.
  const prefixCount = countContextPrefixMessages(activeEntries);
  return [
    ...activeEntries.slice(0, prefixCount).map(cloneRuntimeEntry),
    cloneRuntimeEntry(summaryEntry),
    ...(options.preservedEntries ?? []).map(cloneCompactPreservedRuntimeEntry),
    ...(options.postCompactReminderEntries ?? []).map(cloneRuntimeEntry),
  ];
}

export function maybeLocalMicrocompactRuntimeEntries(input: {
  config?: LocalMicrocompactPolicyConfig;
  entries: readonly RuntimeMessageEntry[];
  lastAssistantCompletedAtMs?: number;
  nowMs?: number;
  useMidConversationSystem?: boolean;
}): RuntimeMicrocompactResult {
  const providerMessages = buildProviderRequestMessages({
    entries: input.entries,
    applyCacheControl: false,
    useMidConversationSystem: input.useMidConversationSystem,
  }).messages;
  const result = maybeLocalMicrocompactMessages({
    config: input.config,
    lastAssistantCompletedAtMs: input.lastAssistantCompletedAtMs,
    messages: providerMessages,
    nowMs: input.nowMs,
  });

  if (!result.payload) {
    return {
      decision: result.decision,
      entries: input.entries,
    };
  }

  const entries = [...input.entries];
  const clearedToolCallIds = new Set<string>(result.payload.clearedToolCallIds);
  const clearedContentByToolCallId = new Map(
    result.messages
      .filter(
        (message) =>
          message.role === "tool" &&
          message.toolCallId &&
          clearedToolCallIds.has(message.toolCallId),
      )
      .map((message) => [message.toolCallId!, cloneModelMessageContent(message.content)]),
  );

  for (let index = 0; index < entries.length; index += 1) {
    const entry = entries[index]!;
    if (isRuntimeAttachmentEntry(entry)) continue;
    if (entry.message.role !== "tool" || !entry.message.toolCallId) continue;
    const clearedContent = clearedContentByToolCallId.get(entry.message.toolCallId);
    if (!clearedContent) continue;
// microcompact only changes the cleaned tool result; do copy-on-write on this entry,
// Avoid deep copying the entire history to protect read-only borrowing of MessageHistory.
    entries[index] = {
      ...entry,
      message: {
        ...entry.message,
        content: clearedContent,
      },
    };
  }

  return {
    decision: result.decision,
    entries,
    payload: result.payload,
  };
}

function cloneRuntimeEntry(entry: RuntimeMessageEntry): RuntimeMessageEntry {
  return cloneRuntimeMessageEntry(entry);
}

function cloneCompactPreservedRuntimeEntry(entry: RuntimeMessageEntry): RuntimeMessageEntry {
  const cloned = cloneRuntimeEntry(entry);
  if (cloned.kind === "attachment" || cloned.message.role !== "assistant" || !cloned.tokens) {
    return cloned;
  }
  return {
    ...cloned,
    tokens: invalidateRuntimeTokenUsage(cloned.tokens),
  };
}
