/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
// Transcript → SessionEvent composition ("reduce(transcript) ≡ reduce(events)").
//
// Motivation: v4 projection is event sourcing, but some historical mutations (pure dialogue fork copies message but does not copy event,
// rewind truncates only the message library) so that the session's event log cannot cover the visible transcript. cold subscription
// Hydration cannot get these histories ("fork-child history") from event log reconstruction.
//
// This module reversely synthesizes the transcript of the message library into a SessionEvent sequence that can be consumed by the reducer——
// This reuses the entire set of ProductProjection reduction logic and eliminates the need to write a message→row parallel reducer.
// Synthetic events are used for "view reconstruction": only the smallest sequence that is "reduction equivalent" to the real event stream is produced.
// v4 cold recovery can only replay events recognized by ProductProjection; if the events in transcript
// tool/reasoning/subagent/compact part does not reverse synthesis, and the historical visible running state will disappear from the snapshot after restarting.
import type {
  AssistantErrorInfo,
  BackgroundResultOriginMeta,
  CompactTimelineStatus as CompactTimelineStatusValue,
  MessagePart,
  MessageWithParts,
  ModelSelection,
  TurnFileChangeSummary,
  TurnInputIntentMetadata,
} from "@zcode/contracts";
import type { EventId, SessionEvent, SessionId, TraceId, TurnId } from "@zcode/contracts";
import {
  CompactTimelineStatus,
  CompactTrigger,
  CoreErrorType,
  createSessionId,
  ModelErrorCode,
  parseCompletedToolPartMetadata,
  SessionEventType,
  STREAM_RECOVERY_DISCARDED_ERROR_NAME,
} from "@zcode/contracts";
import {
  getConversationModelOnlyTurnTriggerSource,
  getConversationMessageProjectionPolicy,
  isConversationRealUserTurnStarter,
} from "@zcode/shared";
import {
  conversationInputIntentSchema,
  errorAttributionSchema,
  workflowLaunchMetaSchema,
  workflowNotificationMetaSchema,
  type ErrorAttribution,
  type WorkflowLaunchMeta,
} from "@zcode/shared/zcode-protocol-v4";
import { shouldHideInvalidToolCallFromProduct } from "../tool-call-product-visibility.js";
import { HYDRATION_TRACE_ID } from "./projection-state.js";

const SUBAGENT_TOOL_NAMES = new Set(["Agent", "Task", "subagent"]);
const LEGACY_MODEL_REQUEST_CANCELLED_MESSAGE = "Model request was cancelled.";
const LEGACY_PROTOCOL_SESSION_STOPPED_MESSAGE = "ZCode Protocol session stopped";
const PERSISTED_CANCELLATION_CODES = new Set<string>([
  CoreErrorType.TurnCancelled,
  ModelErrorCode.ModelRequestCancelled,
  "MODEL_REQUEST_CANCELLED",
  "ABORT_ERR",
]);

type PushEvent = (
  type: SessionEventType,
  payload: unknown,
  turnId?: string,
  sourceTimestampMs?: number,
) => void;

type TurnResultForHydration = "success" | "cancelled" | "error_during_execution";

interface AssistantSynthesisState {
  toolCallCount: number;
  resultType: TurnResultForHydration;
}

interface ParsedSubagentOutput {
  agentId?: string;
  agentType?: string;
  childSessionId?: string;
  description?: string;
  parentToolCallId?: string;
  prompt?: string;
  summaryText?: string;
}

interface SynthesizeOptions {
  sessionId: string;
  /**
* The authoritative context window of the actual selected model in the current session.
* Transcript only persists token usage and does not persist model capabilities. It must be injected by the current workspace registry.
   */
  contextWindow?: number;
/** Synthetic base timestamp (deterministic: no need for Date.now, the caller passes in the first message time to get the bottom line). */
  baseTimestampMs?: number;
  /**
* goal verify fact for session_entry legacy source:
* If there is an anchor, press the anchorAssistantMessageId and drop it after the corresponding assistant.
* If there is no anchor/anchor mismatch, it will fall to the end of the known timeline; press the key to remove duplicates from the timeline part.
   */
  goalVerificationEntries?: readonly HydratedGoalVerificationEntry[];
  /**
* The workspace checkpoint artifact reconstructs a single round summary based on the real user messageId.
* transcript does not have this field and must be explicitly injected into the synthetic ModelComplete to maintain live/cold equivalence.
   */
  fileChangeSummariesByMessageId?: ReadonlyMap<string, TurnFileChangeSummary>;
}

function isRealUserTurnStarter(message: MessageWithParts): boolean {
  return isConversationRealUserTurnStarter(message);
}

/**
* The hub directly starts the launch wheel message of the workflow. Core persistence writes
* `source: "workflow_launch"` + `metadata.workflowLaunch` (the authoritative source for cold recovery). it is synthetic
* But for visible messages that semantically belong to the user's real actions, the shared projection policy will classify the synthetic user as hiddenSynthetic.
* Therefore `isConversationRealUserTurnStarter` does not recognize it; the cold path is explicitly rebuilt before the real-user branch according to this criterion
* controlOnly startup wheel (TurnStarted{inputSource, workflowLaunch, executionKind} +
* TurnComplete) instead of being skipped as a hidden synthetic. Return null for malformed/absent metadata (return to existing skip semantics).
 */
function workflowLaunchOfMessage(message: MessageWithParts): WorkflowLaunchMeta | null {
  if (message.info.role !== "user") return null;
  if (message.info.source !== "workflow_launch") return null;
  const metadata = message.info.metadata;
  if (!metadata || typeof metadata !== "object" || Array.isArray(metadata)) return null;
  const parsed = workflowLaunchMetaSchema.safeParse(
    (metadata as Record<string, unknown>).workflowLaunch,
  );
  return parsed.success ? parsed.data : null;
}

function isProviderContextOnlyAssistant(message: MessageWithParts): boolean {
  return (
    message.info.role === "assistant" &&
    getConversationMessageProjectionPolicy(message) === "providerContextOnly"
  );
}

function textOfMessage(parts: readonly MessagePart[]): string {
  return parts
    .filter(
      (part): part is Extract<MessagePart, { type: "text" }> =>
        part.type === "text" && part.ignored !== true,
    )
    .map((part) => part.text)
    .join("");
}

