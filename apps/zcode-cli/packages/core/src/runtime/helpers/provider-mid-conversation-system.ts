import type {
  ModelInputMessage,
  RuntimeMessageEntry,
  RuntimeMessageMessageEntry,
} from "../../agent/message-history.js";
import {
  isRuntimeAttachmentEntry,
  isKnownSystemReminderSource,
} from "../../agent/message-history.js";
import {
  isMidConversationSystemSource,
  sanitizeSystemReminderBody,
  wrapSystemReminder,
} from "../../system-reminder/source.js";
import { isPresentedInput, type ProviderEntryOrigins } from "./provider-entry-origins.js";

interface PendingMidSystemEntry {
  entry: RuntimeMessageEntry;
  text: string;
}

interface MidSystemProjection {
  fallbackBody: string;
}

interface ProjectedMidSystemMessageEntry extends RuntimeMessageMessageEntry {
  midSystemProjection: MidSystemProjection;
}

export type ProjectedRuntimeMessageEntry = RuntimeMessageEntry | ProjectedMidSystemMessageEntry;

interface MidSystemProjectionResult {
  entries: ProjectedRuntimeMessageEntry[];
}

export function projectMidConversationSystemEntries(
  entries: readonly RuntimeMessageEntry[],
  origins: ProviderEntryOrigins,
): MidSystemProjectionResult {
  const projected: ProjectedRuntimeMessageEntry[] = [];
  const pending: PendingMidSystemEntry[] = [];

  const flushPending = (): void => {
    if (pending.length === 0) return;
    const pendingItems = pending.splice(0);
    const body = pendingItems.map((item) => item.text).join("\n\n");
    const previous = projected.at(-1);

    if (previous && isProjectedMidSystemEntry(previous)) {
      origins.set(previous, [previous, ...pendingItems.map((item) => item.entry)]);
      previous.message = {
        ...previous.message,
        content: `${previous.message.content}\n\n${body}`,
      };
      previous.midSystemProjection.fallbackBody = `${previous.midSystemProjection.fallbackBody}\n\n${body}`;
      return;
    }

    if (previous && canAnchorMidConversationSystemAfter(previous)) {
      const systemEntry: ProjectedMidSystemMessageEntry = {
        message: {
          role: "system",
          content: body,
        },
        midSystemProjection: { fallbackBody: body },
      };
      origins.set(
        systemEntry,
        pendingItems.map((item) => item.entry),
      );
      projected.push(systemEntry);
      return;
    }

    projected.push(...pendingItems.map((item) => item.entry));
  };

  for (const entry of entries) {
// Midway input is a causal boundary: it cannot be crossed by ordinary reminders, nor by subsequent users.
    const hasIncoming = pending.some((item) => isPresentedInput(item.entry));
    const nextIsUser = !isRuntimeAttachmentEntry(entry) && entry.message.role === "user";
    if (pending.length > 0 && (isPresentedInput(entry) || (hasIncoming && nextIsUser)))
      flushPending();
    const projectedText = midConversationSystemText(entry);
    if (projectedText !== undefined) {
      pending.push({ entry, text: projectedText });
      continue;
    }

    if (pending.length > 0 && shouldFlushMidConversationSystemBefore(entry)) {
      flushPending();
    }

    projected.push(entry);
  }

  flushPending();

  return { entries: validateMidConversationSystemPositions(projected, origins) };
}

function midConversationSystemText(entry: RuntimeMessageEntry): string | undefined {
  if (isRuntimeAttachmentEntry(entry)) {
    const source = entry.metadata.source;
    if (!isKnownSystemReminderSource(source)) return undefined;
    if (!isMidConversationSystemSource(source)) return undefined;
    return entry.content;
  }
  return undefined;
}

function shouldFlushMidConversationSystemBefore(entry: RuntimeMessageEntry): boolean {
  if (isRuntimeAttachmentEntry(entry)) return false;
// The pending system reminder needs to wait until the assistant or the system boundary is set.
// Avoid inserting the same set of tool results in the middle, causing the provider-visible order to be illegal.
  return entry.message.role === "assistant" || entry.message.role === "system";
}

