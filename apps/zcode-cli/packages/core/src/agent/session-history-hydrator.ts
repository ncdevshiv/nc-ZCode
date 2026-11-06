/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import { runtimeInputMetadata } from "./runtime-input-presentation.js";
// ============================================================
// Session History Hydration - rebuild provider-visible context
// ============================================================

import { modelMessageContentToText, selectActiveConversationBranch } from "@zcode/contracts";
import type {
  FilePart,
  MessagePart,
  MessageWithParts,
  ModelMessageContent,
  ModelMessageContentBlock,
  ModelReasoningContentBlock,
  MessageId,
  ToolArtifactStorePort,
  ToolPart,
} from "@zcode/contracts";
import {
  getSystemReminderDescriptor,
  wrapSystemReminderForSource,
  type SystemReminderSource,
} from "../system-reminder/source.js";
import {
  buildPromptAttachmentReminderBodies,
  type PromptAttachmentReminderInput,
} from "../system-reminder/prompt-attachment.js";
import { persistedTokenUsageBaseline } from "./message-history-usage.js";
import {
  isKnownSystemReminderSource,
  legacySyntheticRuntimeMetadata,
  realUserRuntimeMetadata,
  systemReminderAttachmentEntry,
  systemReminderRuntimeMetadata,
  todoReminderRuntimeMetadata,
  type MessageHistory,
  type RuntimeMessageEntry,
  type RuntimeMessageMetadata,
  type RuntimeMessageSource,
  type ToolCallInput,
} from "./message-history.js";
import { compactActiveSessionMessages, isActiveCompactionBoundaryPart } from "./compact-session.js";
import { filePartToContentBlock, projectPersistedToolMediaContent } from "./file-part-hydration.js";
import { selectToolPartsForHistory } from "./tool-part-order.js";

const INTERRUPTED_TOOL_RESULT = "[Tool execution was interrupted before resume]";

export interface SessionHistoryHydrationResult {
  appliedMessageCount: number;
  interruptedToolCount: number;
  messageCount: number;
  partCount: number;
}