function finiteTimeMs(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function messageCreatedAtMs(message: MessageWithParts): number | undefined {
  return finiteTimeMs(message.info.time.created);
}

function intervalEndOrStartMs(time: { start: number } & Partial<{ end: number }>) {
  return finiteTimeMs(time.end) ?? finiteTimeMs(time.start);
}

function partEndAtMs(part: MessagePart): number | undefined {
  if (part.type === "reasoning") {
    return finiteTimeMs(part.time?.end) ?? finiteTimeMs(part.time?.start);
  }
  if (part.type === "tool" && "time" in part.state) {
    return intervalEndOrStartMs(part.state.time);
  }
  return undefined;
}

function messageEndAtMs(message: MessageWithParts): number | undefined {
  const time = message.info.time;
// Cold recovery will process user/assistant messages at the same time; user only has created,
// Only assistant may have completed, so the end time must be obtained after narrowing the field existence.
  let end =
    ("completed" in time ? finiteTimeMs(time.completed) : undefined) ?? finiteTimeMs(time.created);
  for (const part of message.parts) {
    const partEnd = partEndAtMs(part);
    if (partEnd !== undefined) {
      end = end === undefined ? partEnd : Math.max(end, partEnd);
    }
  }
  return end;
}

function normalizeTurnResult(
  current: TurnResultForHydration,
  next: TurnResultForHydration,
): TurnResultForHydration {
  if (current === "cancelled" || next === "cancelled") {
    return "cancelled";
  }
  if (current === "error_during_execution" || next === "error_during_execution") {
    return "error_during_execution";
  }
  return "success";
}

function assistantErrorData(error: AssistantErrorInfo): Record<string, unknown> | undefined {
  return error.data && typeof error.data === "object" && !Array.isArray(error.data)
    ? error.data
    : undefined;
}

function persistedErrorAttribution(
  data: Record<string, unknown> | undefined,
): ErrorAttribution | undefined {
  const parsed = errorAttributionSchema.safeParse(data?.attribution);
  return parsed.success ? parsed.data : undefined;
}

function isPersistedAssistantCancellation(error: AssistantErrorInfo): boolean {
  const data = assistantErrorData(error);
  const code = typeof data?.code === "string" ? data.code : undefined;
  if (
    data?.turnResult === "cancelled" ||
    data?.resultType === "cancelled" ||
    (code !== undefined && PERSISTED_CANCELLATION_CODES.has(code)) ||
    error.name === "AbortError"
  ) {
    return true;
  }

  if (
    code === undefined &&
    error.name === "Error" &&
    data?.message === LEGACY_PROTOCOL_SESSION_STOPPED_MESSAGE
  ) {
// The old session/stop uses ordinary Error as AbortSignal.reason, and the transcript is not persisted.
// canceled result; if cold recovery only recognizes AbortError, the user stop will be re-synthesized into TurnError and error Banner.
    return true;
  }

// The AiSdkModelAdapterError of the old transcript does not have a persistent model error code,
// Can only be restored compatible with the standard name/message tuples generated by ZCode itself; does not generalize the matching provider copy.
  return (
    code === undefined &&
    error.name === "AiSdkModelAdapterError" &&
    data?.message === LEGACY_MODEL_REQUEST_CANCELLED_MESSAGE
  );
}

/**
* Stream recovery persists the invalid half assistant into a message with error, and then retrieves it from the anchor point
* Resends and completes normally; live projection just wraps it up as an interrupted line, without raising a TurnError. The old cold is restored but the
* Any assistant with an error will be treated as a failure in this round. After restarting the session, "Partial assistant output" will pop up out of thin air.
* was discarded" error banner. This flag only serves compression/fork boundary isolation and must be transparent to the results of this round.
 */
function isPersistedStreamRecoveryDiscard(error: AssistantErrorInfo): boolean {
  return error.name === STREAM_RECOVERY_DISCARDED_ERROR_NAME;
}

function stableToolSchedule(toolCallId: string) {
  return {
    executionOrder: [toolCallId],
    parallelGroups: [[toolCallId]],
  };
}

function compactEventType(status: CompactTimelineStatusValue): SessionEventType {
  if (status === CompactTimelineStatus.Started || status === CompactTimelineStatus.Retrying) {
    return SessionEventType.CompactStarted;
  }
  if (status === CompactTimelineStatus.Completed || status === CompactTimelineStatus.Skipped) {
    return SessionEventType.CompactCompleted;
  }
  return SessionEventType.CompactFailed;
}

function normalizeCompactTimelineStatus(
  status: string | undefined,
): CompactTimelineStatusValue | null {
  switch (status) {
    case CompactTimelineStatus.Started:
    case CompactTimelineStatus.Retrying:
    case CompactTimelineStatus.Skipped:
    case CompactTimelineStatus.Completed:
    case CompactTimelineStatus.Failed:
    case CompactTimelineStatus.Interrupted:
      return status;
    case "cancelled":
      return CompactTimelineStatus.Interrupted;
    default:
      return null;
  }
}

function compactTriggerOfPart(part: Extract<MessagePart, { type: "compaction" }>) {
  return part.trigger ?? (part.auto ? CompactTrigger.Auto : CompactTrigger.Manual);
}

function compactPayloadFromTimelinePart(part: Extract<MessagePart, { type: "timeline" }>) {
  if (part.timelineType !== "context_compaction") return null;
  const status = normalizeCompactTimelineStatus(part.status);
  if (!status) return null;
  return {
    status,
    payload: {
      operationId: part.operationId,
      messageId: String(part.messageID),
      partId: part.id,
      status,
      trigger: part.trigger,
      display: part.display,
      ...(part.sourceCommandId ? { sourceCommandId: part.sourceCommandId } : {}),
      ...(part.anchorMessageId ? { anchorMessageId: part.anchorMessageId } : {}),
      ...(part.anchorTurnId ? { anchorTurnId: part.anchorTurnId } : {}),
      ...(part.phase ? { phase: part.phase } : {}),
      ...(part.compactReason ? { compactReason: part.compactReason } : {}),
      ...(part.reason ? { reason: part.reason } : {}),
      ...(part.boundaryId ? { boundaryId: part.boundaryId } : {}),
      ...(part.summaryMessageId ? { summaryMessageId: part.summaryMessageId } : {}),
      ...(part.preCompactTokenCount !== undefined
        ? { preCompactTokenCount: part.preCompactTokenCount }
        : {}),
      ...(part.postCompactTokenCount !== undefined
        ? { postCompactTokenCount: part.postCompactTokenCount }
        : {}),
      ...(part.truePostCompactTokenCount !== undefined
        ? { truePostCompactTokenCount: part.truePostCompactTokenCount }
        : {}),
      ...(part.attempt !== undefined ? { attempt: part.attempt } : {}),
      ...(part.maxAttempts !== undefined ? { maxAttempts: part.maxAttempts } : {}),
      ...(part.time?.start !== undefined ? { startedAt: part.time.start } : {}),
      ...(part.time?.end !== undefined ? { endedAt: part.time.end } : {}),
    },
  };
}

function compactPayloadFromLegacyCompactionPart(
  part: Extract<MessagePart, { type: "compaction" }>,
) {
  const status = normalizeCompactTimelineStatus(part.timelineStatus);
  if (!status) return null;
  return {
    status,
    payload: {
      operationId: part.operationId ?? part.boundaryId ?? `legacy-compact-${String(part.id)}`,
      messageId: String(part.messageID),
      partId: part.id,
      status,
      trigger: compactTriggerOfPart(part),
      display: part.timelineDisplay ?? "separator",
      ...(part.phase ? { phase: part.phase } : {}),
      ...(part.compactReason ? { compactReason: part.compactReason } : {}),
      ...(part.reason ? { reason: part.reason } : {}),
      ...(part.boundaryId ? { boundaryId: part.boundaryId } : {}),
      ...(part.summaryMessageId ? { summaryMessageId: part.summaryMessageId } : {}),
      ...(part.tail_start_id ? { tailStartMessageId: part.tail_start_id } : {}),
      ...(part.preCompactTokenCount !== undefined
        ? { preCompactTokenCount: part.preCompactTokenCount }
        : {}),
      ...(part.postCompactTokenCount !== undefined
        ? { postCompactTokenCount: part.postCompactTokenCount }
        : {}),
      ...(part.truePostCompactTokenCount !== undefined
        ? { truePostCompactTokenCount: part.truePostCompactTokenCount }
        : {}),
      ...(part.attempt !== undefined ? { attempt: part.attempt } : {}),
      ...(part.maxAttempts !== undefined ? { maxAttempts: part.maxAttempts } : {}),
      ...(part.time?.start !== undefined ? { startedAt: part.time.start } : {}),
      ...(part.time?.end !== undefined ? { endedAt: part.time.end } : {}),
    },
  };
}

function parseJsonObject(input: string | undefined): Record<string, unknown> | null {
  if (!input) return null;
  try {
    const parsed = JSON.parse(input) as unknown;
    return parsed && typeof parsed === "object" && !Array.isArray(parsed)
      ? (parsed as Record<string, unknown>)
      : null;
  } catch {
    return null;
  }
}

function stringField(
  source: Record<string, unknown> | undefined | null,
  key: string,
): string | undefined {
  const value = source?.[key];
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function inputIntentOfMessage(message: MessageWithParts): TurnInputIntentMetadata | undefined {
  const fullIntent = conversationInputIntentSchema.safeParse(
    message.info.metadata?.conversationInputIntent,
  );
  if (fullIntent.success) {
    const value = fullIntent.data;
    return {
      sourceCommandId: value.sourceCommandId,
      queueItemId: value.queueItemId,
      clientId: value.clientId,
      kind: value.kind,
// It can be seen that text is to display facts; the canonical objective of goal can only read persistent intent.text,
// It is forbidden to do case/keyword parsing from `/goal replace ...` copywriting.
      text: value.text,
      ...(value.modelSelection ? { modelSelection: value.modelSelection } : {}),
      ...(value.mode ? { mode: value.mode } : {}),
      ...(value.planEnabled !== undefined ? { planEnabled: value.planEnabled } : {}),
      admissionSeq: value.order.admissionSeq,
      admittedAt: value.admittedAt,
      requestedDelivery: value.delivery.requested,
      admittedDelivery: value.delivery.admitted,
      ...(value.order.queuePosition !== undefined
        ? { queuePosition: value.order.queuePosition }
        : {}),
      ...(value.delivery.fallbackReasonCode
        ? { fallbackReasonCode: value.delivery.fallbackReasonCode }
        : {}),
      ...(value.attachments.length > 0 ? { attachmentRefs: value.attachments } : {}),
      ...(value.provenance ? { provenance: value.provenance } : {}),
    };
  }

// Compatible with transcripts that only persisted metadata seed before; new writes will always use the above complete facts.
  const value = message.info.metadata?.inputIntent;
  if (!value || typeof value !== "object" || Array.isArray(value)) return undefined;
  const intent = value as Record<string, unknown>;
  if (
    typeof intent.sourceCommandId !== "string" ||
    typeof intent.queueItemId !== "string" ||
    typeof intent.clientId !== "string" ||
    (intent.kind !== "sendText" && intent.kind !== "sendGoalCommand") ||
    typeof intent.admissionSeq !== "number" ||
    typeof intent.admittedAt !== "number" ||
    (intent.requestedDelivery !== "auto" &&
      intent.requestedDelivery !== "startNow" &&
      intent.requestedDelivery !== "queue" &&
      intent.requestedDelivery !== "guide") ||
    (intent.admittedDelivery !== "startNow" &&
      intent.admittedDelivery !== "queue" &&
      intent.admittedDelivery !== "guide")
  ) {
    return undefined;
  }
  return value as TurnInputIntentMetadata;
}

function executionKindOfMessage(message: MessageWithParts): "agent" | "controlOnly" | undefined {
  const value = message.info.metadata?.executionKind;
  return value === "agent" || value === "controlOnly" ? value : undefined;
}

/**
* Starting point for engine attached text: hot path it's on TurnStarted,
* The cold path reads back the same field from the user message metadata. Only non-negative integers are recognized - other shapes are treated as absent (prefer to show more).
 */
function epilogueStartOfMessage(message: MessageWithParts): number | undefined {
  const value = message.info.metadata?.epilogueStart;
  return typeof value === "number" && Number.isInteger(value) && value >= 0 ? value : undefined;
}

function contentBlocksToText(value: unknown): string | undefined {
  if (!Array.isArray(value)) return undefined;
  const chunks = value
    .map((block) => {
      if (!block || typeof block !== "object") return "";
      const text = (block as Record<string, unknown>).text;
      return typeof text === "string" ? text : "";
    })
    .filter((text) => text.length > 0);
  return chunks.length > 0 ? chunks.join("\n\n") : undefined;
}

function subagentInfoFromToolPart(
  part: Extract<MessagePart, { type: "tool" }>,
): ParsedSubagentOutput | null {
  if (!SUBAGENT_TOOL_NAMES.has(part.tool)) return null;
  const input =
    part.state.input && typeof part.state.input === "object"
      ? (part.state.input as Record<string, unknown>)
      : {};
  const output = part.state.status === "completed" ? parseJsonObject(part.state.output) : null;
  const metadata = part.metadata && typeof part.metadata === "object" ? part.metadata : {};
  const explicitAgentId =
    stringField(output, "agentId") ??
    stringField(metadata, "agentId") ??
    agentIdFromToolOutput(part.state.status === "completed" ? part.state.output : undefined);
  const agentId = explicitAgentId ?? part.callID;
  return {
    agentId,
    agentType:
      stringField(output, "agentType") ??
      stringField(metadata, "agentType") ??
      stringField(input, "agent") ??
      stringField(input, "agentType") ??
      "subagent",
    childSessionId:
      stringField(output, "childSessionId") ??
      stringField(metadata, "childSessionId") ??
// The persistence tool output of the background Agent is human-readable text instead of JSON; cold merge
// Will suppress duplicate durable spawned. If the child session is not restored from the stable agentId row, the sidebar entry will be lost.
      (explicitAgentId ? createSessionId(`subagent_${agentId}`) : undefined),
    description:
      stringField(output, "description") ??
      stringField(input, "description") ??
      stringField(metadata, "description"),
    parentToolCallId: part.callID,
    prompt: stringField(output, "prompt") ?? stringField(input, "prompt"),
    summaryText:
      contentBlocksToText(output?.content) ??
      stringField(output, "result") ??
      stringField(output, "summary") ??
      stringField(input, "description") ??
      stringField(input, "prompt"),
  };
}

function agentIdFromToolOutput(value: unknown): string | undefined {
  if (typeof value !== "string") return undefined;
  return /(?:^|\r?\n)agentId:\s*([^\s(]+)/u.exec(value)?.[1];
}

function subagentStatusFromToolPart(
  part: Extract<MessagePart, { type: "tool" }>,
): "completed" | "failed" | "cancelled" {
  switch (part.state.status) {
    case "completed":
      return "completed";
    case "error":
      return "failed";
    default:
      return "cancelled";
  }
}

/**
* Attachment rendering: FilePart → TurnStarted attachment display meta information (TurnAttachmentMeta).
* The historical attachments of cold subscription/fork-child are reversely synthesized by transcript - the same projection entrance as the live event
* (buildUserInputRow), ensure that the contents of cold/hot path rows are consistent.
 */
function attachmentMetasOfMessage(
  parts: readonly MessagePart[],
): Array<{ fileName: string; mime: string; bytes: number; ref?: string }> {
  const fileParts = parts.filter(
    (part): part is Extract<MessagePart, { type: "file" }> => part.type === "file",
  );
  return fileParts.map((part, index) => {
    const urlIsStableRef = part.url.length > 0 && !part.url.startsWith("data:");
    const basenameFromUrl = urlIsStableRef ? (part.url.split(/[\\/]/).pop() ?? "") : "";
    return {
      fileName: part.filename ?? (basenameFromUrl || `attachment-${index + 1}`),
      mime: part.mime,
      bytes: part.metadata?.sizeBytes ?? 0,
      ...(urlIsStableRef ? { ref: part.url } : {}),
    };
  });
}

function forkContextOfMessage(message: MessageWithParts):
  | {
      parentSessionId: string;
      restoredFileCount?: number;
      targetCheckpointId?: string;
      targetMessageId?: string;
    }
  | undefined {
  for (const part of message.parts) {
    if (part.type === "timeline" && part.timelineType === "session_fork") {
      return {
        parentSessionId: String(part.parentSessionId),
        ...(typeof part.restoredFileCount === "number"
          ? { restoredFileCount: part.restoredFileCount }
          : {}),
        ...(part.targetCheckpointId ? { targetCheckpointId: part.targetCheckpointId } : {}),
        ...(part.targetMessageId ? { targetMessageId: String(part.targetMessageId) } : {}),
      };
    }
    const metadata = part.type === "text" ? part.metadata : undefined;
    const context = forkContextFromMetadata(metadata);
    if (context) return context;
  }
  return message.info.role === "user" ? forkContextFromMetadata(message.info.metadata) : undefined;
}

function forkContextFromMetadata(metadata: Record<string, unknown> | undefined):
  | {
      parentSessionId: string;
      restoredFileCount?: number;
      targetCheckpointId?: string;
      targetMessageId?: string;
    }
  | undefined {
  const forkContext = metadata?.forkContext;
  if (typeof forkContext !== "object" || forkContext === null || Array.isArray(forkContext)) {
    return undefined;
  }
  const context = forkContext as Record<string, unknown>;
  if (context.kind !== "session_fork" || typeof context.parentSessionId !== "string") {
    return undefined;
  }
  return {
    parentSessionId: context.parentSessionId,
    ...(typeof context.restoredFileCount === "number"
      ? { restoredFileCount: context.restoredFileCount }
      : {}),
    ...(typeof context.targetCheckpointId === "string"
      ? { targetCheckpointId: context.targetCheckpointId }
      : {}),
    ...(typeof context.targetMessageId === "string"
      ? { targetMessageId: context.targetMessageId }
      : {}),
  };
}

function isForkTimelineMessage(message: MessageWithParts): boolean {
  return (
    getConversationMessageProjectionPolicy(message) === "timelineOnly" &&
    forkContextOfMessage(message) !== undefined
  );
}

function synthesizeTextPart(
  part: Extract<MessagePart, { type: "text" }>,
  assistantMessageId: string,
  assistantMessageCreatedAtMs: number | undefined,
  push: PushEvent,
  turnId: string,
): void {
  if (part.ignored === true || part.text.length === 0) return;
  push(
    SessionEventType.ModelStreaming,
    {
      kind: "text_start",
      delta: "",
      done: false,
      assistantMessageId,
      partId: part.id,
    },
    turnId,
// Cold synthetic events cannot use "first message time + seq": after refreshing
// The assistant action bar will display different historical replies close to the same time. text row is required when creating
// Retain the true creation time of the corresponding transcript assistant message; the order of events is still determined by seq.
    assistantMessageCreatedAtMs,
  );
  push(
    SessionEventType.ModelStreaming,
    {
      kind: "text_delta",
      delta: part.text,
      done: false,
      assistantMessageId,
      partId: part.id,
    },
    turnId,
  );
  push(
    SessionEventType.ModelStreaming,
    { kind: "text_end", delta: "", done: false, partId: part.id },
    turnId,
  );
}

function synthesizeReasoningPart(
  part: Extract<MessagePart, { type: "reasoning" }>,
  assistantMessageId: string,
  push: PushEvent,
  turnId: string,
): void {
  if (part.text.length === 0) return;
  push(
    SessionEventType.ModelStreaming,
    {
      kind: "reasoning_start",
      delta: "",
      done: false,
      assistantMessageId,
      partId: part.id,
    },
    turnId,
  );
  push(
    SessionEventType.ModelStreaming,
    {
      kind: "reasoning_delta",
      delta: part.text,
      done: false,
      partId: part.id,
    },
    turnId,
  );
  push(
    SessionEventType.ModelStreaming,
    { kind: "reasoning_end", delta: "", done: false, partId: part.id },
    turnId,
  );
}

function synthesizeSubagentLifecycle(
  info: ParsedSubagentOutput,
  status: "completed" | "failed" | "cancelled",
  push: PushEvent,
  turnId: string,
): void {
  const agentId = info.agentId ?? `subagent-${turnId}`;
  push(
    SessionEventType.SubagentSpawned,
    {
      agentId,
      agentType: info.agentType ?? "subagent",
      childSessionId: info.childSessionId,
      description: info.description ?? info.summaryText ?? info.prompt ?? agentId,
      parentToolCallId: info.parentToolCallId,
      prompt: info.prompt,
      status: "running",
    },
    turnId,
  );
  push(
    SessionEventType.SubagentStopped,
    {
      agentId,
      agentType: info.agentType ?? "subagent",
      childSessionId: info.childSessionId,
      description: info.description,
      parentToolCallId: info.parentToolCallId,
      prompt: info.prompt,
      summaryText: info.summaryText,
      status,
    },
    turnId,
  );
}

function synthesizeToolPart(
  part: Extract<MessagePart, { type: "tool" }>,
  assistantMessageId: string,
  push: PushEvent,
  turnId: string,
): AssistantSynthesisState {
  if (shouldHideInvalidToolCallFromProduct(part.tool, part.metadata)) {
// Footprint filtering only determines whether the event needs to be supplemented, and cannot prevent the actual synthesis; this must be at the source of the event
// Skip recovery parts with original empty metadata to avoid cold hydration rematerialization of tool lines.
    return { resultType: "success", toolCallCount: 0 };
  }
  const toolCallId = part.callID;
  const persistedMetadata = parseCompletedToolPartMetadata(
    "metadata" in part.state ? part.state.metadata : part.metadata,
  );
  push(
    SessionEventType.ToolCallScheduled,
    {
      toolCallId,
      assistantMessageId,
      toolName: part.tool,
      input: part.state.input,
      ...(persistedMetadata?.display ? { display: persistedMetadata.display } : {}),
      schedule: stableToolSchedule(toolCallId),
    },
    turnId,
  );

  const started =
    part.state.status === "running" ||
    part.state.status === "completed" ||
    part.state.status === "error";
  if (started) {
    push(
      SessionEventType.ToolCallStarted,
      {
        toolCallId,
        toolName: part.tool,
        ...(persistedMetadata?.display ? { display: persistedMetadata.display } : {}),
        startedAt: new Date(
          "time" in part.state && typeof part.state.time.start === "number"
            ? part.state.time.start
            : 0,
        ),
      },
      turnId,
    );
  }

  const subagentInfo = subagentInfoFromToolPart(part);
  if (subagentInfo && started) {
    synthesizeSubagentLifecycle(subagentInfo, subagentStatusFromToolPart(part), push, turnId);
  }

  if (part.state.status === "completed") {
    push(
      SessionEventType.ToolCallResult,
      {
        toolCallId,
        duration: Math.max(0, part.state.time.end - part.state.time.start),
        result: {
          success: true,
          content: part.state.output,
          ...(persistedMetadata?.display ? { display: persistedMetadata.display } : {}),
        },
      },
      turnId,
    );
    return { resultType: "success", toolCallCount: 1 };
  }

  if (part.state.status === "error") {
    push(
      SessionEventType.ToolCallResult,
      {
        toolCallId,
        duration: Math.max(0, part.state.time.end - part.state.time.start),
        result: {
          success: false,
          content: part.state.error,
          error: {
            type: "fault.runtime.toolFailed",
            message: part.state.error,
          },
        },
      },
      turnId,
    );
    return { resultType: "success", toolCallCount: 1 };
  }

// After the CLI is restarted, it cannot prove that the historical pending/running tool is still running and cannot be
// Active work / stop button is resurrected; let TurnComplete(cancelled) be unified into read-only history.
  return { resultType: "cancelled", toolCallCount: 1 };
}

function synthesizeCompactPart(
  part: MessagePart,
  emittedCompactOperations: Set<string>,
  durableCompactPartsByOperation: ReadonlyMap<string, Extract<MessagePart, { type: "compaction" }>>,
  push: PushEvent,
  turnId: string,
): boolean {
  let compact =
    part.type === "timeline"
      ? compactPayloadFromTimelinePart(part)
      : part.type === "compaction"
        ? compactPayloadFromLegacyCompactionPart(part)
        : null;
  if (!compact) return false;
  const operationId = String(compact.payload.operationId);
  const durablePart = durableCompactPartsByOperation.get(operationId);
  if (durablePart) {
// The timeline part of the same operation is usually ranked before the durable compaction part.
// The old "first come first served" will lose tail_start_id; the durable payload with coverage boundary is preferred.
    const durablePayload = compactPayloadFromLegacyCompactionPart(durablePart);
    compact = durablePayload ?? {
      ...compact,
      payload: {
        ...compact.payload,
        ...(durablePart.tail_start_id ? { tailStartMessageId: durablePart.tail_start_id } : {}),
        ...(durablePart.boundaryId ? { boundaryId: durablePart.boundaryId } : {}),
        ...(durablePart.summaryMessageId ? { summaryMessageId: durablePart.summaryMessageId } : {}),
      },
    };
  }
  if (emittedCompactOperations.has(operationId)) return true;
  emittedCompactOperations.add(operationId);
  push(compactEventType(compact.status), compact.payload, turnId);
  return true;
}

// ── goal verification timeline part──
//Persistence contract (core events.ts persistDurableSessionEvent): verifier every life cycle change
// Upsert the same timeline part, the identity is targetId_goalIteration, and the status is the final life cycle state.
//Reverse synthesis into started(+final state) event pair, reuse the existing goalVerify marker state machine of projection.
// The old hydration only recognizes context_compaction, and the goal_verification part is left unattended.
//Consumed branch - goalVerify marker disappears with every cold restore.
function goalVerificationKeyOfPart(part: Extract<MessagePart, { type: "timeline" }>): string {
  if (part.timelineType !== "goal_verification") return String(part.id);
  return part.goalIteration !== undefined
    ? `${part.targetId}_${part.goalIteration}`
    : part.verificationId;
}

// Cold recovery cannot prove that the historical verifier is still running (same as the precedent of pending tool closing as canceled):
// started/unknown state is closed as canceled; completed/failed_closed is restored as it is.
function goalVerificationTerminalStatus(
  status: string | undefined,
): "completed" | "failed_closed" | "cancelled" {
  switch (status) {
    case "completed":
      return "completed";
    case "failed":
    case "failed_closed":
      return "failed_closed";
    default:
      return "cancelled";
  }
}

/** The normalized form of goal verify facts: timeline part (new contract) and session_entry (legacy subject) are shared. */
interface GoalVerificationFact {
  key: string;
  targetId: string;
  verificationId: string;
  goalIteration?: number;
  anchorAssistantMessageId?: string;
  anchorTurnId?: string;
  status?: string;
  verification?: unknown;
}

function pushGoalVerificationFact(
  fact: GoalVerificationFact,
  emittedGoalVerifications: Set<string>,
  push: PushEvent,
  turnId: string | undefined,
): boolean {
  if (emittedGoalVerifications.has(fact.key)) return false;
  emittedGoalVerifications.add(fact.key);
  const base = {
    targetId: fact.targetId,
    verificationId: fact.verificationId,
    ...(fact.goalIteration !== undefined ? { goalIteration: fact.goalIteration } : {}),
    ...(fact.anchorAssistantMessageId
      ? { anchorAssistantMessageId: fact.anchorAssistantMessageId }
      : {}),
    ...(fact.anchorTurnId ? { anchorTurnId: fact.anchorTurnId } : {}),
  };
  push(SessionEventType.TargetCompletionVerification, { ...base, status: "started" }, turnId);
  push(
    SessionEventType.TargetCompletionVerification,
    {
      ...base,
      status: goalVerificationTerminalStatus(fact.status),
      ...(fact.verification ? { verification: fact.verification } : {}),
    },
    turnId,
  );
  return true;
}

function goalVerificationFactOfPart(
  part: Extract<MessagePart, { type: "timeline" }>,
): GoalVerificationFact | null {
  if (part.timelineType !== "goal_verification") return null;
  return {
    key: goalVerificationKeyOfPart(part),
    targetId: part.targetId,
    verificationId: part.verificationId,
    ...(part.goalIteration !== undefined ? { goalIteration: part.goalIteration } : {}),
    ...(part.anchorMessageId ? { anchorAssistantMessageId: String(part.anchorMessageId) } : {}),
    ...(part.anchorTurnId ? { anchorTurnId: String(part.anchorTurnId) } : {}),
    ...(part.status ? { status: part.status } : {}),
    ...(part.verification ? { verification: part.verification } : {}),
  };
}

function synthesizeGoalVerificationPart(
  part: MessagePart,
  emittedGoalVerifications: Set<string>,
  push: PushEvent,
  turnId: string,
): boolean {
  if (part.type !== "timeline") return false;
  const fact = goalVerificationFactOfPart(part);
  if (!fact) return false;
  pushGoalVerificationFact(fact, emittedGoalVerifications, push, turnId);
  return true;
}

//── session_entry legacy source──
// Historically, goal verify was mainly persisted in session_entry (native observation 1,402 lines vs timeline part
// Only 10 lines); entry.data retains the original event payload. Read side cross-origin by targetId_goalIteration
// Deduplication: timeline part and entry are only sent once when expressing the same fact (first come, first served, anchor semantics are consistent).
export interface HydratedGoalVerificationEntry {
  payload: {
    targetId: string;
    status?: string;
    verificationId: string;
    verification?: unknown;
    goalIteration?: number;
    anchorAssistantMessageId?: string;
    anchorTurnId?: string;
  };
  sequenceNumber?: number;
  timeCreated: number;
}

/** SessionEntryInfo (target_completion_verification) → normalized entry; illegal data is silently eliminated. */
export function goalVerificationEntriesFromSessionEntries(
  entries: readonly { data: unknown; time: { created: number } }[],
): HydratedGoalVerificationEntry[] {
  const parsed: HydratedGoalVerificationEntry[] = [];
  for (const entry of entries) {
    const data =
      entry.data && typeof entry.data === "object" && !Array.isArray(entry.data)
        ? (entry.data as Record<string, unknown>)
        : null;
    const payload =
      data?.payload && typeof data.payload === "object" && !Array.isArray(data.payload)
        ? (data.payload as Record<string, unknown>)
        : null;
    if (!payload) continue;
    const targetId = typeof payload.targetId === "string" ? payload.targetId : null;
    const verificationId =
      typeof payload.verificationId === "string" ? payload.verificationId : null;
    if (!targetId || !verificationId) continue;
    parsed.push({
      payload: {
        targetId,
        verificationId,
        ...(typeof payload.status === "string" ? { status: payload.status } : {}),
        ...(typeof payload.goalIteration === "number"
          ? { goalIteration: payload.goalIteration }
          : {}),
        ...(typeof payload.anchorAssistantMessageId === "string"
          ? { anchorAssistantMessageId: payload.anchorAssistantMessageId }
          : {}),
        ...(typeof payload.anchorTurnId === "string" ? { anchorTurnId: payload.anchorTurnId } : {}),
        ...(payload.verification !== undefined ? { verification: payload.verification } : {}),
      },
      ...(typeof data?.sequenceNumber === "number" ? { sequenceNumber: data.sequenceNumber } : {}),
      timeCreated: entry.time.created,
    });
  }
// Multiple entries for the same key (one entry each for started/terminal): get the latest final state in event order.
  parsed.sort(
    (left, right) =>
      (left.sequenceNumber ?? left.timeCreated) - (right.sequenceNumber ?? right.timeCreated),
  );
  return parsed;
}

function goalVerificationFactOfEntry(entry: HydratedGoalVerificationEntry): GoalVerificationFact {
  const payload = entry.payload;
  return {
    key:
      payload.goalIteration !== undefined
        ? `${payload.targetId}_${payload.goalIteration}`
        : payload.verificationId,
    targetId: payload.targetId,
    verificationId: payload.verificationId,
    ...(payload.goalIteration !== undefined ? { goalIteration: payload.goalIteration } : {}),
    ...(payload.anchorAssistantMessageId
      ? { anchorAssistantMessageId: payload.anchorAssistantMessageId }
      : {}),
    ...(payload.anchorTurnId ? { anchorTurnId: payload.anchorTurnId } : {}),
    ...(payload.status ? { status: payload.status } : {}),
    ...(payload.verification !== undefined ? { verification: payload.verification } : {}),
  };
}

/** Multiple entries of the same key (one for each life cycle) are merged into a single fact: the final state covers started. */
function mergeGoalVerificationEntryFacts(
  entries: readonly HydratedGoalVerificationEntry[],
): GoalVerificationFact[] {
  const byKey = new Map<string, GoalVerificationFact>();
  for (const entry of entries) {
    const fact = goalVerificationFactOfEntry(entry);
    const existing = byKey.get(fact.key);
    if (!existing) {
      byKey.set(fact.key, fact);
      continue;
    }
// Entries are sorted by event order: the later arriving life cycle state (final state) is overwritten, and the anchor takes the first value.
    byKey.set(fact.key, {
      ...existing,
      ...fact,
      anchorAssistantMessageId: existing.anchorAssistantMessageId ?? fact.anchorAssistantMessageId,
      anchorTurnId: existing.anchorTurnId ?? fact.anchorTurnId,
      verification: fact.verification ?? existing.verification,
    });
  }
  return [...byKey.values()];
}

//──Round selection facts──
// modelChange marker is generated by comparing lastTurnModel and config when the projection is in TurnStarted;
// There is no ModelSelected event in cold recovery. It is rebuilt based on the persistence selection facts of each round.
// Source priority: model snapshot of user prompt (always present, consistent with the config when submitted);
// The preface wheel (without user) gets the assistant message fact. Synthesize timeline host messages
// The model of (semantics.kind=timeline_event) is a host-compatible placeholder, not a fact of this round.
interface HydratedTimelineModel {
  modelSelection: ModelSelection;
  previousModelSelection?: ModelSelection | null;
}

function hydratedModelKey(modelSelection: ModelSelection): string {
  return `${modelSelection.providerId}\u0000${modelSelection.modelId}\u0000${modelSelection.options?.reasoningLevel ?? ""}`;
}

function turnModelSelectionOfUserMessage(message: MessageWithParts): ModelSelection | null {
  if (message.info.role !== "user") return null;
  return message.info.modelSelection ?? null;
}

function assistantModelSelectionOf(message: MessageWithParts): ModelSelection | null {
  if (message.info.role !== "assistant") return null;
  if (message.info.semantics?.kind === "timeline_event") return null;
  if (!message.info.providerId || !message.info.modelId) return null;
  return {
    providerId: String(message.info.providerId),
    modelId: String(message.info.modelId),
    ...(message.info.reasoningLevel
      ? { options: { reasoningLevel: message.info.reasoningLevel } }
      : {}),
  };
}

function modelChangeToModelOf(message: MessageWithParts): HydratedTimelineModel | null {
  for (let index = message.parts.length - 1; index >= 0; index -= 1) {
    const part = message.parts[index]!;
    if (part.type !== "timeline" || part.timelineType !== "model_change") continue;
    if (!part.toModel) return null;
    return {
      modelSelection: {
        providerId: part.toModel.providerId,
        modelId: part.toModel.modelId,
        ...(part.toModel.options ? { options: part.toModel.options } : {}),
      },
      previousModelSelection: part.fromModel
        ? {
            providerId: part.fromModel.providerId,
            modelId: part.fromModel.modelId,
            ...(part.fromModel.options ? { options: part.fromModel.options } : {}),
          }
        : null,
    };
  }
  return null;
}

// preface wheel opening threshold: only assistants containing non-renderable content such as model_change/session_fork host
// The message does not open the wheel to avoid synthesizing an empty wheel with only "working" shells.
function assistantMessageHasSynthesizableContent(message: MessageWithParts): boolean {
  return message.parts.some((part) => {
    switch (part.type) {
      case "text":
        return part.ignored !== true && part.text.length > 0;
      case "reasoning":
        return part.text.length > 0;
      case "tool":
        return !shouldHideInvalidToolCallFromProduct(part.tool, part.metadata);
      case "subtask":
      case "compaction":
        return true;
      case "timeline":
        return (
          part.timelineType === "context_compaction" || part.timelineType === "goal_verification"
        );
      default:
        return false;
    }
  });
}

function synthesizeSubtaskPart(
  part: Extract<MessagePart, { type: "subtask" }>,
  push: PushEvent,
  turnId: string,
): void {
  synthesizeSubagentLifecycle(
    {
      agentId: String(part.id),
      agentType: part.agent,
      description: part.description,
      prompt: part.prompt,
      summaryText: part.description,
    },
    "completed",
    push,
    turnId,
  );
}

function synthesizeAssistantParts(
  message: MessageWithParts,
  emittedCompactOperations: Set<string>,
  durableCompactPartsByOperation: ReadonlyMap<string, Extract<MessagePart, { type: "compaction" }>>,
  emittedGoalVerifications: Set<string>,
  push: PushEvent,
  turnId: string,
): AssistantSynthesisState {
  let resultType: TurnResultForHydration =
    message.info.role === "assistant" && message.info.error
      ? isPersistedAssistantCancellation(message.info.error)
        ? "cancelled"
        : isPersistedStreamRecoveryDiscard(message.info.error)
          ? "success"
          : "error_during_execution"
      : message.info.role === "assistant" && message.info.time.completed === undefined
        ? // 进程退出可能只持久化 step-start/partial，却没有 assistant error；
// The old cold hydration defaults to success, fakes normal TurnComplete and lets the exception Worked be put away.
          "cancelled"
        : "success";
  let toolCallCount = 0;
  for (const part of message.parts) {
    switch (part.type) {
      case "text":
        synthesizeTextPart(
          part,
          String(message.info.id),
          messageCreatedAtMs(message),
          push,
          turnId,
        );
        break;
      case "reasoning":
// cold hydration did not bring transcript assistant message identity to
// reasoning_start causes the restored ReasoningRow to be unable to reuse the response boundary of the live projection.
        synthesizeReasoningPart(part, String(message.info.id), push, turnId);
        break;
      case "tool": {
        const state = synthesizeToolPart(part, String(message.info.id), push, turnId);
        toolCallCount += state.toolCallCount;
        resultType = normalizeTurnResult(resultType, state.resultType);
        break;
      }
      case "timeline":
        if (synthesizeGoalVerificationPart(part, emittedGoalVerifications, push, turnId)) {
          break;
        }
        synthesizeCompactPart(
          part,
          emittedCompactOperations,
          durableCompactPartsByOperation,
          push,
          turnId,
        );
        break;
      case "compaction":
        synthesizeCompactPart(
          part,
          emittedCompactOperations,
          durableCompactPartsByOperation,
          push,
          turnId,
        );
        break;
      case "subtask":
        synthesizeSubtaskPart(part, push, turnId);
        break;
      default:
        break;
    }
  }
  const assistantFeedback = message.info.metadata?.assistantFeedback;
  if (assistantFeedback === "like" || assistantFeedback === "dislike") {
    push(
      SessionEventType.AssistantFeedbackUpdated,
      { entityId: String(message.info.id), feedback: assistantFeedback },
      turnId,
    );
  }
  return { resultType, toolCallCount };
}

//── model-only wakeup wheel──
// background wake / goal continuation of live path with TurnStarted(inputVisibility=
// model-only) turns on the independent wheel; the cold path cannot skip this type of synthetic user and let the assistant behind it proceed concurrently.
// Previous round - live/cold must have the same structure. Trigger source by shared projection policy
// The only maintenance; non-triggered synthetic contexts such as compact summary / rewind notice still do not open the wheel.

//── guide steer inline──
// drain persistent user message with metadata.turnSteerDelivery: guide=inline current round
// (not round boundary), queue=independent round (real starter, consistent with live segmentation). legacy unmarked by queue.
function steerDeliveryOfMessage(message: MessageWithParts): "guide" | "queue" | null {
  if (message.info.role !== "user") return null;
  const metadata = (message.info as { metadata?: unknown }).metadata;
  if (!metadata || typeof metadata !== "object" || Array.isArray(metadata)) return null;
  const delivery = (metadata as Record<string, unknown>).turnSteerDelivery;
  return delivery === "guide" || delivery === "queue" ? delivery : null;
}

/**
* Round boundary determination: true user starter (except guide steer - inline current round) or model-only
* Wake-up trigger (background wake / goal continuation each starts one round).
* Note: In the live "merge" scenario (the notification is merged into the current round when the active loop has not ended), the cold path cannot be persisted from
* Distinguish between facts and handle them uniformly according to boundaries - no content is lost, no bubbles, only there are known differences between round attribution and live merging scenarios.
 */
function isTurnBoundaryStarter(message: MessageWithParts): boolean {
  if (isRealUserTurnStarter(message)) {
    return steerDeliveryOfMessage(message) !== "guide";
  }
// The startup wheel is visible controlOnly user wheel, must be used as a boundary to stop the previous round of output collection (one run per session
// It is the first message, but semantically it is still an independent round boundary and cannot be merged into the previous round).
  if (workflowLaunchOfMessage(message)) return true;
  return getConversationModelOnlyTurnTriggerSource(message) !== null;
}

function backgroundResultOriginMetaOfMessage(
  message: MessageWithParts,
): BackgroundResultOriginMeta | undefined {
  const messageMetadata = message.info.metadata;
  const partMetadata = message.parts.find((part) => part.type === "text")?.metadata;
  const candidate = messageMetadata?.originMeta ?? partMetadata?.originMeta;
  if (!candidate || typeof candidate !== "object" || Array.isArray(candidate)) return undefined;
  const record = candidate as Record<string, unknown>;
  const backgroundSource = record.backgroundSource;
  const workId = typeof record.workId === "string" ? record.workId.trim() : "";
  const title = typeof record.title === "string" ? record.title.trim() : "";
// The three values ​​​​are synchronized with BackgroundResultOriginMeta (contracts/src/events/session.events.ts).
// "workflow" is workflow run (workId ≡ runId): if you miss it, the background result wheel of workflow will be lost after cold recovery.
// Silently degenerates into an untitled model-only message, and the associated key of the tool card → details page is lost.
  if (
    (backgroundSource !== "bash" &&
      backgroundSource !== "subagent" &&
      backgroundSource !== "workflow") ||
    !workId ||
    !title
  ) {
    return undefined;
  }
// The manifest payload (workflowNotification) also needs to be restored after cold recovery: if only the three base fields are read back here, after cold recovery
// The payload is lost - the manifest entry falls back to the bare header row. Use shared zod schema verification, if it is malformed, only the payload will be lost.
//Keep the base field and never throw it away: This is the projected reconstruction path, and a bad load should not disrupt the entire cold recovery.
  const workflowNotification = parseWorkflowNotificationMeta(record.workflowNotification);
  return {
    backgroundSource,
    title,
    workId,
    ...(workflowNotification ? { workflowNotification } : {}),
  };
}

/** Defensively parse the manifest payload: if it is malformed or absent, it will return undefined (the caller will make the field absent accordingly) and will never be thrown. */
function parseWorkflowNotificationMeta(
  value: unknown,
): BackgroundResultOriginMeta["workflowNotification"] {
  if (value === undefined || value === null) return undefined;
  const parsed = workflowNotificationMetaSchema.safeParse(value);
  return parsed.success ? parsed.data : undefined;
}

function isLegacyCompactMaintenanceInput(
  message: MessageWithParts,
  nextMessage: MessageWithParts | undefined,
): boolean {
  if (message.info.role !== "user") return false;
//Only repair old data missing canonical policy; explicit user-visible `/compact` must be delivered as is,
// UI must no longer rely on text to override CLI visibility authority.
  if (message.info.visibility !== undefined || message.info.semantics !== undefined) return false;
  const text = textOfMessage(message.parts).trim();
  if (text !== "/compact" && !text.startsWith("/compact ")) return false;
  if (!nextMessage || nextMessage.info.role !== "assistant") return false;
  return nextMessage.parts.some(
    (part) =>
      part.type === "compaction" ||
      (part.type === "timeline" && part.timelineType === "context_compaction"),
  );
}

interface TurnOutputCollection {
  failure?: {
    type: string;
    message: string;
    attribution?: ErrorAttribution;
    retryable?: boolean;
    data?: unknown;
  };
  nextIndex: number;
  resultType: TurnResultForHydration;
  toolCallCount: number;
  historyRoundCount: number;
  turnEndedAtMs: number;
}

/** Collects one round of assistant output (until the next round boundary); common rounds are shared with preface rounds. */
function collectTurnOutput(options: {
  messages: readonly MessageWithParts[];
  startIndex: number;
  turnId: string;
  turnStartedAtMs: number;
  emittedCompactOperations: Set<string>;
  durableCompactPartsByOperation: ReadonlyMap<string, Extract<MessagePart, { type: "compaction" }>>;
  emittedGoalVerifications: Set<string>;
  goalVerificationsByAnchor: ReadonlyMap<string, GoalVerificationFact[]>;
  onModelChange: (selection: HydratedTimelineModel) => void;
  push: PushEvent;
}): TurnOutputCollection {
  const { messages, turnId, push } = options;
  let index = options.startIndex;
  let resultType: TurnResultForHydration = "success";
  let failure: TurnOutputCollection["failure"];
// If the tail invalidated by stream recovery is the last assistant in this round, it means that the recovery request has not been placed.
//(The process exits before retransmitting), press interrupted to close this round; subsequent assistant will determine the result if it appears.
  let awaitingStreamRecovery = false;
  let toolCallCount = 0;
  let historyRoundCount = 0;
  let turnEndedAtMs = options.turnStartedAtMs;
  while (index < messages.length && !isTurnBoundaryStarter(messages[index]!)) {
    const message = messages[index]!;
    if (isProviderContextOnlyAssistant(message)) {
// selection side chat will mark the inherited assistant history as model-only.
// The old cold hydration only hides the user carrier, and then uses the assistant as preface/previous round of output synthesis.
// Causes the parent timeline to be leaked when the secondary screen is first opened and cold restored. Uniformly obey the projection policy and skip the entire line.
      index += 1;
      continue;
    }
    if (isForkTimelineMessage(message)) {
      const forkContext = forkContextOfMessage(message);
      if (forkContext) {
        push(
          SessionEventType.SessionForked,
          {
            forkPoint: 0,
            originalSessionId: forkContext.parentSessionId,
            restoredFileCount: forkContext.restoredFileCount,
            targetCheckpointId: forkContext.targetCheckpointId,
            targetMessageId: forkContext.targetMessageId,
          },
          turnId,
        );
      }
      index += 1;
      continue;
    }
// guide steer: inline into the current round - synthesized TurnSteerDrained (with drainedInputs),
// For projection, follow delivery=guide and use the inline userInput line, which is the same reduction entrance as live.
    if (message.info.role === "user" && steerDeliveryOfMessage(message) === "guide") {
      const intent = inputIntentOfMessage(message);
// cold guide used to temporarily spell pendingInputId only based on messageId, and did not
// The persisted ConversationInputIntent in transcript brings back the event; the row will be lost after restoration
// sourceCommandId/clientId/attachments, command deduplication and display are no longer equivalent to live.
// New data will reuse the original queueItemId first, and legacy will use the diagnosable hydration fallback.
      const pendingInputId = intent?.queueItemId ?? `hydrate-steer-${String(message.info.id)}`;
      push(
        SessionEventType.TurnSteerDrained,
        {
          pendingInputIds: [pendingInputId],
          injectedMessageIds: [String(message.info.id)],
          drainedInputs: [
            {
              pendingInputId,
              messageId: String(message.info.id),
              text: textOfMessage(message.parts),
              delivery: "guide",
              ...(intent ? { intent } : {}),
            },
          ],
          targetTurnId: turnId,
        },
        turnId,
        messageCreatedAtMs(message),
      );
      index += 1;
      continue;
    }
    if (message.info.role !== "assistant") {
      index += 1;
      continue;
    }
    awaitingStreamRecovery =
      message.info.error !== undefined && isPersistedStreamRecoveryDiscard(message.info.error);
    if (
      message.info.error &&
      !isPersistedAssistantCancellation(message.info.error) &&
      !awaitingStreamRecovery
    ) {
      const data = assistantErrorData(message.info.error);
      const attribution = persistedErrorAttribution(data);
      failure = {
        type: message.info.error.name,
        message: (typeof data?.message === "string" && data.message) || message.info.error.name,
// The old cold hydration only leaves the attribution in data, and the TurnError projection cannot be read, and it degrades to runtime after restarting.
        ...(attribution ? { attribution } : {}),
        ...(typeof data?.retryable === "boolean" ? { retryable: data.retryable } : {}),
        ...(message.info.error.data !== undefined ? { data: message.info.error.data } : {}),
      };
    }
    const timelineModel = modelChangeToModelOf(message);
    if (timelineModel) {
      options.onModelChange(timelineModel);
      index += 1;
      continue;
    }
// Host cannot infer model rounds from toolCallCount or clipped history length.
// The new transcript will be fixed to the exact value in the final assistant anchor; the old transcript will be independent
// Assistant history entries are counted for compatibility.
    historyRoundCount = message.info.anchor?.historyRoundCount ?? historyRoundCount + 1;
    const messageEnd = messageEndAtMs(message);
    if (messageEnd !== undefined) {
      turnEndedAtMs = Math.max(turnEndedAtMs, messageEnd);
    }
    const synthesized = synthesizeAssistantParts(
      message,
      options.emittedCompactOperations,
      options.durableCompactPartsByOperation,
      options.emittedGoalVerifications,
      push,
      turnId,
    );
    resultType = normalizeTurnResult(resultType, synthesized.resultType);
    toolCallCount += synthesized.toolCallCount;
    if (
      message.info.role === "assistant" &&
      message.info.finish?.trim().toLowerCase() === "length" &&
      synthesized.toolCallCount === 0
    ) {
// live ProductProjection can identify the output-token from the per-request ModelComplete
// Continue, but cold transcript used to be synthesized only once at the end of the entire round end_turn, resulting in a refresh
// The same sentence degenerates into multiple assistant rows. Persistence finish is the request termination fact; use zero usage
// The hydration-only ModelComplete restores qualifications without repeating the accumulation of tokens or injecting Continue user.
      push(
        SessionEventType.ModelComplete,
        {
          content: "",
          stopReason: "length",
          querySource: "main_turn",
          toolCallCount: 0,
          usage: {
            inputTokens: 0,
            outputTokens: 0,
            cacheReadTokens: 0,
            cacheWriteTokens: 0,
          },
        },
        turnId,
        messageEnd,
      );
    }
// Goal verify of session_entry source: The fact anchoring this assistant is immediately followed by
//(Same key as timeline part to remove duplicates, first come first served).
    const anchored = options.goalVerificationsByAnchor.get(String(message.info.id));
    if (anchored) {
      for (const fact of anchored) {
        pushGoalVerificationFact(fact, options.emittedGoalVerifications, push, turnId);
      }
    }
    index += 1;
  }
  if (awaitingStreamRecovery) {
    resultType = normalizeTurnResult(resultType, "cancelled");
  }
  return {
    ...(failure ? { failure } : {}),
    nextIndex: index,
    resultType,
    toolCallCount,
    historyRoundCount,
    turnEndedAtMs,
  };
}

/**
* Synthesize transcript (MessageWithParts in time/parentID order) into a reduced equivalent SessionEvent sequence.
* Round grouping: user message starts one round, followed by assistant message (output in the same round) until the next user message.
 */
export function synthesizeEventsFromMessages(
  messages: readonly MessageWithParts[],
  options: SynthesizeOptions,
): SessionEvent[] {
  const sessionId = options.sessionId as SessionId;
  const traceId = HYDRATION_TRACE_ID as TraceId;
  let seq = 0;
  const baseMs = options.baseTimestampMs ?? messages[0]?.info.time.created ?? 0;
  const events: SessionEvent[] = [];

  const push = (
    type: SessionEventType,
    payload: unknown,
    turnId?: string,
    sourceTimestampMs?: number,
  ): void => {
    seq += 1;
    events.push({
      id: `hydrate-${seq}` as EventId,
      sessionId,
      turnId: turnId as TurnId | undefined,
      type,
// source timestamp only restores display facts such as row.createdAt; the total order of events is always determined by sequenceNumber.
      timestamp: new Date(sourceTimestampMs ?? baseMs + seq),
      traceId,
      sequenceNumber: seq,
      payload,
    });
  };

// Historical messages do not declare model capacity; when the caller is unknown, it remains unknown and cannot be synthesized into the default denominator.
  const contextWindow = options.contextWindow;
  push(SessionEventType.SessionCreated, {
    mode: "default",
    contextWindow,
  });

  let turnNumber = 0;
  let index = 0;
  const emittedCompactOperations = new Set<string>();
  const durableCompactPartsByOperation = new Map<
    string,
    Extract<MessagePart, { type: "compaction" }>
  >();
  for (const message of messages) {
    for (const part of message.parts) {
      if (part.type !== "compaction") continue;
      if (!part.timelineStatus && !part.tail_start_id && !part.compactBoundary) continue;
      const operationId = String(
        part.operationId ?? part.boundaryId ?? `legacy-compact-${String(part.id)}`,
      );
      const existing = durableCompactPartsByOperation.get(operationId);
      if (!existing || (!existing.tail_start_id && part.tail_start_id)) {
        durableCompactPartsByOperation.set(operationId, part);
      }
    }
  }
  const emittedGoalVerifications = new Set<string>();
  let lastTurnId: string | undefined;

  const entryFacts = mergeGoalVerificationEntryFacts(options.goalVerificationEntries ?? []);
  const goalVerificationsByAnchor = new Map<string, GoalVerificationFact[]>();
  for (const fact of entryFacts) {
    if (!fact.anchorAssistantMessageId) continue;
    const list = goalVerificationsByAnchor.get(fact.anchorAssistantMessageId) ?? [];
    list.push(fact);
    goalVerificationsByAnchor.set(fact.anchorAssistantMessageId, list);
  }

// MC-cold: modelChange marker is projected on TurnStarted
// Compare lastTurnModel and config generation; cold recovery is based on each round of persistence selection fact before TurnStarted
//Synthetic ModelSelected - ordinary first-round silence, explicit source-less and subsequent A→B boundary constant reconstruction.
// This synthetic event carries HYDRATION_TRACE_ID and does not declare the seed authority (see onModelSelected).
  let lastSelectedModelKey: string | null = null;
  let pendingTimelineModel: HydratedTimelineModel | null = null;
  const selectTurnModel = (selection: HydratedTimelineModel | null): void => {
    if (!selection) return;
    const key = hydratedModelKey(selection.modelSelection);
    if (key === lastSelectedModelKey) return;
    lastSelectedModelKey = key;
    push(SessionEventType.ModelSelected, {
      modelSelection: selection.modelSelection,
      ...(selection.previousModelSelection !== undefined
        ? {
            previousModelSelection: selection.previousModelSelection
              ? selection.previousModelSelection
              : null,
          }
        : {}),
    });
  };
  const selectAcceptedTurnModel = (fallback: ModelSelection | null): void => {
    const selected = pendingTimelineModel ?? (fallback ? { modelSelection: fallback } : null);
    pendingTimelineModel = null;
    selectTurnModel(selected);
  };
  const recordTimelineModel = (selection: HydratedTimelineModel): void => {
    pendingTimelineModel = selection;
    selectTurnModel(selection);
  };

  const finishTurn = (input: {
    failure?: TurnOutputCollection["failure"];
    fileChanges?: TurnFileChangeSummary;
    turnId: string;
    resultType: TurnResultForHydration;
    toolCallCount: number;
    historyRoundCount: number;
    turnStartedAtMs: number;
    turnEndedAtMs: number;
  }): void => {
    if (input.failure) {
// Failure before the first word of provider is only persisted in assistant.info.error, old cold path
// Fold into TurnComplete(error_during_execution), causing all the code/message of lastError to be lost.
// Reuse the live TurnError state machine here to avoid building another cold-only error reducer.
      push(
        SessionEventType.TurnError,
        { error: input.failure, turnPhase: "model" },
        input.turnId,
        input.turnEndedAtMs,
      );
      return;
    }
    push(
      SessionEventType.ModelComplete,
      {
        content: "",
        stopReason: "end_turn",
        querySource: "main_turn",
// Cold recovery once fixed the window of synthetic events to 200,000, covering the same model in
// 1M capability in the workspace provider registry; the current model truth value parsed by the caller is used here.
        contextWindow,
        usage: {
          inputTokens: 0,
          outputTokens: 0,
          cacheReadTokens: 0,
          cacheWriteTokens: 0,
        },
        ...(input.fileChanges ? { fileChanges: input.fileChanges } : {}),
      },
      input.turnId,
      input.turnEndedAtMs,
    );
    push(
      SessionEventType.TurnComplete,
      {
        response: "",
        tokenCount: 0,
        toolCallCount: input.toolCallCount,
        historyRoundCount: input.historyRoundCount,
// Cold recovery is a reverse synthesis of events from message transcript, and cannot be used like live
// The event also depends on the runtime startedAt; fixing it to 0 will cause the historical rounds to be displayed as 1 second.
        duration: Math.max(0, input.turnEndedAtMs - input.turnStartedAtMs),
        resultType: input.resultType,
      },
      input.turnId,
      input.turnEndedAtMs,
    );
  };

  while (index < messages.length) {
    const message = messages[index]!;
    if (isProviderContextOnlyAssistant(message)) {
      index += 1;
      continue;
    }
    if (isLegacyCompactMaintenanceInput(message, messages[index + 1])) {
// The user host of the old manual compact is just a maintenance command, not a real-user intent; after skipping the host,
// The next assistant compact fact will go through the preface model-only round and generate a canonical marker.
      index += 1;
      continue;
    }
    if (isForkTimelineMessage(message)) {
      const forkContext = forkContextOfMessage(message);
      if (forkContext) {
        push(SessionEventType.SessionForked, {
          forkPoint: 0,
          originalSessionId: forkContext.parentSessionId,
          restoredFileCount: forkContext.restoredFileCount,
          targetCheckpointId: forkContext.targetCheckpointId,
          targetMessageId: forkContext.targetMessageId,
        });
      }
      index += 1;
      continue;
    }
    const timelineModel = modelChangeToModelOf(message);
    if (timelineModel) {
// model_change timeline part is a persistent boundary fact for accepted rounds;
// The host message cannot be completely skipped, only the subsequent user message model snapshot happens to be rebuilt:
// The marker will disappear when the snapshot is missing/lags, so consume the explicit toModel first,
// The next TurnStarted will only use this authoritative selection and will no longer be overwritten by the lagging snapshot.
      recordTimelineModel(timelineModel);
      index += 1;
      continue;
    }
    const workflowLaunch = workflowLaunchOfMessage(message);
    if (workflowLaunch) {
// The startup wheel started directly by the center: visible controlOnly user wheel, cold recovery must be the same shape as the live projection - the same messageId,
// origin workflowLaunch (mapped by inputSource), same workflowLaunch metadata, no assistant output.
//Place it before the real-user branch to prevent this synthetic user from being skipped by hiddenSynthetic.
      turnNumber += 1;
      const turnId = `hydrate-turn-${turnNumber}`;
      lastTurnId = turnId;
      const launchText = textOfMessage(message.parts);
      const turnStartedAtMs = messageCreatedAtMs(message) ?? baseMs + seq;
      selectAcceptedTurnModel(turnModelSelectionOfUserMessage(message));
      push(
        SessionEventType.TurnStarted,
        {
          turnNumber,
// Text still goes into userInput.text (downgraded rendering of old client/TUI); GUI uses metadata to draw launch cards.
          input: launchText,
// The persistent messageId is the authoritative target of this round, which is used with live projection, otherwise the productTurn identity is bifurcated into hot and cold.
          messageId: String(message.info.id),
// The startup wheel does not execute Agent (controlOnly, no working hours); source driver origin=workflowLaunch.
          executionKind: "controlOnly",
          inputSource: "workflow_launch",
          workflowLaunch,
        },
        turnId,
        turnStartedAtMs,
      );
      index += 1;
      const collected = collectTurnOutput({
        messages,
        startIndex: index,
        turnId,
        turnStartedAtMs,
        emittedCompactOperations,
        durableCompactPartsByOperation,
        emittedGoalVerifications,
        goalVerificationsByAnchor,
        onModelChange: recordTimelineModel,
        push,
      });
      index = collected.nextIndex;
      finishTurn({
        failure: collected.failure,
        turnId,
        resultType: collected.resultType,
        toolCallCount: collected.toolCallCount,
        historyRoundCount: collected.historyRoundCount,
        turnStartedAtMs,
        turnEndedAtMs: collected.turnEndedAtMs,
      });
      continue;
    }
    if (!isRealUserTurnStarter(message)) {
// model-only wake wheel: background wake /
// The synthetic user triggered by goal continuation opens an independent model-only round (no visible bubbles,
//The notification text does not enter rows), and then the assistant returns to the current round - TurnStarted with live
// (inputVisibility=model-only) The structure is consistent and will not advance to the previous round.
      const wakeSource = getConversationModelOnlyTurnTriggerSource(message);
      if (wakeSource) {
        turnNumber += 1;
        const turnId = `hydrate-turn-${turnNumber}`;
        lastTurnId = turnId;
        const turnStartedAtMs = messageCreatedAtMs(message) ?? baseMs + seq;
        selectAcceptedTurnModel(turnModelSelectionOfUserMessage(message));
        push(
          SessionEventType.TurnStarted,
          {
            turnNumber,
// cold hydration once cleared the original text of model-only wake background,
// As a result, even if ProductProjection can consume task-notification, it cannot be obtained during recovery.
// tool-use-id and failure details. The input is still model-only and no user bubbles are generated.
            input: wakeSource === "background_task" ? textOfMessage(message.parts) : "",
            inputVisibility: "model-only",
            inputSource: wakeSource,
            ...(wakeSource === "background_task"
              ? { originMeta: backgroundResultOriginMetaOfMessage(message) }
              : {}),
// model-only trigger is also a persistent user entity; if messageId is not passed,
// cold will degenerate to hydrate-turn-N, and the live/cold productTurn identity will fork again.
            messageId: String(message.info.id),
          },
          turnId,
          turnStartedAtMs,
        );
        index += 1;
        const collected = collectTurnOutput({
          messages,
          startIndex: index,
          turnId,
          turnStartedAtMs,
          emittedCompactOperations,
          durableCompactPartsByOperation,
          emittedGoalVerifications,
          goalVerificationsByAnchor,
          onModelChange: recordTimelineModel,
          push,
        });
        index = collected.nextIndex;
        finishTurn({
          failure: collected.failure,
          fileChanges: options.fileChangeSummariesByMessageId?.get(String(message.info.id)),
          turnId,
          resultType: collected.resultType,
          toolCallCount: collected.toolCallCount,
          historyRoundCount: collected.historyRoundCount,
          turnStartedAtMs,
          turnEndedAtMs: collected.turnEndedAtMs,
        });
        continue;
      }
// assistant-head-skip fix ("assistant recovery entire section disappears" cold path vector):
// Messages before the first real user message cannot be skipped - the session header is rewind notice /
// When compact summary is not a real user message, the entire paragraph of the assistant reply will disappear after refreshing.
// Hence the composite preface model-only wheel for the head assistant output (no visible user bubble,
//The content is rendered as usual). The non-triggered synthetic context of the user role is still invisible by design and is still skipped.
      if (message.info.role !== "assistant" || !assistantMessageHasSynthesizableContent(message)) {
        index += 1;
        continue;
      }
      turnNumber += 1;
      const turnId = `hydrate-turn-${turnNumber}`;
      lastTurnId = turnId;
      const turnStartedAtMs = messageCreatedAtMs(message) ?? baseMs + seq;
      selectAcceptedTurnModel(assistantModelSelectionOf(message));
      push(
        SessionEventType.TurnStarted,
        { turnNumber, input: "", inputVisibility: "model-only" },
        turnId,
        turnStartedAtMs,
      );
      const collected = collectTurnOutput({
        messages,
        startIndex: index,
        turnId,
        turnStartedAtMs,
        emittedCompactOperations,
        durableCompactPartsByOperation,
        emittedGoalVerifications,
        goalVerificationsByAnchor,
        onModelChange: recordTimelineModel,
        push,
      });
      index = collected.nextIndex;
      finishTurn({
        failure: collected.failure,
        turnId,
        resultType: collected.resultType,
        toolCallCount: collected.toolCallCount,
        historyRoundCount: collected.historyRoundCount,
        turnStartedAtMs,
        turnEndedAtMs: collected.turnEndedAtMs,
      });
      continue;
    }

    turnNumber += 1;
    const turnId = `hydrate-turn-${turnNumber}`;
    lastTurnId = turnId;
    const userText = textOfMessage(message.parts);
    const attachments = attachmentMetasOfMessage(message.parts);
    const intent = inputIntentOfMessage(message);
    const executionKind = executionKindOfMessage(message);
    const epilogueStart = epilogueStartOfMessage(message);
    const turnStartedAtMs = messageCreatedAtMs(message) ?? baseMs + seq;
// legacy synthetic can be sandwiched between the timeline part and the next accepted turn
// context, cannot be guessed based on "immediately preceding one"; hold explicit boundaries until the actual start of the round.
    selectAcceptedTurnModel(turnModelSelectionOfUserMessage(message));
    push(
      SessionEventType.TurnStarted,
      {
        turnNumber,
        input: userText,
        ...(epilogueStart === undefined ? {} : { epilogueStart }),
// Root cause: cold hydration used to only rebuild the visible user row, leaving out the persistent messageId, resulting in
// The same historical message is visible in the UI but cannot be addressed by the edit/rewind command. real user
// transcript message is the authoritative target of this row and must be consistent with live TurnStarted
// Use the same messageId field to enter ProductProjection.
        messageId: String(message.info.id),
        ...(executionKind ? { executionKind } : {}),
        ...(message.info.anchor?.sourceCommandId
          ? { inputId: message.info.anchor.sourceCommandId }
          : {}),
        ...(intent ? { intent } : {}),
        ...(attachments.length > 0 ? { attachments } : {}),
      },
      turnId,
      turnStartedAtMs,
    );
    index += 1;

    const collected = collectTurnOutput({
      messages,
      startIndex: index,
      turnId,
      turnStartedAtMs,
      emittedCompactOperations,
      durableCompactPartsByOperation,
      emittedGoalVerifications,
      goalVerificationsByAnchor,
      onModelChange: recordTimelineModel,
      push,
    });
    index = collected.nextIndex;
    finishTurn({
      failure: collected.failure,
      fileChanges: options.fileChangeSummariesByMessageId?.get(String(message.info.id)),
      turnId,
      resultType: collected.resultType,
      toolCallCount: collected.toolCallCount,
      historyRoundCount: collected.historyRoundCount,
      turnStartedAtMs,
      turnEndedAtMs: collected.turnEndedAtMs,
    });
  }

// The anchor is missing or points to an entry that does not appear in the transcript. Fact: falls to the end of the known timeline
//(The key has been pressed to remove duplicates, and the successfully anchored one has been launched in the above loop; the timestamp is not guessed, and it is not lost silently).
  for (const fact of entryFacts) {
    pushGoalVerificationFact(fact, emittedGoalVerifications, push, lastTurnId);
  }

  return events;
}