function canAnchorMidConversationSystemAfter(entry: ProjectedRuntimeMessageEntry): boolean {
  if (isRuntimeAttachmentEntry(entry)) return false;
  const message = entry.message;
// Legal anchor alignment provider-visible role for mid-conversation system,
// model-only user (such as target continuation) is still a user message and cannot be accidentally downgraded due to internal source metadata.
  if (message.role === "tool") return true;
  return message.role === "user";
}

function validateMidConversationSystemPositions(
  entries: readonly ProjectedRuntimeMessageEntry[],
  origins: ProviderEntryOrigins,
): ProjectedRuntimeMessageEntry[] {
  const projected: ProjectedRuntimeMessageEntry[] = [];

  for (let index = 0; index < entries.length; index++) {
    const entry = entries[index]!;
    if (!isProjectedMidSystemEntry(entry)) {
      projected.push(entry);
      continue;
    }

    const previous = projected.at(-1);
    const next = entries[index + 1];
    if (previous && canKeepMidConversationSystemBetween(previous, next)) {
      projected.push(entry);
      continue;
    }

// The mid-conversation system is only retained at the legal boundary of the provider;
// Other locations are downgraded to the old user system-reminder to avoid generating illegal role sequences in the request body.
    const fallback: RuntimeMessageEntry = {
      message: {
        role: "user",
// The MCS body has not been wrapped by user, so the downgrade must be escaped by itself and cannot be assumed to have been processed by the producer.
        content: wrapSystemReminder(
          sanitizeSystemReminderBody(entry.midSystemProjection.fallbackBody),
        ),
      },
      metadata: { source: "legacy_synthetic" },
    };
    origins.set(fallback, [entry]);
    projected.push(fallback);
  }

  return projected;
}

function canKeepMidConversationSystemBetween(
  previous: ProjectedRuntimeMessageEntry,
  next: ProjectedRuntimeMessageEntry | undefined,
): boolean {
  if (!canAnchorMidConversationSystemAfter(previous)) return false;
  if (!next) return true;
  if (isProjectedMidSystemEntry(next)) return true;
  if (isRuntimeAttachmentEntry(next)) return false;
  return next.message.role === "assistant";
}

function isProjectedMidSystemEntry(
  entry: ProjectedRuntimeMessageEntry | undefined,
): entry is ProjectedMidSystemMessageEntry {
  return Boolean(entry && "midSystemProjection" in entry && entry.midSystemProjection);
}

export function moveLegacySystemRemindersAfterToolResultRun(
  entries: readonly ProjectedRuntimeMessageEntry[],
): ProjectedRuntimeMessageEntry[] {
// legacy/fallback system-reminder is ordinary user text;
// If sandwiched in the same group of tool results, Anthropic serialization will only merge roles and will not automatically put tool_result back before text.
  const projected: ProjectedRuntimeMessageEntry[] = [];
  const pendingLegacyReminders: ProjectedRuntimeMessageEntry[] = [];

  const flushPendingLegacyReminders = (): void => {
    if (pendingLegacyReminders.length === 0) return;
    projected.push(...pendingLegacyReminders.splice(0));
  };

  for (const entry of entries) {
    if (
      isLegacySystemReminderEntry(entry) &&
      (pendingLegacyReminders.length > 0 || isToolResultEntry(projected.at(-1)))
    ) {
      pendingLegacyReminders.push(entry);
      continue;
    }

    if (pendingLegacyReminders.length > 0) {
      if (isToolResultEntry(entry)) {
        projected.push(entry);
        continue;
      }
      flushPendingLegacyReminders();
    }

    projected.push(entry);
  }

  flushPendingLegacyReminders();
  return projected;
}

function isLegacySystemReminderEntry(entry: ProjectedRuntimeMessageEntry): boolean {
  if (isRuntimeAttachmentEntry(entry)) return true;
  return entry.message.role === "user" && entry.metadata?.source === "legacy_synthetic";
}

function isToolResultEntry(entry: ProjectedRuntimeMessageEntry | undefined): boolean {
  if (!entry || isRuntimeAttachmentEntry(entry)) return false;
  return entry.message.role === "tool" || isToolResultUserMessage(entry.message);
}

export function isToolResultUserMessage(message: ModelInputMessage): boolean {
  return message.role === "user" && Boolean(message.toolCallId || message.toolName);
}