export async function hydrateMessageHistoryFromSession(input: {
  artifactStore?: ToolArtifactStorePort;
  branchCutAfterMessageId?: MessageId;
  history: MessageHistory;
  messages: MessageWithParts[];
  rewindCreatedMessageId?: MessageId;
  rewindKeptMessageIds?: readonly MessageId[];
  rewindTargetMessageId?: MessageId;
}): Promise<SessionHistoryHydrationResult> {
  const activeMessages = activeSessionMessages(input.messages, {
    branchCutAfterMessageId: input.branchCutAfterMessageId,
    rewindCreatedMessageId: input.rewindCreatedMessageId,
    rewindKeptMessageIds: input.rewindKeptMessageIds,
    rewindTargetMessageId: input.rewindTargetMessageId,
  });
  let appliedMessageCount = 0,
    interruptedToolCount = 0,
    partCount = 0;

  for (const message of activeMessages) {
    const parts = dedupeParts(message.parts);
    partCount += parts.length;

    if (message.info.role === "user") {
      const sharedContextStatus =
        message.info.source === "shared_context" &&
        message.info.metadata &&
        typeof message.info.metadata === "object"
          ? (message.info.metadata as Record<string, unknown>).sharedContextStatus
          : undefined;
      if (
        message.info.source === "shared_context" &&
        sharedContextStatus !== undefined &&
        sharedContextStatus !== "attached"
      ) {
// The pending/reserved context of Share handover is only a local candidate and cannot be used for the first time by the user.
//Sneakly enter the provider history before sending; after attaching, it is explicitly injected once by the runtime.
        continue;
      }
// Session persistence is raw synthetic notice, if the hydrate phase is completed in advance
// user <system-reminder>, subsequent mid-conversation system projection will lose attachment source.
      const syntheticAttachment = syntheticSystemReminderAttachmentFromParts(parts);
      if (syntheticAttachment) {
        input.history.addAttachment(syntheticAttachment.source, syntheticAttachment.content);
        appliedMessageCount++;
        continue;
      }

      const entries = await userEntriesFromParts(parts, input.artifactStore);
      if (entries.length === 0) continue;
      const presentation = runtimeInputMetadata(
        (message.info.metadata as Record<string, unknown> | undefined)?.inputPresentation,
      );
      input.history.addEntries(
        presentation
          ? entries.map((entry) =>
              entry.kind === "attachment" ? entry : { ...entry, metadata: presentation },
            )
          : entries,
      );
      appliedMessageCount++;
      continue;
    }

    const text = assistantTextFromParts(parts);
    const reasoning = assistantReasoningFromParts(parts);
    const toolParts = selectToolPartsForHistory(parts.filter(isToolPart));
// Live history will retain the empty assistant with legal provider usage as the estimated anchor point.
// The old hydration is unconditionally discarded, causing the context estimate before and after restart to be inconsistent.
    if (
      text.trim().length === 0 &&
      reasoning.length === 0 &&
      toolParts.length === 0 &&
      !persistedTokenUsageBaseline(message.info.tokens)
    ) {
      continue;
    }

    input.history.addAssistant(
      text,
      toolParts.map(
        (part): ToolCallInput => ({
          id: part.callID,
          input: part.state.input,
          name: providerToolNameFromPart(part),
        }),
      ),
      reasoning,
      message.info.modelId && message.info.providerId
        ? { modelId: message.info.modelId, providerId: message.info.providerId }
        : undefined,
      message.info.tokens,
    );
    appliedMessageCount++;

    for (const part of toolParts) {
      const providerToolName = providerToolNameFromPart(part);
      if (part.state.status === "completed") {
// live tool result uses structured media, but old recovery only reads the output summary,
// Causes the real media produced by Read/MCP to be lost after a model switch or cold recovery.
        const attachmentBlocks = part.state.attachments
          ? await Promise.all(
              part.state.attachments.map((attachment) =>
                filePartToContentBlock(attachment, input.artifactStore),
              ),
            )
          : [];
// Old completed part may also have attachments; only a complete and valid layout can prove this
// They belong to new provider-visible media content and legacy output must be preserved when missing or corrupted.
        const projectedMediaContent =
          attachmentBlocks.length > 0
            ? projectPersistedToolMediaContent(
                part.state.metadata?.modelContentLayout,
                attachmentBlocks,
              )
            : undefined;
        const content = projectedMediaContent ?? part.state.output;
        input.history.addToolResult(part.callID, providerToolName, content, true);
        continue;
      }

      if (part.state.status === "error") {
        const persistedModelContent = part.state.metadata?.modelContent;
// The live link uses ToolExecutionResult.modelContent, but the old recovery logic only replays
// State.error for UI/logging; preferentially use persistent strings and be compatible with old sessions.
        input.history.addToolResult(
          part.callID,
          providerToolName,
          typeof persistedModelContent === "string" ? persistedModelContent : part.state.error,
          false,
        );
        continue;
      }

      interruptedToolCount++;
      input.history.addToolResult(part.callID, providerToolName, INTERRUPTED_TOOL_RESULT, false);
    }
  }

  return {
    appliedMessageCount,
    interruptedToolCount,
    messageCount: activeMessages.length,
    partCount,
  };
}

export function activeSessionMessages(
  messages: MessageWithParts[],
  options: {
    branchCutAfterMessageId?: MessageId;
    includeCompactPreservedSegment?: boolean;
    rewindCreatedMessageId?: MessageId;
    rewindKeptMessageIds?: readonly MessageId[];
    rewindTargetMessageId?: MessageId;
  } = {},
): MessageWithParts[] {
  if (!options.branchCutAfterMessageId) {
// The old data does not have branch cut, and continues to use compact-first/createdMessageID compatible semantics; it cannot be
// Historically illegal kept IDs before compact are interpreted as new branches, thereby changing the existing cold recovery results.
    let legacyCompactIndex = -1;
    for (let index = messages.length - 1; index >= 0; index--) {
      if (messages[index]!.parts.some(isActiveCompactionBoundaryPart)) {
        legacyCompactIndex = index;
        break;
      }
    }
    const compactActiveMessages =
      legacyCompactIndex >= 0
        ? compactActiveSessionMessages(
            messages,
            legacyCompactIndex,
            options.includeCompactPreservedSegment !== false,
          )
        : messages;
    if (options.rewindKeptMessageIds && legacyCompactIndex >= 0) {
      const postCompactIds = new Set(
        messages.slice(legacyCompactIndex).map((message) => message.info.id),
      );
      if (!options.rewindKeptMessageIds.some((messageId) => postCompactIds.has(messageId))) {
        return compactActiveMessages;
      }
    }
    return selectActiveConversationBranch(compactActiveMessages, options);
  }

//The last compact boundary. Reverse the order so that the kept prefix before compact can never be recovered.
  const branchActiveMessages = selectActiveConversationBranch(messages, options);
  let lastCompactionIndex = -1;
  for (let index = branchActiveMessages.length - 1; index >= 0; index--) {
    if (branchActiveMessages[index]!.parts.some(isActiveCompactionBoundaryPart)) {
      lastCompactionIndex = index;
      break;
    }
  }
  return lastCompactionIndex >= 0
    ? compactActiveSessionMessages(
        branchActiveMessages,
        lastCompactionIndex,
        options.includeCompactPreservedSegment !== false,
      )
    : branchActiveMessages;
}

function dedupeParts(parts: MessagePart[]): MessagePart[] {
  const byId = new Map<string, MessagePart>();
  for (const part of parts) {
    byId.set(part.id, part);
  }
  return [...byId.values()];
}

async function userEntriesFromParts(
  parts: MessagePart[],
  artifactStore: ToolArtifactStorePort | undefined,
): Promise<RuntimeMessageEntry[]> {
  const attachmentBlocks: ModelMessageContentBlock[] = [];
// Media data blocks (image/video) unify the post-group and restore order alignment to live main path [text, media].
  const inlineMediaBlocks: ModelMessageContentBlock[] = [];
  const promptBlocks: ModelMessageContentBlock[] = [];
  const syntheticAttachmentEntries: RuntimeMessageEntry[] = [];
  const promptAttachmentEntries: RuntimeMessageEntry[] = [];

  for (const part of parts) {
    if (part.type === "text" && !part.ignored) {
      const syntheticAttachment = syntheticSystemReminderAttachmentFromTextPart(part);
      if (syntheticAttachment) {
        syntheticAttachmentEntries.push(
          systemReminderAttachmentEntry(syntheticAttachment.source, syntheticAttachment.content),
        );
        continue;
      }
      promptBlocks.push({ type: "text", text: textPartToProviderText(part) });
      continue;
    }

    if (part.type === "file") {
      const block = await filePartToContentBlock(part, artifactStore);
// local_ref is the only path handle when restoring history of attachments, and cannot be filtered due to metadata_only.
      if (block.type === "text") {
        const promptAttachmentInput = promptAttachmentReminderInputForFilePart(part, block);
        if (promptAttachmentInput) {
          const reminderBody =
            buildPromptAttachmentReminderBodies(promptAttachmentInput).join("\n");
          promptAttachmentEntries.push(
            systemReminderAttachmentEntry("prompt_attachment", reminderBody),
          );
          continue;
        }
      }
      if (block.type === "image" || block.type === "video") {
        inlineMediaBlocks.push(block);
      } else {
        attachmentBlocks.push(block);
      }
      continue;
    }

    if (part.type === "agent") {
      promptBlocks.push({ type: "text", text: `[Selected agent: ${part.name}]` });
    }
  }

  const content = contentFromUserBlocks(
    [...attachmentBlocks, ...promptBlocks, ...inlineMediaBlocks],
    {
      preserveBlocks: attachmentBlocks.length > 0 || inlineMediaBlocks.length > 0,
    },
  );
  const hasUserContent = modelMessageContentToText(content).trim().length > 0;
  const userMetadata = metadataFromUserParts(parts);
// After the text attachment is restored from the original file part to prompt_attachment, the empty body
// real_user envelope was once discarded by trim, which caused the provider history of live and resume to be inconsistent.
// Only the Agent memory anchor is restored here; bare-empty and purely synthetic/meta users still do not generate empty messages.
  const shouldRestoreRealUserEnvelope =
    hasUserContent || (userMetadata.source === "real_user" && promptAttachmentEntries.length > 0);
  if (
    !shouldRestoreRealUserEnvelope &&
    syntheticAttachmentEntries.length === 0 &&
    promptAttachmentEntries.length === 0
  ) {
    return [];
  }
  return [
    ...(shouldRestoreRealUserEnvelope
      ? [
          {
            message: { role: "user" as const, content },
            metadata: userMetadata,
          },
        ]
      : []),
    ...syntheticAttachmentEntries,
    ...promptAttachmentEntries,
  ];
}

function contentFromUserBlocks(
  blocks: readonly ModelMessageContentBlock[],
  options: { preserveBlocks?: boolean } = {},
): ModelMessageContent {
  if (options.preserveBlocks) {
    return blocks.map((block) => ({ ...block })) as ModelMessageContentBlock[];
  }
  const textBlocks = blocks.filter(
    (block): block is Extract<ModelMessageContentBlock, { type: "text" }> => block.type === "text",
  );
  if (blocks.length === textBlocks.length) {
    return textBlocks
      .map((block) => block.text)
      .filter(Boolean)
      .join("\n\n");
  }
  return blocks.map((block) => ({ ...block })) as ModelMessageContentBlock[];
}

function promptAttachmentReminderInputForFilePart(
  part: FilePart,
  block: Extract<ModelMessageContentBlock, { type: "text" }>,
): (PromptAttachmentReminderInput & { content: string; kind: "file" | "inline_text" }) | undefined {
  if (!part.mime.startsWith("text/")) return undefined;
  if (!part.source) {
    return {
      content: block.text,
      kind: "inline_text",
      label: part.filename,
      preview: part.metadata?.preview,
    };
  }
  if (part.metadata?.storageKind !== "inline") return undefined;
  if (
    part.metadata.recoverability !== "provider_ready" &&
    part.metadata.recoverability !== "preview_only"
  ) {
    return undefined;
  }
  if (part.metadata.preview?.text !== block.text) return undefined;
  return {
    content: block.text,
    kind: "file",
    label: part.source.text.value ?? part.filename,
    preview: part.metadata.preview,
  };
}

function textPartToProviderText(part: Extract<MessagePart, { type: "text" }>): string {
  if (!part.synthetic) {
    return part.text;
  }
  if (isProviderWrappedSystemReminderText(part.text)) {
    return part.text;
  }

  const runtimeMetadata = runtimeMessageMetadataFromPartMetadata(part.metadata);
  if (runtimeMetadata?.source === "task_status" && part.metadata?.source !== "background_task") {
    return wrapSystemReminderForSource("task_status", part.text);
  }
  if (
    runtimeMetadata?.source === "queued_system_notification" ||
    part.metadata?.source === "subagent"
  ) {
// Subagent notification also only wraps the outer layer when the provider history is restored to avoid changing the session/UI raw transcript.
    return wrapSystemReminderForSource("queued_system_notification", part.text);
  }
  return part.text;
}

interface SyntheticSystemReminderAttachment {
  source: SystemReminderSource;
  content: string;
}

function syntheticSystemReminderAttachmentFromParts(
  parts: MessagePart[],
): SyntheticSystemReminderAttachment | undefined {
  const visibleParts = parts.filter((part) => !(part.type === "text" && part.ignored));
  if (visibleParts.length !== 1) return undefined;

  const part = visibleParts[0]!;
  if (part.type !== "text" || !part.synthetic) return undefined;
  return syntheticSystemReminderAttachmentFromTextPart(part);
}

function syntheticSystemReminderAttachmentFromTextPart(
  part: Extract<MessagePart, { type: "text" }>,
): SyntheticSystemReminderAttachment | undefined {
  if (!part.synthetic) return undefined;
  if (part.text.trim().length === 0) return undefined;
  if (isProviderWrappedSystemReminderText(part.text)) return undefined;
  const metadata = metadataFromSyntheticTextPart(part);
  if (!isRestorableSystemReminderAttachmentSource(metadata.source)) return undefined;

  return {
    source: metadata.source,
    content: part.text,
  };
}

function isRestorableSystemReminderAttachmentSource(
  source: RuntimeMessageSource,
): source is SystemReminderSource {
  if (!isKnownSystemReminderSource(source)) return false;
  const descriptor = getSystemReminderDescriptor(source);
  return (
    descriptor.isMeta &&
    descriptor.providerVisibility === "provider_visible" &&
    descriptor.channel !== "real_user" &&
    descriptor.channel !== "tool_result"
  );
}

function isProviderWrappedSystemReminderText(text: string): boolean {
  return text.trimStart().startsWith("<system-reminder");
}

function metadataFromUserParts(parts: MessagePart[]): RuntimeMessageMetadata {
  const visibleTextParts = parts.filter(
    (part): part is Extract<MessagePart, { type: "text" }> => part.type === "text" && !part.ignored,
  );
  const hasRealUserText = visibleTextParts.some((part) => !part.synthetic);
  const hasStructuredUserPart = parts.some((part) => part.type === "file" || part.type === "agent");
  if (hasRealUserText || hasStructuredUserPart) {
    return realUserRuntimeMetadata();
  }

  const syntheticTextPart = visibleTextParts.find((part) => part.synthetic);
  if (!syntheticTextPart) {
    return realUserRuntimeMetadata();
  }

  return metadataFromSyntheticTextPart(syntheticTextPart);
}

function metadataFromSyntheticTextPart(
  part: Extract<MessagePart, { type: "text" }>,
): RuntimeMessageMetadata {
  const source = part.metadata?.source;
  if (source === "background_task" || source === "subagent_message") {
// The runtime command carrier is aligned for direct user-like injection; even if it is used in the old persistence
// system reminder metadata, background completion or child replies cannot be repackaged into reminders during recovery.
    return legacySyntheticRuntimeMetadata();
  }

  const persistedRuntimeMetadata = runtimeMessageMetadataFromPartMetadata(part.metadata);
  if (persistedRuntimeMetadata) {
    return persistedRuntimeMetadata;
  }

  if (source === "subagent") {
    return systemReminderRuntimeMetadata("queued_system_notification");
  }
  if (source === "todo_reminder") {
    return todoReminderRuntimeMetadata();
  }
  if (source === "goal-continuation") {
    return systemReminderRuntimeMetadata("target_continuation");
  }
  if (source === "rewind" || source === "fork") {
    return systemReminderRuntimeMetadata("rewind_notice");
  }
  if (isKnownSystemReminderSource(source)) {
    return systemReminderRuntimeMetadata(source);
  }

  return legacySyntheticRuntimeMetadata();
}

function runtimeMessageMetadataFromPartMetadata(
  metadata: Record<string, unknown> | undefined,
): RuntimeMessageMetadata | undefined {
  const runtimeMessage = metadata?.runtimeMessage;
  if (!isRecord(runtimeMessage)) return undefined;

  const presentation = runtimeInputMetadata(runtimeMessage.inputPresentation);
  if (presentation) return presentation;
  const source = runtimeMessage.source;
  if (source === "real_user") {
    return realUserRuntimeMetadata();
  }
  if (source === "legacy_synthetic") {
    return legacySyntheticRuntimeMetadata();
  }
  if (source === "todo_reminder") {
    return todoReminderRuntimeMetadata();
  }
  if (isKnownSystemReminderSource(source)) {
    return systemReminderRuntimeMetadata(source);
  }

  return undefined;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function assistantTextFromParts(parts: MessagePart[]): string {
  const chunks: string[] = [];

  for (const part of parts) {
    if (part.type === "text" && !part.ignored) {
      chunks.push(part.text);
    }
  }

  return chunks.join("\n\n");
}

function assistantReasoningFromParts(parts: MessagePart[]): ModelReasoningContentBlock[] {
  const blocks: ModelReasoningContentBlock[] = [];

  for (const part of parts) {
    if (part.type === "reasoning") {
      blocks.push({
        type: "reasoning",
        text: part.text,
        providerOptions: part.metadata ? { ...part.metadata } : undefined,
      });
    }
  }

  return blocks;
}

function isToolPart(part: MessagePart): part is ToolPart {
  return part.type === "tool";
}

function providerToolNameFromPart(part: ToolPart): string {
  const providerToolName = part.metadata?.providerToolName;
// Empty strings in cold hydration could only rely on non-empty ToolPart.tool;
// The original value must be restored based on the existence of the field. Truthy judgment cannot be used to re-overwrite the empty name into a placeholder value.
  return providerToolName !== undefined && typeof providerToolName === "string"
    ? providerToolName
    : part.tool;
}
