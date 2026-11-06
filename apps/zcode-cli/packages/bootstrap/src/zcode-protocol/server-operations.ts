/* eslint-disable max-lines -- ZCode Protocol's session/workspace methods share the same server context and snapshot helpers, and are maintained centrally during the migration period. */
import { observeSessionDebug } from "./session-debug.js";
import {
  TASK_LIST_SESSION_TYPES,
  isTaskListSessionType,
} from "../zcode-protocol-v4/task-list-session-membership.js";
import { resolveEffectiveBashShellSelection } from "@zcode/adapters/exec";
import { inputIntentMetadata } from "../zcode-protocol-v4/commands/input-intent.js";
import { createModelExecutionContext } from "./model-execution.js";
import type { SendInputOptions } from "../app/types.js";
import { repairPersistedRemoteSessionPaths, type TurnAttachment } from "@zcode/core";
import {
  CoreErrorType,
  SESSION_ENTRY_TARGET_COMPLETION_VERIFICATION,
  createMessageId,
  createPartId,
  createSessionId,
  createCoreError,
  parseRewindTriggeredPayload,
  RewindScope,
  RewindStrategy,
  type EventId,
  type ExecutionShellSelection,
  type MessageWithParts,
  type ModelSelection,
  type MessageId,
  type ModelId,
  type ModelProviderId,
  type QueryId,
  type SessionEvent,
  SessionEventType,
  type SessionId,
  type SessionInfo,
  type SessionTaskType,
  type CollaborationMode,
  type TargetCompletionVerificationPayload,
  type TraceId,
  type TurnBackgroundAttribution,
  type TurnId,
  type UsageStorePort,
  type WorkspaceId,
} from "@zcode/contracts";
import {
  DEFAULT_ZCODE_MODEL_CONTEXT_BUDGET_STRATEGY,
  ZCODE_SESSION_RUNTIME_PREFERENCES_REQUEST_TIMEOUT_MS,
  zcodeProtocolErrorCodes,
  zcodeProtocolMethods,
  zcodeSessionCancelBackgroundTaskParamsSchema,
  zcodeSessionCompactParamsSchema,
  zcodeSessionCloseParamsSchema,
  zcodeSessionCreateParamsSchema,
  zcodeSessionEventsParamsSchema,
  zcodeSessionForkParamsSchema,
  zcodeSessionGoalParamsSchema,
  zcodeSessionListParamsSchema,
  zcodeSessionMessagesParamsSchema,
  zcodeSessionReadParamsSchema,
  zcodeSessionRuntimePreferencesResultSchema,
  zcodeSessionResumeParamsSchema,
  zcodeSessionSendParamsSchema,
  zcodeSessionSetModeParamsSchema,
  zcodeSessionSetModelParamsSchema,
  zcodeSessionSetThoughtLevelParamsSchema,
  zcodeSessionStopParamsSchema,
  zcodeSessionSubscribeParamsSchema,
  zcodeSessionSubagentsParamsSchema,
  zcodeTaskTokenUsageParamsSchema,
  zcodeUsageStatsParamsSchema,
  zcodeWorkspaceGenerateTextParamsSchema,
  getConversationMessageProjectionPolicy,
  parseRemoteWorkspaceIdentity,
  type ZCodeSessionCreateParams,
  type ZCodeDeliveryKind,
  type IntegratedTerminalShellSelection,
  type ZCodeSessionRuntimePreferencesScope,
  type ZCodeSessionRuntimePreferencesResult,
  type ZCodeModelContextBudgetStrategy,
  type ZCodeProtocolTrace,
  type ZCodeSessionEvent,
  type ZCodeSessionHistoryTarget,
  type ZCodeSessionResumeParams,
  type ZCodeSessionPersistence,
  type ZCodeStateUpdatedNotification,
} from "@zcode/shared";
import {
  buildSessionSnapshot,
  buildWorkspaceRef,
  formatProtocolModelSelection,
  mapSessionEventForProtocol,
  mapSessionInfo,
  resolveSessionContextUsage,
  shouldExposeSessionEventToProtocol,
} from "./mapper.js";
import { optionalModelSelectionFromString } from "./model-mapper.js";
import {
  ProtocolRequestError,
  assertExpectedRevision,
  createProtocolRootTraceContext,
  parseParams,
  requireSession,
  type ZCodeProtocolAgentServerContext,
  type ZCodeProtocolSessionRecord,
  type ZCodeProtocolToolInputTransmissionState,
} from "./server-types.js";
import { createWorkspaceZCodeApp, ensureSessionModelAvailable } from "./workspace-model-runtime.js";
import { buildAppUsageSnapshot, resolveTzOffsetMs } from "./usage-stats-builder.js";
import { createProtocolInteractionBroker } from "./interaction-broker.js";
import { createProtocolAutomationPort } from "./automation-port.js";
import { createProtocolOffPeakPort } from "./offpeak-port.js";
import { createProtocolBrowserControlBroker } from "./browser-control-broker.js";
import { mapComputerUseOperationEvent } from "./computer-use-operation-event.js";
import { protocolMcpServersToRuntimeMcpConfig } from "./protocol-mcp-config.js";
import { projectIdFromDirectory } from "../app/paths.js";
import {
  collectSubagentChildSessionIds,
  paginateEndedSubagents,
  projectSessionSubagents,
} from "./subagent-session-query.js";
import { runSessionModelConfigMutation } from "../zcode-protocol-v4/model-config-mutation.js";
import { runWithSessionResidencyFinalization } from "./session-residency.js";

const PLAN_MODE_GOAL_CONTINUATION_SKIPPED_MESSAGE = "The goal is recorded in Plan mode, but will not continue automatically.";
const SLOW_SNAPSHOT_LOG_THRESHOLD_MS = 1000;

type ProtocolGoalTarget = NonNullable<
  Awaited<ReturnType<NonNullable<ZCodeProtocolSessionRecord["app"]["readTarget"]>>>
>;

type ZCodeSessionRecordParams = (
  | ZCodeSessionCreateParams
  | (ZCodeSessionResumeParams & {
      mode?: ZCodeSessionCreateParams["mode"];
      model?: ZCodeSessionCreateParams["model"];
      parentSessionId?: ZCodeSessionCreateParams["parentSessionId"];
      thoughtLevel?: ZCodeSessionCreateParams["thoughtLevel"];
// Automated sessions will turn off title secondary generation during creation, and create/resume
// Shared record initialization function; resume compatible branches must also declare this policy field.
      titleGenerationEnabled?: ZCodeSessionCreateParams["titleGenerationEnabled"];
    })
) & { taskType?: SessionTaskType };

interface SessionStartupPreferences {
  memoryEnabled: boolean;
  modelContextBudgetStrategy: ZCodeModelContextBudgetStrategy;
  nativeSearchEnhancementsEnabled: boolean;
  resolveInitialBashShellSelection: () => Promise<ExecutionShellSelection | undefined>;
}

type SessionStartupPreferencesSource =
  | { kind: "host" }
  | { kind: "inherit"; parent: ZCodeProtocolSessionRecord };

function resolveSupportedAppThoughtLevel(
  app: Pick<ZCodeProtocolSessionRecord["app"], "listThoughtLevels">,
  thoughtLevel: string | undefined,
): string | undefined {
  const normalizedThoughtLevel = thoughtLevel?.trim();
  if (!normalizedThoughtLevel) {
    return undefined;
  }
  const supportedLevels = app.listThoughtLevels();
  return supportedLevels.includes(normalizedThoughtLevel) ? normalizedThoughtLevel : undefined;
}

const DEFAULT_PROTOCOL_EVENT_SEQUENCE_KEY = "__default__";
const PROTOCOL_TOOL_INPUT_DELTA_BATCH_MAX_CHARS = 4 * 1024;
const PROTOCOL_TOOL_INPUT_DELTA_BATCH_MAX_INTERVAL_MS = 750;
const PROTOCOL_TEXT_STREAMING_DELTA_BATCH_MAX_CHARS = 2 * 1024;
const PROTOCOL_TEXT_STREAMING_DELTA_BATCH_MAX_INTERVAL_MS = 250;

type ProtocolStreamingDeltaKind = "reasoning_delta" | "text_delta" | "tool_input_delta";

interface ProtocolStreamingDeltaBatchInfo {
  batchKey: string;
  delta: string;
  kind: ProtocolStreamingDeltaKind;
  maxChars: number;
  shouldFlushFirstDelta: boolean;
  timestampMs: number;
}

interface ProtocolStreamingDeltaBatch {
  batchKey: string;
  delta: string;
  event: SessionEvent;
  kind: ProtocolStreamingDeltaKind;
  maxChars: number;
  startedAtMs: number;
  updatedAtMs: number;
}

interface LiveProtocolStreamingDeltaBatch {
  batch?: ProtocolStreamingDeltaBatch;
  flushedFirstDeltaBatchKeys: Set<string>;
  lastFlushAtByBatchKey: Map<string, number>;
}

const liveProtocolStreamingDeltaBatches = new WeakMap<
  ZCodeProtocolSessionRecord,
  Map<string, LiveProtocolStreamingDeltaBatch>
>();

function protocolEventSequenceKey(deliveryKind?: ZCodeDeliveryKind): string {
  return deliveryKind ?? DEFAULT_PROTOCOL_EVENT_SEQUENCE_KEY;
}

function getProtocolEventSequenceState(
  record: ZCodeProtocolSessionRecord,
  deliveryKind?: ZCodeDeliveryKind,
) {
  const key = protocolEventSequenceKey(deliveryKind);
  const existing = record.protocolEventSequences.get(key);
  if (existing) {
    return existing;
  }
  const created = {
    lastSeq: 0,
    seqBySourceEventKey: new Map<string, number>(),
  };
  record.protocolEventSequences.set(key, created);
  return created;
}

function getProtocolToolInputTransmissionState(
  record: ZCodeProtocolSessionRecord,
  deliveryKind?: ZCodeDeliveryKind,
): ZCodeProtocolToolInputTransmissionState {
  const key = protocolEventSequenceKey(deliveryKind);
  const existing = record.protocolToolInputTransmissions.get(key);
  if (existing) {
    return existing;
  }
  const created: ZCodeProtocolToolInputTransmissionState = {
    streamedToolCallIdsWithInput: new Set(),
  };
  record.protocolToolInputTransmissions.set(key, created);
  return created;
}

function protocolSourceEventKey(event: SessionEvent): string {
  return event.sequenceNumber > 0 ? `seq:${event.sequenceNumber}` : `event:${String(event.id)}`;
}

function assignProtocolEventSeq(
  record: ZCodeProtocolSessionRecord,
  event: SessionEvent,
  deliveryKind?: ZCodeDeliveryKind,
): number {
  const state = getProtocolEventSequenceState(record, deliveryKind);
  const sourceEventKey = protocolSourceEventKey(event);
  const existing = state.seqBySourceEventKey.get(sourceEventKey);
  if (existing !== undefined) {
    return existing;
  }
  const nextSeq = state.lastSeq + 1;
  state.lastSeq = nextSeq;
  state.seqBySourceEventKey.set(sourceEventKey, nextSeq);
  return nextSeq;
}

function asRecord(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null ? (value as Record<string, unknown>) : {};
}

function stringValue(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

function numberValue(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function getLiveProtocolStreamingDeltaBatchState(
  record: ZCodeProtocolSessionRecord,
  deliveryKind?: ZCodeDeliveryKind,
): LiveProtocolStreamingDeltaBatch {
  let byDeliveryKind = liveProtocolStreamingDeltaBatches.get(record);
  if (!byDeliveryKind) {
    byDeliveryKind = new Map();
    liveProtocolStreamingDeltaBatches.set(record, byDeliveryKind);
  }
  const key = protocolEventSequenceKey(deliveryKind);
  const existing = byDeliveryKind.get(key);
  if (existing) {
    return existing;
  }
  const created: LiveProtocolStreamingDeltaBatch = {
    flushedFirstDeltaBatchKeys: new Set(),
    lastFlushAtByBatchKey: new Map(),
  };
  byDeliveryKind.set(key, created);
  return created;
}

function buildProtocolStreamingDeltaBatchKey(params: {
  assistantMessageId?: string;
  inputId?: string;
  kind: ProtocolStreamingDeltaKind;
  partId?: string;
  parentToolUseId?: string;
  toolCallId?: string;
}): string {
  return [
    params.kind,
    params.assistantMessageId ?? "",
    params.inputId ?? "",
    params.partId ?? "",
    params.parentToolUseId ?? "",
    params.toolCallId ?? "",
  ].join(":");
}

function readProtocolStreamingParentToolUseId(
  payload: Record<string, unknown>,
): string | undefined {
  const direct = stringValue(payload.parentToolUseId) ?? stringValue(payload.parentToolCallId);
  if (direct) {
    return direct;
  }
  const meta = asRecord(payload._meta);
  const zcode = asRecord(meta.zcode);
  return (
    stringValue(meta.parentToolUseId) ??
    stringValue(meta.parentToolCallId) ??
    stringValue(zcode.parentToolUseId) ??
    stringValue(zcode.parentToolCallId)
  );
}

function timestampMsForProtocolEvent(event: SessionEvent): number {
  const timestampMs = event.timestamp.getTime();
  return Number.isFinite(timestampMs) ? timestampMs : 0;
}

function readStreamingDeltaBatchableEvent(
  event: SessionEvent,
): ProtocolStreamingDeltaBatchInfo | null {
  if (event.type !== SessionEventType.ModelStreaming) {
    return null;
  }
  const payload = asRecord(event.payload);
  const kind = stringValue(payload.kind) as ProtocolStreamingDeltaKind | undefined;
  if (kind !== "tool_input_delta" && kind !== "text_delta" && kind !== "reasoning_delta") {
    return null;
  }
  const delta = stringValue(payload.delta);
  if (!delta) {
    return null;
  }
  const assistantMessageId = stringValue(payload.assistantMessageId);
  const inputId = stringValue(payload.inputId);
  const partId = stringValue(payload.partId);
  const parentToolUseId = readProtocolStreamingParentToolUseId(payload);
  const toolCallId = stringValue(payload.toolCallId);
  if (kind === "tool_input_delta" && !toolCallId) {
    return null;
  }
  return {
    batchKey: buildProtocolStreamingDeltaBatchKey({
      assistantMessageId,
      inputId,
      kind,
      partId,
      parentToolUseId,
      toolCallId,
    }),
    delta,
    kind,
    maxChars:
      kind === "tool_input_delta"
        ? PROTOCOL_TOOL_INPUT_DELTA_BATCH_MAX_CHARS
        : PROTOCOL_TEXT_STREAMING_DELTA_BATCH_MAX_CHARS,
    shouldFlushFirstDelta:
      kind === "text_delta" || kind === "reasoning_delta" || kind === "tool_input_delta",
    timestampMs: timestampMsForProtocolEvent(event),
  };
}

function mergeProtocolStreamingDeltaBatch(
  batch: ProtocolStreamingDeltaBatch | undefined,
  event: SessionEvent,
  deltaInfo: ProtocolStreamingDeltaBatchInfo,
): ProtocolStreamingDeltaBatch {
  if (!batch || batch.batchKey !== deltaInfo.batchKey) {
    return {
      batchKey: deltaInfo.batchKey,
      delta: deltaInfo.delta,
      event,
      kind: deltaInfo.kind,
      maxChars: deltaInfo.maxChars,
      startedAtMs: deltaInfo.timestampMs,
      updatedAtMs: deltaInfo.timestampMs,
    };
  }
  return {
    batchKey: deltaInfo.batchKey,
    delta: `${batch.delta}${deltaInfo.delta}`,
    event,
    kind: deltaInfo.kind,
    maxChars: deltaInfo.maxChars,
    startedAtMs: batch.startedAtMs,
    updatedAtMs: deltaInfo.timestampMs,
  };
}

function materializeProtocolStreamingDeltaBatch(batch: ProtocolStreamingDeltaBatch): SessionEvent {
  const payload = asRecord(batch.event.payload);
  return {
    ...batch.event,
    payload: {
      ...payload,
      delta: batch.delta,
    },
  };
}

function rememberProtocolStreamingDeltaBatchFlush(
  lastFlushAtByBatchKey: Map<string, number>,
  batch: ProtocolStreamingDeltaBatch,
): void {
  lastFlushAtByBatchKey.set(batch.batchKey, batch.updatedAtMs);
}

function shouldFlushProtocolStreamingDeltaBatchForInterval(
  batch: ProtocolStreamingDeltaBatch,
  lastFlushAtByBatchKey: Map<string, number>,
): boolean {
  const lastFlushAt = lastFlushAtByBatchKey.get(batch.batchKey);
  if (lastFlushAt === undefined) {
    return false;
  }
// The streaming function call of the active task cannot only output packets according to the 4KB byte budget.
// Single-line JSON parameters will stay at the protocol boundary for a long time, which means that the tool card parameters will no longer be updated in a streaming manner.
  const maxIntervalMs =
    batch.kind === "tool_input_delta"
      ? PROTOCOL_TOOL_INPUT_DELTA_BATCH_MAX_INTERVAL_MS
      : PROTOCOL_TEXT_STREAMING_DELTA_BATCH_MAX_INTERVAL_MS;
  return batch.updatedAtMs - lastFlushAt >= maxIntervalMs;
}

function markStreamedToolInputIfPresent(
  mappedEvent: ZCodeSessionEvent,
  toolInputTransmissions: ZCodeProtocolToolInputTransmissionState,
): void {
  if (mappedEvent.type !== "model.streaming") {
    return;
  }
  const payload = asRecord(mappedEvent.payload);
  if (payload.kind !== "tool_call" || !("input" in payload)) {
    return;
  }
  const toolCallId = stringValue(payload.toolCallId);
  if (!toolCallId) {
    return;
  }
  toolInputTransmissions.streamedToolCallIdsWithInput.add(toolCallId);
}

function omitDuplicateScheduledToolInput(
  event: SessionEvent,
  mappedEvent: ZCodeSessionEvent,
  toolInputTransmissions: ZCodeProtocolToolInputTransmissionState,
): ZCodeSessionEvent {
  markStreamedToolInputIfPresent(mappedEvent, toolInputTransmissions);
  if (event.type !== SessionEventType.ToolCallScheduled || mappedEvent.type !== "tool.updated") {
    return mappedEvent;
  }
  const payload = asRecord(mappedEvent.payload);
  if (payload.kind !== "scheduled" || !("input" in payload)) {
    return mappedEvent;
  }
  const toolCallId = stringValue(payload.toolCallId);
  if (!toolCallId || !toolInputTransmissions.streamedToolCallIdsWithInput.has(toolCallId)) {
    return mappedEvent;
  }

  const nextPayload: Record<string, unknown> = { ...payload };
  const input = nextPayload.input;
  delete nextPayload.input;
  nextPayload.inputByteLength = measureJsonBytes(input);
  nextPayload.inputOmitted = true;
  nextPayload.inputRef = "model_stream";
//Performance fix: AI SDK's tool_call has delivered the complete input in model.streaming;
// Scheduled is just the life cycle boundary. Repeatedly carrying large Write/Edit parameters will double the stdio and renderer amplification.
  return {
    ...mappedEvent,
    payload: nextPayload,
  };
}

function mapProtocolPromptAttachments(
  attachments: unknown[] | undefined,
): TurnAttachment[] | undefined {
  const mapped = (attachments ?? [])
    .map(mapProtocolPromptAttachment)
    .filter((attachment): attachment is TurnAttachment => attachment !== undefined);
  return mapped.length > 0 ? mapped : undefined;
}

function mapProtocolPromptAttachment(attachment: unknown): TurnAttachment | undefined {
  const record = asRecord(attachment);
  const kind = stringValue(record.kind);
  const filename = stringValue(record.filename) ?? "attachment";
  const localPath = stringValue(record.localPath);
  const mimeType = stringValue(record.mimeType);
  const isPdf =
    kind === "pdf" || mimeType?.split(";", 1)[0]?.trim().toLowerCase() === "application/pdf";
// The GUI protocol attachment uses kind/localPath/dataBase64, while the core runtime only recognizes type/path/content.
// Semantic conversion is completed here at the protocol boundary to avoid leaking the renderer's transmission format into the agent.
// Display meta information (filename/mimeType/sizeBytes) with fidelity and transparent transmission at protocol boundaries,
// TurnStarted event and attachment display of v4 userInput row no longer rely on basename/extension inference.
  const displayMeta = {
    filename,
    ...(mimeType ? { mimeType } : {}),
    ...(typeof record.sizeBytes === "number" ? { sizeBytes: record.sizeBytes } : {}),
  };
  if (isPdf) {
    if (localPath) {
      return { path: localPath, type: "pdf", ...displayMeta };
    }
    const dataBase64 = stringValue(record.dataBase64);
    return dataBase64
      ? {
          content: `data:application/pdf;base64,${dataBase64}`,
          path: filename,
          type: "pdf",
          ...displayMeta,
        }
      : undefined;
  }
  if (kind === "image") {
    if (localPath) {
      return { path: localPath, type: "image", ...displayMeta };
    }
    const dataBase64 = stringValue(record.dataBase64);
    const mimeType = stringValue(record.mimeType) ?? "image/*";
    return dataBase64
      ? {
          content: `data:${mimeType};base64,${dataBase64}`,
          path: filename,
          type: "image",
          ...displayMeta,
        }
      : undefined;
  }

// video: localPath zero copy (the agent reads the file side-by-side for size verification); the dataBase64 group data URL inline on the Web side.
  if (kind === "video") {
    if (localPath) {
      return { path: localPath, type: "video", ...displayMeta };
    }
    const dataBase64 = stringValue(record.dataBase64);
    const mimeType = stringValue(record.mimeType) ?? "video/mp4";
    return dataBase64
      ? {
          content: `data:${mimeType};base64,${dataBase64}`,
          path: filename,
          type: "video",
          ...displayMeta,
        }
      : undefined;
  }

  if (kind === "file" || kind === "audio") {
    if (localPath) {
      return {
        path: localPath,
        ...(kind === "file" && stringValue(record.sourceKind) === "clipboard-text"
          ? { sourceKind: "clipboard-text" as const }
          : {}),
        type: "file",
        ...displayMeta,
      };
    }
    const textContent = stringValue(record.textContent);
    if (textContent !== undefined) {
      return {
        content: textContent,
        path: filename,
        type: "file",
        ...displayMeta,
      };
    }
    const decoded = decodeTextProtocolAttachment(record);
    return decoded !== undefined
      ? { content: decoded, path: filename, type: "file", ...displayMeta }
      : undefined;
  }

  return undefined;
}

function decodeTextProtocolAttachment(record: Record<string, unknown>): string | undefined {
  const dataBase64 = stringValue(record.dataBase64);
  if (!dataBase64) return undefined;
  const sizeBytes = numberValue(record.sizeBytes);
  if (sizeBytes !== undefined && sizeBytes > 64 * 1024) return undefined;
  try {
    return Buffer.from(dataBase64, "base64").toString("utf8");
  } catch {
    return undefined;
  }
}

function shouldHideProtocolSessionEvent(
  record: ZCodeProtocolSessionRecord,
  event: SessionEvent,
): boolean {
  const payload = asRecord(event.payload);
  const querySource = stringValue(payload.querySource);
  if (event.type === "model_request" && querySource === "session_title") {
    return true;
  }
  if (event.type === "model_network_status" && querySource === "session_title") {
// model_network_status cannot be hidden with the global suppress tag generated by the header:
// When the title request is retried, the retry event of the concurrent main request will also be swallowed, resulting in the retry count not being visible in the lower right corner of the app input box.
//The status event takes querySource, only hides the title and generates its own network status.
    return true;
  }
  if (event.type === "model_complete" && querySource === "session_title") {
    return true;
  }
  return false;
}

function mapProtocolSessionEvent(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
  event: SessionEvent,
  deliveryKind?: ZCodeDeliveryKind,
  options: {
    toolInputTransmissions?: ZCodeProtocolToolInputTransmissionState;
  } = {},
): ZCodeSessionEvent | null {
  if (shouldHideProtocolSessionEvent(record, event)) {
    return null;
  }
  if (!shouldExposeSessionEventToProtocol(event)) {
    const payload = asRecord(event.payload);
    context.logger?.debug("ZCode Protocol session event filtered", {
      event: "zcode_protocol.session_event.filtered",
      eventId: String(event.id),
      module: "bootstrap.zcode_protocol",
      payloadKind: stringValue(payload.kind),
      sessionEventSequenceNumber: event.sequenceNumber,
      sessionEventType: event.type,
      sessionId: String(event.sessionId),
      turnId: event.turnId ? String(event.turnId) : undefined,
    });
    return null;
  }
// The sequenceNumber of eventStore is the internal full ledger, including the ones filtered by the protocol.
// tool_input_delta and other high-frequency model intermediate states. The seq of ZCode Protocol is the recovery key of UI/replay.
// Must be re-numbered continuously according to the "actual visible event stream of the protocol", and the internal sequenceNumber cannot be directly exposed.
// The subagent mirror/live-only event with sequenceNumber=0 does not enter the parent eventStore and must
// Allocate independent seq according to eventId, and cannot reuse the same 0 mapping.
  const protocolSeq = assignProtocolEventSeq(record, event, deliveryKind);
  const mappedEvent = mapSessionEventForProtocol(event, deliveryKind, {
    seq: protocolSeq,
  });
  if (!mappedEvent) {
    return null;
  }
  return omitDuplicateScheduledToolInput(
    event,
    mappedEvent,
    options.toolInputTransmissions ?? getProtocolToolInputTransmissionState(record, deliveryKind),
  );
}

async function readProtocolSessionEvents(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
  deliveryKind?: ZCodeDeliveryKind,
  options: { afterSeq?: number; limit?: number } = {},
): Promise<ZCodeSessionEvent[]> {
  const events = await record.eventStore.getEvents(record.app.sessionId as SessionId);
  const visibleEvents: ZCodeSessionEvent[] = [];
  const toolInputTransmissions: ZCodeProtocolToolInputTransmissionState = {
    streamedToolCallIdsWithInput: new Set(),
  };
  let pendingStreamingDeltaBatch: ProtocolStreamingDeltaBatch | undefined;
  const flushedFirstDeltaBatchKeys = new Set<string>();
  const lastFlushAtByBatchKey = new Map<string, number>();
  const pushMappedEvent = (event: SessionEvent): void => {
    const mappedEvent = mapProtocolSessionEvent(context, record, event, deliveryKind, {
      toolInputTransmissions,
    });
    if (!mappedEvent) return;
    if (options.afterSeq !== undefined && mappedEvent.seq <= options.afterSeq) return;
    visibleEvents.push(mappedEvent);
  };
  const flushPendingStreamingDeltaBatch = (): void => {
    if (!pendingStreamingDeltaBatch) {
      return;
    }
    rememberProtocolStreamingDeltaBatchFlush(lastFlushAtByBatchKey, pendingStreamingDeltaBatch);
    pushMappedEvent(materializeProtocolStreamingDeltaBatch(pendingStreamingDeltaBatch));
    pendingStreamingDeltaBatch = undefined;
  };
  const resetPendingStreamingDeltaBatch = (): void => {
    flushPendingStreamingDeltaBatch();
    flushedFirstDeltaBatchKeys.clear();
    lastFlushAtByBatchKey.clear();
  };
  for (const event of events) {
    if (
      shouldHideProtocolSessionEvent(record, event) ||
      !shouldExposeSessionEventToProtocol(event)
    ) {
      resetPendingStreamingDeltaBatch();
      pushMappedEvent(event);
      continue;
    }
    const deltaInfo = readStreamingDeltaBatchableEvent(event);
    if (deltaInfo) {
      if (
        pendingStreamingDeltaBatch &&
        pendingStreamingDeltaBatch.batchKey !== deltaInfo.batchKey
      ) {
        flushPendingStreamingDeltaBatch();
      }
      pendingStreamingDeltaBatch = mergeProtocolStreamingDeltaBatch(
        pendingStreamingDeltaBatch,
        event,
        deltaInfo,
      );
      if (deltaInfo.shouldFlushFirstDelta && !flushedFirstDeltaBatchKeys.has(deltaInfo.batchKey)) {
        flushPendingStreamingDeltaBatch();
        flushedFirstDeltaBatchKeys.add(deltaInfo.batchKey);
        continue;
      }
      if (
        pendingStreamingDeltaBatch.delta.length >= pendingStreamingDeltaBatch.maxChars ||
        shouldFlushProtocolStreamingDeltaBatchForInterval(
          pendingStreamingDeltaBatch,
          lastFlushAtByBatchKey,
        )
      ) {
        flushPendingStreamingDeltaBatch();
      }
      continue;
    }
    resetPendingStreamingDeltaBatch();
    pushMappedEvent(event);
  }
  flushPendingStreamingDeltaBatch();
  return options.limit ? visibleEvents.slice(-options.limit) : visibleEvents;
}

async function getProtocolEventSeq(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
  deliveryKind?: ZCodeDeliveryKind,
): Promise<number> {
  await readProtocolSessionEvents(context, record, deliveryKind);
  return getProtocolEventSequenceState(record, deliveryKind).lastSeq;
}

function logProtocolSessionEventSent(
  context: ZCodeProtocolAgentServerContext,
  sourceEvent: SessionEvent,
  mappedEvent: ZCodeSessionEvent,
): void {
  const payload =
    typeof mappedEvent.payload === "object" && mappedEvent.payload !== null
      ? (mappedEvent.payload as Record<string, unknown>)
      : {};
  const protocolMessage = {
    method: "session/event",
    params: mappedEvent,
  };
  context.logger?.debug("ZCode Protocol session event sent", {
    deliveryKind: mappedEvent.deliveryKind,
    event: "zcode_protocol.session_event.sent",
    eventId: mappedEvent.eventId,
    method: "session/event",
    module: "bootstrap.zcode_protocol",
    payloadKeys: Object.keys(payload).sort(),
    payloadKind: typeof payload.kind === "string" ? payload.kind : undefined,
    payloadSummary: summarizeProtocolPayload(payload),
    protocolMessageBytes: measureJsonBytes(protocolMessage),
    protocolPayloadBytes: measureJsonBytes(mappedEvent.payload),
    protocolEventType: mappedEvent.type,
    protocolSeq: mappedEvent.seq,
    sessionEventSequenceNumber: sourceEvent.sequenceNumber,
    sessionEventType: sourceEvent.type,
    sessionId: mappedEvent.sessionId,
    turnId: mappedEvent.turnId,
  });
}

function summarizeProtocolPayload(payload: Record<string, unknown>): Record<string, unknown> {
  const summary: Record<string, unknown> = {};
  copyStringFields(summary, payload, [
    "action",
    "anchorId",
    "assistantMessageId",
    "inputId",
    "kind",
    "queryId",
    "reason",
    "requestId",
    "resultPartId",
    "source",
    "status",
    "taskId",
    "terminalId",
    "toolCallId",
    "toolName",
  ]);
  copyBooleanFields(summary, payload, ["concurrentSafe", "destructive", "done", "readOnly"]);
  copyNumberFields(summary, payload, [
    "durationMs",
    "elapsedMs",
    "outputBytes",
    "stderrBytes",
    "stdoutBytes",
    "tokenCount",
    "toolCallCount",
  ]);
  copyByteLength(summary, payload, "content", "contentBytes");
  copyByteLength(summary, payload, "delta", "deltaBytes");
  copyByteLength(summary, payload, "input", "inputBytes");
  copyByteLength(summary, payload, "outputTail", "outputTailBytes");
  copyByteLength(summary, payload, "previousTarget", "previousTargetBytes");
  copyByteLength(summary, payload, "response", "responseBytes");
  copyByteLength(summary, payload, "result", "resultBytes");
  copyByteLength(summary, payload, "stderrTail", "stderrTailBytes");
  copyByteLength(summary, payload, "stdoutTail", "stdoutTailBytes");
  copyByteLength(summary, payload, "target", "targetBytes");
  copyObjectKeySummary(summary, payload, "input", "input");
  copyObjectKeySummary(summary, payload, "requestHeaders", "requestHeader");
  copyObjectKeySummary(summary, payload, "responseHeaders", "responseHeader");
  copyObjectKeySummary(summary, payload, "result", "result");
  copySmallObject(summary, payload, "usage");
  copyContextUsageBreakdownSummary(summary, payload);
  return summary;
}

function copyStringFields(
  target: Record<string, unknown>,
  source: Record<string, unknown>,
  keys: string[],
): void {
  for (const key of keys) {
    const value = source[key];
    if (typeof value === "string" && value.length > 0) {
      target[key] = value;
    }
  }
}

function copyBooleanFields(
  target: Record<string, unknown>,
  source: Record<string, unknown>,
  keys: string[],
): void {
  for (const key of keys) {
    const value = source[key];
    if (typeof value === "boolean") {
      target[key] = value;
    }
  }
}

function copyNumberFields(
  target: Record<string, unknown>,
  source: Record<string, unknown>,
  keys: string[],
): void {
  for (const key of keys) {
    const value = source[key];
    if (typeof value === "number" && Number.isFinite(value)) {
      target[key] = value;
    }
  }
}

function copyByteLength(
  target: Record<string, unknown>,
  source: Record<string, unknown>,
  sourceKey: string,
  targetKey: string,
): void {
  if (source[sourceKey] !== undefined) {
    target[targetKey] = measureJsonBytes(source[sourceKey]);
  }
}

function copyObjectKeySummary(
  target: Record<string, unknown>,
  source: Record<string, unknown>,
  sourceKey: string,
  targetPrefix: string,
): void {
  const value = source[sourceKey];
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return;
  }
  const keys = Object.keys(value).sort();
  target[`${targetPrefix}KeyCount`] = keys.length;
  target[`${targetPrefix}Keys`] = keys.slice(0, 20);
}

function copySmallObject(
  target: Record<string, unknown>,
  source: Record<string, unknown>,
  sourceKey: string,
): void {
  const value = source[sourceKey];
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return;
  }
  if (measureJsonBytes(value) <= 1024) {
    target[sourceKey] = value;
  }
}

function copyContextUsageBreakdownSummary(
  target: Record<string, unknown>,
  source: Record<string, unknown>,
): void {
  const value = source.contextUsageBreakdown;
  if (!Array.isArray(value)) {
    return;
  }
  target.contextUsageBreakdownCount = value.length;
  target.contextUsageBreakdownSources = value
    .map((item) => asRecord(item).source)
    .filter((source): source is string => typeof source === "string")
    .slice(0, 20);
}

function measureJsonBytes(value: unknown): number {
  try {
    return Buffer.byteLength(JSON.stringify(value) ?? "null", "utf8");
  } catch {
    return 0;
  }
}

function slugifyImportedSession(value: string): string {
  const slug = value
    .toLowerCase()
    .replace(/[^a-z0-9._-]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 120);
  return slug.length > 0 ? slug : "imported-session";
}

function isImportedHistoryMessage(
  message: MessageWithParts,
  source: NonNullable<ZCodeSessionCreateParams["importedHistory"]>["source"],
): boolean {
  const messageId = String(message.info.id);
  if (/^msg_import_\d+$/u.test(messageId) || messageId.startsWith("msg_claude-import-")) {
    return true;
  }
  const messageMetadata = asRecord((message.info as unknown as { metadata?: unknown }).metadata);
  if (messageMetadata.migrationSource === source) {
    return true;
  }
  return message.parts.some((part) => {
    const partMetadata = asRecord((part as unknown as { metadata?: unknown }).metadata);
    return partMetadata.migrationSource === source;
  });
}

async function removePreviousImportedSessionHistory(params: {
  sessionStore: NonNullable<ZCodeProtocolAgentServerContext["deps"]["sessionStore"]>;
  sessionId: SessionId;
  source: NonNullable<ZCodeSessionCreateParams["importedHistory"]>["source"];
}): Promise<number> {
  const existingMessages = await params.sessionStore.messages({
    sessionID: params.sessionId,
  });
  let removedCount = 0;
  for (const message of existingMessages) {
    if (!isImportedHistoryMessage(message, params.source)) {
      continue;
    }
// The old import version reuses global fixed IDs such as msg_import_0/msg_import_1.
// The second imported session will change and bind the message of the previous session through the upsert of saveMessage.
// and retain the old time_created, eventually causing the session string message, order to be reversed, or reverted to empty. Just clean before reimporting
// The migration message of the current session retains the real messages that the user continues to chat in the import session.
    await params.sessionStore.removeMessage({
      sessionID: params.sessionId,
      messageID: message.info.id,
    });
    removedCount += 1;
  }
  return removedCount;
}

async function persistImportedSessionHistory(params: {
  context: ZCodeProtocolAgentServerContext;
  record: ZCodeProtocolSessionRecord;
  sessionId: SessionId;
  createParams: ZCodeSessionCreateParams;
}): Promise<void> {
  const importedHistory = params.createParams.importedHistory;
  const sessionStore = params.context.deps.sessionStore;
  if (!importedHistory) return;
  if (!sessionStore) {
    throw new ProtocolRequestError(-32003, "Cannot import session history without session store");
  }

  const workspace = params.record.workspace;
  const workspaceIdentity = workspace.workspaceIdentity?.trim();
  const now = Date.now();
  const createdAt =
    importedHistory.createdAt ??
    (importedHistory.source === "claudeCode"
      ? importedHistory.messages[0]?.timestamp
      : undefined) ??
    now;
  const updatedAt =
    importedHistory.source === "claudeCode"
      ? (importedHistory.updatedAt ?? importedHistory.messages.at(-1)?.timestamp ?? createdAt)
      : createdAt;
// History import does not execute the model; message content is retained when unbound, and cannot require the current selection or forge the message source.
  const currentModel = optionalModelSelectionFromString(params.record.app.getModel());
  const providerId = currentModel?.providerId as ModelProviderId | undefined;
  const modelId = currentModel?.modelId as ModelId | undefined;

  if (importedHistory.source === "sharedContext") {
    if (!sessionStore.commitSharedContextImportBundle) {
      throw new ProtocolRequestError(
        -32003,
        "Shared context import requires atomic session storage",
      );
    }
    const messageId = createMessageId(`${params.sessionId}_shared_context`);
    const importedAt = importedHistory.createdAt ?? now;
    const contextId =
      importedHistory.provenance.contextId ?? `legacy-shared-context-${params.sessionId}`;
    const contextStatus = importedHistory.provenance.status ?? "pending";
    await sessionStore.commitSharedContextImportBundle({
      session: {
        id: params.sessionId,
        projectID: projectIdFromDirectory(workspace.workspacePath),
        workspaceID: workspaceIdentity as WorkspaceId | undefined,
        traceID: params.record.traceContext.traceId,
        slug: slugifyImportedSession(params.sessionId),
        directory: workspace.workspacePath,
        path: workspace.workspacePath,
        title: importedHistory.title,
        titleSource: "custom",
        version: params.context.deps.version ?? "0.0.0",
        permission: { mode: params.record.app.getMode() },
        time: { created: importedAt, updated: importedAt },
      },
      contextMessage: {
        info: {
          id: messageId,
          sessionID: params.sessionId,
          role: "user",
          time: { created: importedAt },
          agent: "zcode-agent",
// When merging new shared imports, the old model fields are still used, which not only references invalid variables, but also loses unbound semantics.
// Shares the structured selection contract with normal import; import does not require the model to be executable.
          ...(currentModel ? { modelSelection: currentModel } : {}),
          synthetic: true,
          source: "shared_context",
          visibility: "model-only",
          semantics: {
            origin: "import",
            kind: "shared_context",
            source: "conversation_share",
            uiVisibility: "hidden",
            providerVisibility: "visible",
            transcriptVisibility: "visible",
          },
          metadata: {
            shareId: importedHistory.provenance.shareId,
            contextId,
            sharedContextStatus: contextStatus,
          },
        },
        parts: [
          {
            id: createPartId(`${params.sessionId}_shared_context_text`),
            sessionID: params.sessionId,
            messageID: messageId,
            type: "text",
            text: importedHistory.markdown,
            time: { start: importedAt, end: importedAt },
            metadata: { sharedContext: true },
          },
        ],
      },
      provenance: {
// session_entry.id is the primary key of the whole database (see sqlite-session-store.ts for similar fixes)
// fork command fact), the old template only contains shareId. Import the same share into the second
// When working in workspace, the deduplication of (shareCode, workspaceKey) does not hit, and the markers are also on their own.
// There is no conflict in the workspace. The on conflict(id) of saveSessionEntry will be the first session's
// Provenance is bound to the new session, and the shared context record of the old session disappears silently.
// Verification: All reads are done through (session_id, type) + data.contextId, there is no reverse check based on id.
// Therefore, the coexistence of old and new IDs is safe and no data migration is required.
        id: `v4_shared_context_import:${params.sessionId}:${importedHistory.provenance.shareId}`,
        sessionID: params.sessionId,
        type: "v4/shared_context_import",
        time: { created: importedAt, updated: importedAt },
        data: {
          ...importedHistory.provenance,
          contextId,
          status: contextStatus,
        },
      },
    });
    return;
  }

  await sessionStore.createSession({
    id: params.sessionId,
    projectID: projectIdFromDirectory(workspace.workspacePath),
// The import session will resume immediately after it is created; the current identity must be downloaded first to avoid degradation to path isolation during recovery.
    workspaceID: workspaceIdentity as WorkspaceId | undefined,
    traceID: params.record.traceContext.traceId,
    slug: slugifyImportedSession(params.sessionId),
    directory: workspace.workspacePath,
    path: workspace.workspacePath,
    title: importedHistory.title?.trim() || "Imported session",
    titleSource: "custom",
    version: params.context.deps.version ?? "0.0.0",
    permission: {
      mode: params.record.app.getMode(),
    },
    time: {
      created: createdAt,
      updated: Math.max(createdAt, updatedAt),
    },
  });

  const removedMessageCount = await removePreviousImportedSessionHistory({
    sessionStore,
    sessionId: params.sessionId,
    source: importedHistory.source,
  });
  let lastUserMessageId: MessageId | undefined;
  let lastImportedMessageTimestamp = createdAt - 1;
  for (const [index, message] of importedHistory.messages.entries()) {
    const rawTimestamp = message.timestamp ?? createdAt + index;

// sessionStore reads messages sorted by time_created. If assistant time is earlier than user,
// The UI will display the import session with assistant at the top and user at the bottom. The import boundary is based on array order.
// Only converge the timestamps to monotonically increasing values ​​before writing to the library, leaving the message content and roles unchanged.
    const timestamp =
      rawTimestamp > lastImportedMessageTimestamp ? rawTimestamp : lastImportedMessageTimestamp + 1;
    lastImportedMessageTimestamp = timestamp;
    const messageId = createMessageId(`${params.sessionId}_import_${index}`);
    if (message.role === "user") {
      lastUserMessageId = messageId;
      await sessionStore.saveMessage({
        id: messageId,
        sessionID: params.sessionId,
        role: "user",
        time: { created: timestamp },
        agent: "zcode-agent",
        ...(currentModel ? { modelSelection: currentModel } : {}),
        metadata: { migrationSource: importedHistory.source },
      });
    } else {
      await sessionStore.saveMessage({
        id: messageId,
        sessionID: params.sessionId,
        role: "assistant",
        time: { created: timestamp, completed: timestamp },
        parentID:
          lastUserMessageId ?? createMessageId(`${params.sessionId}_import_parent_${index}`),
        ...(currentModel ? { modelId, providerId } : {}),
        mode: params.record.app.getMode(),
        agent: "zcode-agent",
        path: {
          cwd: workspace.workspacePath,
          root: workspace.workspacePath,
        },
        cost: 0,
        tokens: {
          input: 0,
          output: 0,
          reasoning: 0,
          cache: { read: 0, write: 0 },
        },
        finish: "stop",
      });
    }
    await sessionStore.savePart({
      id: createPartId(`${params.sessionId}_import_${index}_text`),
      sessionID: params.sessionId,
      messageID: messageId,
      type: "text",
      text: message.content,
      time: { start: timestamp, end: timestamp },
      metadata: { migrationSource: importedHistory.source },
    });
  }

  params.context.logger?.info("ZCode Protocol imported history persisted", {
    event: "zcode_protocol.session_import.persisted",
    messageCount: importedHistory.messages.length,
    module: "bootstrap.zcode_protocol",
    removedMessageCount,
    sessionId: params.sessionId,
    source: importedHistory.source,
    workspaceKey: workspace.workspaceKey,
    workspacePath: workspace.workspacePath,
  });
}

export async function createSession(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
  trace?: ZCodeProtocolTrace,
) {
  return createSessionWithProjection(context, rawParams, trace, async (record) => {
    const result = await snapshotWithDiagnostics(context, record);
    return {
      value: result.snapshot,
      phaseDurationsMs: result.phaseDurationsMs,
      messageCount: result.snapshot.messages.length,
    };
  });
}

/** Only records are required for V4 creation; do not force parsing of empty models for discarded legacy snapshots. */
export async function createSessionRecordForV4(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
) {
  return createSessionWithProjection(context, rawParams, undefined, async (record) => ({
    value: { sessionId: record.app.sessionId },
  }));
}

async function createSessionWithProjection<T>(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
  trace: ZCodeProtocolTrace | undefined,
  project: (
    record: ZCodeProtocolSessionRecord,
  ) => Promise<{ value: T; phaseDurationsMs?: SnapshotPhaseDurationsMs; messageCount?: number }>,
): Promise<T> {
  const params = parseParams(zcodeSessionCreateParamsSchema, rawParams);
  const startedAt = Date.now();
  if (params.sessionId && !params.importedHistory) {
// If ordinary session/create allows external specification of id, it will overwrite the active record in context.sessions.
// Causing running sessions to be taken over, runtime leaks, or subsequent setModel/sendPrompt routing misalignment.
    throw new ProtocolRequestError(
      -32602,
      "sessionId is only supported for imported history creates",
    );
  }
  const sessionId = (params.sessionId ?? createSessionId()) as SessionId;
  const workspace = params.workspace;
  context.logger?.info("ZCode Protocol session/create started", {
    event: "zcode_protocol.session_create.started",
    hasInitialModel: params.model !== undefined,
    hasInitialThoughtLevel: params.thoughtLevel !== undefined,
    inboundTraceId: trace?.traceId,
    module: "bootstrap.zcode_protocol",
    persistence: params.persistence,
    sessionId,
    status: "started",
    workspaceKey: workspace.workspaceKey,
    workspacePath: workspace.workspacePath,
  });
  const recordStartedAt = Date.now();
  const record = await materializeSessionRecord(
    context,
    { ...params, workspace },
    sessionId,
    false,
    { kind: "host" },
    trace,
  );
  const recordCreateDurationMs = Date.now() - recordStartedAt;
  context.assertServing?.();
  context.sessions.set(sessionId, record);
  let setInitialModelDurationMs: number | undefined;
  let setInitialThoughtLevelDurationMs: number | undefined;
  let snapshotDurationMs = 0;
  let snapshotPhaseDurationsMs: SnapshotPhaseDurationsMs | undefined;
  const initialModel = params.model;
  const initialThoughtLevel = params.thoughtLevel;
  try {
    await runSessionModelConfigMutation(record.app, async () => {
      if (initialModel) {
        const setInitialModelStartedAt = Date.now();
        await record.app.setModel(formatProtocolModelSelection(initialModel));
        setInitialModelDurationMs = Date.now() - setInitialModelStartedAt;
        record.stateRevision++;
      }
      const supportedInitialThoughtLevel = resolveSupportedAppThoughtLevel(
        record.app,
        initialThoughtLevel,
      );
      if (supportedInitialThoughtLevel) {
// The app has taken over the default model/thought depth persistence, and session/create will explicitly pass in thoughtLevel.
// Workspace Preferences must be read persistently: read-only Agent memory will lose the user's last selection after restart.
        const setInitialThoughtLevelStartedAt = Date.now();
        await record.app.setThoughtLevel(supportedInitialThoughtLevel);
        setInitialThoughtLevelDurationMs = Date.now() - setInitialThoughtLevelStartedAt;
        record.stateRevision++;
      } else if (initialThoughtLevel) {
// workspace default thoughtLevel may come from previous model. When a new session inherits another model,
// Unsupported gears cannot be forced into the runtime, otherwise Unsupported reasoning effort will be thrown during the creation phase.
        context.logger?.warn("ZCode Protocol session/create skipped unsupported thought level", {
          event: "zcode_protocol.session_create.thought_level_skipped",
          module: "bootstrap.zcode_protocol",
          requestedThoughtLevel: initialThoughtLevel,
          sessionId,
          supportedThoughtLevels: record.app.listThoughtLevels(),
          workspaceKey: workspace.workspaceKey,
          workspacePath: workspace.workspacePath,
        });
      }
    });
    if (params.importedHistory) {
//History import used to only write legacy snapshot, taskId is not the real protocol sessionId,
// setModel/sendPrompt will hit a runtime that does not exist. Here, the history is written to sessionStore and resumed during the creation period.
// Let the import result be a ZCode session that can be continued and cut between models from the first moment.
      await persistImportedSessionHistory({
        context,
        record,
        sessionId,
        createParams: params,
      });
      await record.app.resume();
      record.stateRevision++;
    }
    const snapshotStartedAt = Date.now();
    const createdSnapshotResult = await project(record);
    const createdSnapshot = createdSnapshotResult.value;
    snapshotPhaseDurationsMs = createdSnapshotResult.phaseDurationsMs;
    snapshotDurationMs = Date.now() - snapshotStartedAt;
    context.logger?.info("ZCode Protocol session/create completed", {
      durationMs: Date.now() - startedAt,
      event: "zcode_protocol.session_create.completed",
      hasInitialModel: initialModel !== undefined,
      hasInitialThoughtLevel: initialThoughtLevel !== undefined,
      messageCount: createdSnapshotResult.messageCount,
      module: "bootstrap.zcode_protocol",
      persistence: params.persistence,
      recordCreateDurationMs,
      rootTraceId: record.traceContext.traceId,
      sessionId,
      setInitialModelDurationMs,
      setInitialThoughtLevelDurationMs,
      snapshotPhaseDurationsMs,
      snapshotDurationMs,
      status: "completed",
      workspaceKey: workspace.workspaceKey,
      workspacePath: workspace.workspacePath,
    });
    return createdSnapshot;
  } catch (error) {
    context.logger?.warn("ZCode Protocol session/create failed", {
      durationMs: Date.now() - startedAt,
      errorMessage: error instanceof Error ? error.message : String(error),
      event: "zcode_protocol.session_create.failed",
      module: "bootstrap.zcode_protocol",
      persistence: params.persistence,
      recordCreateDurationMs,
      rootTraceId: record.traceContext.traceId,
      sessionId,
      setInitialModelDurationMs,
      setInitialThoughtLevelDurationMs,
      snapshotPhaseDurationsMs,
      snapshotDurationMs,
      status: "failed",
      workspaceKey: workspace.workspaceKey,
      workspacePath: workspace.workspacePath,
    });
// Draft creation will use the model last selected by the user as the starting seed.
// If the model has been deleted, the failed record cannot be left in context.sessions:
// Subsequent workspace status continues to read this semi-initialized app, causing model/thinking gears to be confused.
    record.unsubscribe?.();
// v4 channel: Events that have been ingested during the creation period will lazily build publishers and be recycled when they fail to be cleaned up.
// dispose is deleted before the registry, ensuring that session.removed can be pushed with workspaceId.
    context.v4Gateway?.disposeSession(sessionId);
    context.sessions.delete(sessionId);
    try {
      await record.app.close?.();
    } catch {
// Retain the original create failure reason; close is just the best-effort action to clean up the semi-initialized runtime.
    }
//The memory event store is released with the record.
    await record.eventStore.deleteSession(sessionId as SessionId).catch(() => undefined);
    throw error;
  }
}

interface ActivatedSessionForResume {
  record: ZCodeProtocolSessionRecord;
  knownSession?: SessionInfo;
/** Only used to start this V4 hydration, no record written or cross-request cache. */
  persistedMessages?: MessageWithParts[];
}

/**
* Only restores the session runtime life cycle and does not materialize any protocol representation.
 *
* V4 cold subscription used to reuse resumeSession. Although the return value will be directly discarded, it will still be a large history.
* Build a complete legacy snapshot. Runtime activation and legacy representation are two responsibilities; reserved here
* Original order and side effects before snapshot, let V4 then hydrate with its own durable projection.
 */
export async function activateSessionForResume(
  context: ZCodeProtocolAgentServerContext,
  params: ZCodeSessionResumeParams,
  options: { reusePersistedMessages?: boolean } = {},
): Promise<ActivatedSessionForResume> {
  const resumeStartedAt = Date.now();
  const activeBeforeWait = context.sessions.has(params.sessionId);
  context.logger?.debug("ZCode Protocol session resume started", {
    activeBeforeWait,
    event: "zcode_protocol.session.resume_started",
    hasSessionStore: Boolean(context.deps.sessionStore),
    module: "bootstrap.zcode_protocol",
    sessionId: params.sessionId,
  });
// Reactivate the gate: The session is in the process of capacity deactivation (app.close closing is not completed), wait first,
// Prevent old and new apps from interleaving reading and writing of the same session resource. Return immediately without in-flight.
  await context.sessionResidentPool?.waitForDeactivation(params.sessionId);
  const existing = context.sessions.get(params.sessionId);
  if (existing) {
    return { record: existing };
  }
  let session = await getPersistedSession(context, params.sessionId);
  if (!session) {
// Diagnosis: Cold recovery only gets here if the persistent record also does not exist; the error text alone cannot be reconciled
// The "runtime is not yet active" distinction of readSession, so the two-layer status and waiting time are placed together.
    context.logger?.warn("ZCode Protocol session resume found no persisted record", {
      activeBeforeWait,
      activeSessionCount: context.sessions.size,
      durationMs: Math.max(0, Date.now() - resumeStartedAt),
      event: "zcode_protocol.session.resume_persisted_missing",
      hasSessionStore: Boolean(context.deps.sessionStore),
      module: "bootstrap.zcode_protocol",
      sessionId: params.sessionId,
    });
    throw new ProtocolRequestError(
      zcodeProtocolErrorCodes.sessionUnavailable,
      `Session not found: ${params.sessionId}`,
    );
  }
  session = await repairLegacyRemoteSessionWorkspaceForResume(context, session);
  const workspace =
    params.workspace ??
    buildWorkspaceRef({
// V4 historical session cold subscription only passes sessionId; if the remote session only uses the directory here
// Rebuild the workspace, the identity will degenerate into the path key, and it cannot hit the one isolated by workspaceID.
// Provider registry eventually misjudges available models as unavailable and permanently triggers restoreWarning.
      workspaceIdentity: session.workspaceID,
      workspacePath: session.path ?? session.directory,
    });
  let persistedMessages = await readPersistedSessionMessages(context, params.sessionId);
  const mode = derivePersistedSessionMode(persistedMessages);
// Shell setting changes only take effect for new sessions; cold recovery must use the database that was dropped when creating
// Bash shell snapshot. runtime.resumeFromStore will read the snapshot; this is only responsible for not
//The current settings carried in the resume request are re-injected into the old session.
  const record = await materializeSessionRecord(
    context,
    {
      ...params,
      mode,
// The model is only restored by the App's one-way migration/current entry, and a selection cannot be constructed with a message or caller hint first.
      model: undefined,
      ...(session.parentID ? { parentSessionId: String(session.parentID) } : {}),
// resume has lost the persistent taskType, and createRecord has returned to the default
// "interactive". So the resumed workflow_child / subagent_child session passes
// The filter of isTaskListSessionType leaks into sessions-index through getSessionWorkspaceId.
// The desktop task list grows "workflow actor actor#N@k" fake task, and the task index synchronizer will also
// Issue session/resume to them repeatedly. The fork path always carries taskType, and it must also be carried here.
      taskType: session.taskType,
      workspace,
    },
    params.sessionId as SessionId,
    true,
    { kind: "host" },
    session.traceID ? { traceId: session.traceID } : undefined,
  );
// createRecord hardcodes createdAt/updatedAt as Date.now()——resume old session
// Will make sessions-index treat it as a "newly created" session (with the empty title before hydration, sidebar
// The original session disappears and "new task just appeared" appears). The actual time it takes for the recovery path to backfill the store.
  if (session.time?.created) record.createdAt = session.time.created;
  if (session.time?.updated) record.updatedAt = session.time.updated;
  context.assertServing?.();
  context.sessions.set(params.sessionId, record);
  const resumeResult = await runSessionModelConfigMutation(record.app, async () => {
    const result = options.reusePersistedMessages
      ? await record.app.resume({ persistedMessages })
      : await record.app.resume();
// Only used for first projection seed, contains valid model + empty bits; it is not a second executable runtime selection.
    record.restoredModelSelection = result.modelSelection;
    return result;
  });
  if (options.reusePersistedMessages && resumeResult.persistedMessagesReloadRequired) {
    persistedMessages = await readPersistedSessionMessages(context, params.sessionId);
  }
  context.logger?.info("ZCode Protocol session resume completed", {
    activeSessionCount: context.sessions.size,
    durationMs: Math.max(0, Date.now() - resumeStartedAt),
    event: "zcode_protocol.session.resume_completed",
    module: "bootstrap.zcode_protocol",
    persistedMessageCount: persistedMessages.length,
    sessionId: params.sessionId,
    traceId: record.traceContext.traceId,
  });
  return {
    knownSession: session,
    record,
    ...(options.reusePersistedMessages ? { persistedMessages } : {}),
  };
}

export async function resumeSession(context: ZCodeProtocolAgentServerContext, rawParams: unknown) {
  const params = parseParams(zcodeSessionResumeParamsSchema, rawParams);
  const activated = await activateSessionForResume(context, params);
  return await snapshot(context, activated.record, activated.knownSession);
}

async function repairLegacyRemoteSessionWorkspaceForResume(
  context: ZCodeProtocolAgentServerContext,
  session: SessionInfo,
): Promise<SessionInfo> {
  if (session.workspaceID || !context.deps.sessionStore?.repairLegacyRemoteSessionWorkspace) {
    return session;
  }
  const legacyWorkspaceDirectory = session.path ?? session.directory;
  if (session.directory !== legacyWorkspaceDirectory) return session;
  const legacyRemote = resolveLegacyRemoteWorkspace(context, legacyWorkspaceDirectory);
  if (!legacyRemote) return session;

  const repaired = await context.deps.sessionStore.repairLegacyRemoteSessionWorkspace({
    sessionID: session.id,
    projectID: projectIdFromDirectory(legacyRemote.workspacePath),
    legacyWorkspaceDirectory,
    workspaceID: legacyRemote.workspaceIdentity,
    workspacePath: legacyRemote.workspacePath,
  });
// After the atomic repair returns false, the old directory/path is still used to materialize the runtime, and finally the error cwd
//Execute tools in. The original persistent fact must be read here, and the memory path of the existing identity must not be repaired and pretended to have been dropped.
  const refreshed = await context.deps.sessionStore.getSession(session.id);
  const repairPersisted =
    refreshed?.workspaceID === legacyRemote.workspaceIdentity &&
    refreshed.directory === legacyRemote.workspacePath &&
    refreshed.path === legacyRemote.workspacePath;
  if (!repairPersisted) {
    context.logger?.warn("legacy remote session workspace repair was not persisted", {
      event: "zcode_protocol.session_resume.legacy_remote_workspace_repair_rejected",
      legacyWorkspaceDirectory,
      legacyWorkspaceIdentity: legacyRemote.workspaceIdentity,
      module: "bootstrap.zcode_protocol",
      repaired,
      sessionId: session.id,
      workspacePath: legacyRemote.workspacePath,
    });
    throw createCoreError(
      CoreErrorType.SessionCorrupted,
      "Legacy remote session workspace repair was not persisted",
      {
        context: {
          directory: refreshed?.directory ?? session.directory,
          path: refreshed?.path ?? session.path,
          reason: "legacy_remote_session_workspace_repair_rejected",
          sessionId: session.id,
          workspaceIdentity: legacyRemote.workspaceIdentity,
        },
        recoverable: true,
      },
    );
  }
  if (repaired) {
    context.logger?.info("legacy remote session workspace repaired", {
      event: "zcode_protocol.session_resume.legacy_remote_workspace_repaired",
      legacyWorkspaceDirectory,
      legacyWorkspaceIdentity: legacyRemote.workspaceIdentity,
      module: "bootstrap.zcode_protocol",
      sessionId: session.id,
      workspacePath: legacyRemote.workspacePath,
    });
  }
// Core resumeFromStore will re-read the library; NULL identity repair must be persisted first and cannot just change the bootstrap memory value.
  return refreshed;
}

function resolveLegacyRemoteWorkspace(
  context: ZCodeProtocolAgentServerContext,
  legacyWorkspaceDirectory: string,
): { workspaceIdentity: WorkspaceId; workspacePath: string } | null {
  const currentWorkspacePath = context.deps.cwd ?? process.cwd();
  const direct = parseRemoteWorkspaceIdentity(legacyWorkspaceDirectory);
  if (direct?.kind === "wsl" && direct.workspacePath === currentWorkspacePath) {
    return {
      workspaceIdentity: legacyWorkspaceDirectory as WorkspaceId,
      workspacePath: direct.workspacePath,
    };
  }

  const identityPrefix = currentWorkspacePath === "/" ? "/" : `${currentWorkspacePath}/`;
  if (!legacyWorkspaceDirectory.startsWith(identityPrefix)) return null;
  const embeddedIdentity = legacyWorkspaceDirectory.slice(identityPrefix.length);
  const embedded = parseRemoteWorkspaceIdentity(embeddedIdentity);
  if (
    embedded?.kind !== "wsl" ||
    `${identityPrefix}${embeddedIdentity}` !== legacyWorkspaceDirectory ||
    embedded.workspacePath !== currentWorkspacePath
  ) {
    return null;
  }

// Only accept the exact form directly spelled out by the current app-server cwd, and do not fuzzy search remote: prefix in any path.
  return {
    workspaceIdentity: embeddedIdentity as WorkspaceId,
    workspacePath: embedded.workspacePath,
  };
}

export async function listSessions(context: ZCodeProtocolAgentServerContext, rawParams: unknown) {
  const params = parseParams(zcodeSessionListParamsSchema, rawParams ?? {});
  const store = context.deps.sessionStore;
// Explicit ID queries the read-only persistent identity for the Host to repair the old index; the runtime cannot be activated to identify the child.
  const stored = (
    params.sessionIds
      ? await Promise.all(params.sessionIds.map((id) => store?.getSession(id as SessionId) ?? null))
      : store
        ? await store.listSessions({
            directory: params.workspace?.workspacePath,
            includeArchived: params.includeArchived,
            limit: params.limit ?? 50,
            taskTypes: [...TASK_LIST_SESSION_TYPES],
          })
        : []
  ).filter((session): session is SessionInfo => {
    if (!session) return false;
    return (
      (params.includeArchived || session.time.archived === undefined) &&
      (!params.workspace ||
        (session.workspaceID?.trim() || session.path || session.directory) ===
          (params.workspace.workspaceIdentity?.trim() || params.workspace.workspacePath))
    );
  });
  const storedIds = new Set(stored.map((session) => String(session.id)));
  const sessions = stored.map((session) =>
    mapSessionInfo({
      session,
      workspace:
        params.workspace ?? buildWorkspaceRef({ workspacePath: session.path ?? session.directory }),
    }),
  );
  if (params.sessionIds) return { sessions };
  for (const record of context.sessions.values()) {
    if (record.persistence === "deferred") continue;
    if (!isTaskListSessionType(record.taskType)) continue;
    if (storedIds.has(record.app.sessionId)) continue;
    if (params.workspace && params.workspace.workspaceKey !== record.workspace.workspaceKey)
      continue;
    sessions.push(
      mapSessionInfo({
        app: record.app,
        workspace: record.workspace,
        taskType: record.taskType,
        parentSessionId: record.parentSessionId,
      }),
    );
  }
  return { sessions };
}

export async function listSessionSubagents(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
  persistedMessages?: MessageWithParts[],
) {
  const params = parseParams(zcodeSessionSubagentsParamsSchema, rawParams ?? {});
  const store = context.deps.sessionStore;
  const liveParent = context.sessions.get(params.sessionId);
  if (!store) {
    return {
      revision: liveParent?.stateRevision ?? 0,
      childSessionIds: [],
      running: [],
      ended: { total: 0, items: [] },
    };
  }

  const parentSession = await store.getSession(params.sessionId as SessionId);
  if (!parentSession) {
// Diagnosis: hydrate will reuse subtask seed reading; if the task index/old ACP task has an invalid ID remaining,
// Here, "persistent record does not exist" will be packaged as v4.hydrate. The calling phase must be recorded instead of just looking at the error text.
    context.logger?.warn("ZCode Protocol session subagents has no persisted parent", {
      activeSessionCount: context.sessions.size,
      activeSession: Boolean(liveParent),
      event: "zcode_protocol.session.persisted_missing",
      module: "bootstrap.zcode_protocol",
      operation: "session_subagents",
      sessionId: params.sessionId,
    });
    throw new ProtocolRequestError(
      zcodeProtocolErrorCodes.sessionUnavailable,
      `Session not found: ${params.sessionId}`,
    );
  }
  const messages = persistedMessages ?? (await store.messages({ sessionID: parentSession.id }));
  const parentEvents = liveParent
    ? await liveParent.eventStore.getEvents(parentSession.id).catch(() => [])
    : [];
  const childSessionIds = collectSubagentChildSessionIds(parentSession, messages, parentEvents);
  const childEntries = await Promise.all(
    childSessionIds.map(async (childSessionId) => {
      const childSession = await store.getSession(childSessionId as SessionId);
      if (!childSession || childSession.taskType !== "subagent_child") return null;
      const childMessages = await store.messages({
        sessionID: childSession.id,
      });
      const liveChild = context.sessions.get(childSessionId);
      const childProjection = liveChild
        ? await liveChild.app.runtime.getProjection().catch(() => undefined)
        : undefined;
      return { childMessages, childProjection, childSession, childSessionId };
    }),
  );
  const persistedChildren = childEntries.filter(
    (entry): entry is NonNullable<typeof entry> => entry !== null,
  );
  const parentProjection = liveParent
    ? await liveParent.app.runtime.getProjection().catch(() => undefined)
    : undefined;
  const projection = projectSessionSubagents({
    revision: liveParent?.stateRevision ?? parentSession.time.updated,
    parentSession,
    messages,
    childSessionsById: new Map(
      persistedChildren.map((entry) => [entry.childSessionId, entry.childSession]),
    ),
    childMessagesById: new Map(
      persistedChildren.map((entry) => [entry.childSessionId, entry.childMessages]),
    ),
    childProjectionsById: new Map(
      persistedChildren.flatMap((entry) =>
        entry.childProjection ? [[entry.childSessionId, entry.childProjection]] : [],
      ),
    ),
    ...(parentProjection ? { parentProjection } : {}),
    ...(parentEvents.length > 0 ? { parentEvents } : {}),
  });
  const ended = paginateEndedSubagents(projection.ended, {
    cursor: params.endedCursor,
    limit: params.endedLimit,
  });
  return {
    revision: projection.revision,
    childSessionIds: persistedChildren.map((entry) => entry.childSessionId),
    running: projection.running,
    ended: {
      total: projection.ended.length,
      items: ended.items,
      ...(ended.nextCursor ? { nextCursor: ended.nextCursor } : {}),
    },
  };
}

const APP_USAGE_RANGE_DAYS: Record<string, number> = { "7d": 7, "30d": 30 };

export async function getUsageStats(context: ZCodeProtocolAgentServerContext, rawParams: unknown) {
  const params = parseParams(zcodeUsageStatsParamsSchema, rawParams ?? {});
  const timeZone = params.timeZone ?? "UTC";
  const until = Date.now();
  const tzOffsetMs = resolveTzOffsetMs(timeZone, until);
  const rangeDays = APP_USAGE_RANGE_DAYS[params.range] ?? 30;
  const since = params.range === "all" ? 0 : until - rangeDays * 86_400_000;
  const buildOptions = {
    range: params.range,
    timeZone,
    tzOffsetMs,
    generatedAt: until,
    since,
    until,
  };

// SessionStorePort and UsageStorePort are separate interfaces, but the actual store implements both at the same time;
// Follow the runtime narrowing of core/usage-observability to access read-only aggregate methods.
  const usageStore = context.deps.sessionStore as Partial<UsageStorePort> | undefined;
  if (!usageStore?.queryAppUsage) {
// No usage store (should not happen): Return an empty snapshot instead of throwing an error, so that the UI can display an empty state.
    return buildAppUsageSnapshot(
      {
        totals: {
          totalTokens: 0,
          inputTokens: 0,
          outputTokens: 0,
          reasoningTokens: 0,
          cacheCreationTokens: 0,
          cacheReadTokens: 0,
          modelRequestCount: 0,
          modelErrorCount: 0,
          avgTimeToFirstTokenMs: null,
        },
        turnTotals: {
          totalSessions: 0,
          totalTurns: 0,
          avgTurnDurationMs: null,
          longestSessionMs: 0,
        },
        toolTotals: { toolCallCount: 0, toolErrorCount: 0 },
        models: [],
        tools: [],
        days: [],
        dayModels: [],
      },
      buildOptions,
    );
  }

  const result = await usageStore.queryAppUsage({ since, until, tzOffsetMs });
  return buildAppUsageSnapshot(result, buildOptions);
}

export async function readSession(context: ZCodeProtocolAgentServerContext, rawParams: unknown) {
  const params = parseParams(zcodeSessionReadParamsSchema, rawParams);
  const record = requireSession(context, params.sessionId, {
    deliveryKind: params.deliveryKind,
    operation: "session_read",
  });
  record.deliveryKind = params.deliveryKind ?? record.deliveryKind;
  return await snapshot(context, record, undefined, {
    messageLimit: params.messageLimit,
    modelAvailability: "current",
  });
}

/**
* Narrow read for V4 cold recovery usage: reuses the legacy snapshot's meter calculations, but does not map the full protocol snapshot.
 */
export async function readSessionContextUsage(
  context: ZCodeProtocolAgentServerContext,
  sessionId: string,
  persistedMessages?: MessageWithParts[],
) {
  const record = context.sessions.get(sessionId);
  if (!record) return undefined;
  const [resolvedPersistedMessages, session, events] = await Promise.all([
    persistedMessages ?? readSessionMessages(context, record),
    getPersistedSession(context, sessionId),
    record.eventStore.getEvents(sessionId as SessionId),
  ]);
// Maintain the order of accessing legacy buildSessionSnapshot: fix the persistent facts first, then read the runtime
// projection. If the first subscription during operation coincides with ModelComplete/rewind, the projection cannot be changed due to narrow reading.
// Advance to the parallel stage to create new cross-water level combinations.
  const projection = await record.app.runtime.getProjection();
  const messages = projectActiveSessionMessages(
    resolvedPersistedMessages,
    session,
    events.filter((event) => event.type === SessionEventType.RewindTriggered),
  );
  return resolveSessionContextUsage({
    messages,
    persistedContextUsageBreakdownEvents: events,
    projection,
  });
}

export async function readMessages(context: ZCodeProtocolAgentServerContext, rawParams: unknown) {
  const params = parseParams(zcodeSessionMessagesParamsSchema, rawParams);
  const record = requireSession(context, params.sessionId);
  const allMessages = await readActiveSessionMessages(context, record);
  const afterMessageIndex = params.afterMessageId
    ? allMessages.findIndex((message) => String(message.info.id) === params.afterMessageId)
    : -1;
  const messages = afterMessageIndex >= 0 ? allMessages.slice(afterMessageIndex + 1) : allMessages;
  return {
    messages: params.limit ? messages.slice(-params.limit) : messages,
  };
}

export async function readEvents(context: ZCodeProtocolAgentServerContext, rawParams: unknown) {
  const params = parseParams(zcodeSessionEventsParamsSchema, rawParams);
  const record = requireSession(context, params.sessionId);
  return {
    events: await readProtocolSessionEvents(context, record, record.deliveryKind, {
      afterSeq: params.afterSeq,
      limit: params.limit,
    }),
  };
}

export async function subscribeSession(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
) {
  const params = parseParams(zcodeSessionSubscribeParamsSchema, rawParams);
  const record = requireSession(context, params.sessionId);
  record.deliveryKind = params.deliveryKind;
// Compatibility protection: old replayable streams don't have unsubscribe RPCs. Only the real subscribe is recorded here.
// Do not reuse the deliveryKind that will be written by session/read to avoid normal reading from permanently preventing resident recycling.
  record.legacyStreamSubscribed = true;
  const events =
    params.afterSeq === undefined
      ? []
      : await readProtocolSessionEvents(context, record, record.deliveryKind, {
          afterSeq: params.afterSeq,
        });
  return {
    eventSeq: await getProtocolEventSeq(context, record, record.deliveryKind),
    events,
    sessionId: params.sessionId,
    snapshot: params.includeSnapshot
      ? await snapshot(context, record, undefined, {
          modelAvailability: "current",
        })
      : undefined,
  };
}

export async function sendPrompt(context: ZCodeProtocolAgentServerContext, rawParams: unknown) {
  const params = parseParams(zcodeSessionSendParamsSchema, rawParams);
  const record = requireSession(context, params.sessionId);
// The legacy input portal must also remain read-only for subagents and cannot bypass V4 user input access.
  if (record.taskType === "subagent_child") {
    throw new ProtocolRequestError(-32010, "Subagent sessions are read-only", {
      reasonCode: "guard.subagentReadOnly",
    });
  }
  assertExpectedRevision(record, params.expectedRevision);
  assertExpectedProviderRevision(context, record, params.expectedProviderRevision);
  if (record.activeAbortController) {
    context.logger?.warn("ZCode Protocol session/send rejected: active prompt exists", {
      inputId: params.inputId,
      queryId: params.queryId,
      sessionId: params.sessionId,
      textLength: params.content.length,
      workspacePath: record.workspace.workspacePath,
    });
    throw new ProtocolRequestError(-32010, "A prompt is already running for this session");
  }
  if (record.restoreWarning) {
    throw new ProtocolRequestError(-32031, record.restoreWarning.message, {
      code: record.restoreWarning.type,
      sessionId: record.app.sessionId,
      workspace: record.workspace,
    });
  }
  await ensureSessionModelAvailableForNextTurn(context, record);
  if (record.persistence === "deferred") {
// The draft session before sending does not enter two copies of sqlite; when the user actually sends the first message,
// Only then promote it to a normal session, allowing subsequent list/syncer/task index to be processed according to real tasks.
    record.persistence = "immediate";
  }
  const abortController = new AbortController();
  record.activeAbortController = abortController;
  context.logger?.info("ZCode Protocol session/send accepted", {
    attachmentCount: params.attachments?.length ?? 0,
    inputId: params.inputId,
    queryId: params.queryId,
    sessionId: params.sessionId,
    textLength: params.content.length,
    workspacePath: record.workspace.workspacePath,
  });
  const inputId = params.inputId ?? (params.modelSelection ? crypto.randomUUID() : undefined);
// The old attachment payload is also fixed this time by the same canonical intent, without changing the Session first. Only borrowing here
// Public metadata constructor (non-V4 ACK/replay entry); the sequence number edge of no V4 admission has an existing value of 0.
  const intent =
    params.modelSelection && inputId
      ? inputIntentMetadata(
          {
            commandId: inputId,
            clientId: "legacy-session-send",
            sessionId: params.sessionId,
            type: "sendText",
            payload: {},
            issuedAt: Date.now(),
          },
          {
            text: params.content,
            requestedDelivery: "startNow",
            modelSelection: params.modelSelection,
          },
        )
      : undefined;
  void runWithSessionResidencyFinalization(record, () =>
    runPromptTurnInBackground(context, record, {
      abortController,
      attachments: params.attachments,
      browserAmbientContext: params.browserAmbientContext,
      inputId,
      intent,
      modelExecution: params.modelExecution
        ? createModelExecutionContext(params.modelExecution)
        : undefined,
      queryId: (params.queryId ?? inputId) as QueryId | undefined,
      content: params.content,
      ...(params.automationId
        ? { automationId: params.automationId }
        : params.offPeakTaskId
          ? {
              offPeakTaskId: params.offPeakTaskId,
              ...(params.offPeakRunType ? { offPeakRunType: params.offPeakRunType } : {}),
            }
          : {}),
      toolDenylist: params.toolDenylist,
    }),
  ).catch(() => {
// Background turn errors will be reported through status/event stream degradation; this is a safety net to prevent unhandled rejection in the protocol process.
  });
// session/send is just a protocol request to "submit user input" and cannot wait for the entire round of model generation to be completed synchronously.
// Waiting for the first token/full round synchronously will trigger host timeout for requests exceeding 30 seconds, and block subsequent session/resume, list and other protocol messages.
// ACK immediately after receiving the input here, and the background turn continues to push the progress through session event/state.updated.
  return afterPromptAccepted(context, record, "prompt_started");
}

export async function compactSession(context: ZCodeProtocolAgentServerContext, rawParams: unknown) {
  const params = parseParams(zcodeSessionCompactParamsSchema, rawParams);
  const record = requireSession(context, params.sessionId);
  assertExpectedRevision(record, params.expectedRevision);
  const activeTurn = record.app.runtime.getActiveTurnInfo();
  if (activeTurn?.kind === "compact") {
// Repeated `/compact` is a context maintenance request for the same session; when received again during compression, it should be discarded directly.
// Cannot enter the normal prompt/steer queue, nor can it be rendered into a failed compression bar.
    return {
      response: "",
      snapshot: await snapshot(context, record, undefined, {
        modelAvailability: "current",
      }),
      compact: {
        state: "already_running" as const,
        ...(params.inputId ? { inputId: params.inputId } : {}),
      },
    };
  }
  ensureNoActiveTurn(record, "Cannot compact while a prompt is running");
  if (record.restoreWarning) {
    throw new ProtocolRequestError(-32031, record.restoreWarning.message, {
      code: record.restoreWarning.type,
      sessionId: record.app.sessionId,
      workspace: record.workspace,
    });
  }
  await ensureSessionModelAvailableForNextTurn(context, record);
  const instructions = params.instructions?.trim();
  const command = instructions ? `/compact ${instructions}` : "/compact";
  const abortController = new AbortController();
// The real model request for compact is executed in the background, but Stop is still interrupted via record.activeAbortController.
// If the controller is not registered here, the model request during compression will continue to end naturally.
  record.activeAbortController = abortController;
  void runWithSessionResidencyFinalization(record, () =>
    runCompactTurnInBackground(context, record, {
      abortController,
      command,
      inputId: params.inputId,
    }),
  ).catch(() => {
// Background compact errors will be reported through compact timeline/state.updated downgrade; here is a safety net to prevent unhandled rejection.
  });
  record.stateRevision++;
  record.updatedAt = Date.now();
  const acceptedSnapshot = await snapshot(context, record, undefined, {
    modelAvailability: "current",
  });
  emitStateUpdated(context, record, "compact_started", { status: "running" });
  return {
    response: "",
    snapshot: acceptedSnapshot,
    compact: {
      state: "accepted" as const,
      ...(params.inputId ? { inputId: params.inputId } : {}),
    },
  };
}

async function runCompactTurnInBackground(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
  params: {
    abortController: AbortController;
    command: string;
    inputId?: string;
  },
): Promise<void> {
  const startedAt = Date.now();
  let mutationReason = "session_compacted";
  context.logger?.info("ZCode Protocol background compact started", {
    inputId: params.inputId,
    sessionId: record.app.sessionId,
    workspacePath: record.workspace.workspacePath,
  });
  try {
    await record.app.submitPrompt(params.command, {
      abortSignal: params.abortController.signal,
      inputId: params.inputId,
    });
    context.logger?.info("ZCode Protocol background compact completed", {
      durationMs: Date.now() - startedAt,
      inputId: params.inputId,
      sessionId: record.app.sessionId,
      workspacePath: record.workspace.workspacePath,
    });
  } catch (error) {
    mutationReason = params.abortController.signal.aborted
      ? "session_compact_cancelled"
      : "session_compact_failed";
    context.logger?.warn("ZCode Protocol background compact failed", {
      durationMs: Date.now() - startedAt,
      error: error instanceof Error ? error.message : String(error),
      inputId: params.inputId,
      sessionId: record.app.sessionId,
      workspacePath: record.workspace.workspacePath,
    });
  } finally {
    if (record.activeAbortController === params.abortController) {
// After compacting, active lock must be released first before broadcasting status; otherwise queued prompt
// or subsequent `/compact` will briefly hit the old controller at the ready boundary.
      record.activeAbortController = undefined;
    }
  }
  await afterStateMutation(context, record, mutationReason);
}

export async function goalSession(context: ZCodeProtocolAgentServerContext, rawParams: unknown) {
  const params = parseParams(zcodeSessionGoalParamsSchema, rawParams);
  const record = requireSession(context, params.sessionId);
  assertExpectedRevision(record, params.expectedRevision);
// /goal is the target state written, not a normal prompt; allowing execution during running except pause will bypass the front-end queue and pollute the current turn.
// pause is a control surface command for the user to explicitly stop the active goal, and must be able to interrupt the current assistant turn or verifier.
  const goalPauseMayInterruptActiveTurn = params.action === "pause";
  if (!goalPauseMayInterruptActiveTurn) {
    ensureNoActiveTurn(record, "Cannot manage goals while a prompt is running");
  }

  if (
    !record.app.readTarget ||
    !record.app.setTarget ||
    !record.app.updateTargetStatus ||
    !record.app.clearTarget
  ) {
    return {
      response: "Goal management is not available in this client.",
      snapshot: await snapshot(context, record),
      startedTurn: false,
    };
  }

  if (params.action === "show") {
    return {
      response: formatGoalSummary(await record.app.readTarget()),
      snapshot: await snapshot(context, record),
      startedTurn: false,
    };
  }

  if (params.action === "pause") {
    const activeAbortController = record.activeAbortController;
    const target = await record.app.updateTargetStatus("paused");
    if (target && activeAbortController) {
      context.logger?.info("ZCode Protocol goal pause aborting active turn", {
        inputId: params.inputId,
        sessionId: params.sessionId,
        targetId: target.targetID,
        workspacePath: record.workspace.workspacePath,
      });
      activeAbortController.abort(new Error("ZCode Protocol goal paused"));
    }
    const snapshotAfterGoal = await afterStateMutation(context, record, "goal_paused");
    return {
// pause is a state control action, and the UI has reflected the results through the target panel and runtime closure;
// Returning "Goal paused Objective..." will be rendered as a meaningless assistant bubble and expose the target text repeatedly.
      response: target ? "" : "No goal to pause.",
      snapshot: snapshotAfterGoal,
      startedTurn: false,
    };
  }

  if (params.action === "resume") {
    const target = await record.app.updateTargetStatus("active");
    if (!target) {
      return {
        response: "No goal to resume.",
        snapshot: await snapshot(context, record),
        startedTurn: false,
      };
    }
    return await continueGoalAfterChange(context, record, {
      inputId: params.inputId,
      reason: "goal_resumed",
      target,
      title: "Goal resumed",
    });
  }

  if (params.action === "clear") {
    const cleared = await record.app.clearTarget();
    const snapshotAfterGoal = await afterStateMutation(context, record, "goal_cleared");
    return {
      response: cleared ? "Goal cleared." : "No goal to clear.",
      snapshot: snapshotAfterGoal,
      startedTurn: false,
    };
  }

  const objective = params.objective?.trim() ?? "";
  if (objective.length === 0) {
    return {
      response:
        params.action === "replace"
          ? "Usage: /goal replace <objective>"
          : "Usage: /goal <objective>",
      snapshot: await snapshot(context, record),
      startedTurn: false,
    };
  }

// The `/goal new goal` in the App input box is a new goal explicitly submitted by the user; if there is already a goal, continue to request it
// replace will make the user think that the target has changed but the database still retains the old target. Here, repeated set is converged into replace semantics.
  const replacesExistingGoal =
    params.action === "replace" || Boolean(await record.app.readTarget());

  const target = await record.app.setTarget({ objective, status: "active" });
  return await continueGoalAfterChange(context, record, {
    inputId: params.inputId,
    reason: replacesExistingGoal ? "goal_replaced" : "goal_set",
    target,
    title: "Goal active",
  });
}

export async function forkSession(context: ZCodeProtocolAgentServerContext, rawParams: unknown) {
  const params = parseParams(zcodeSessionForkParamsSchema, rawParams);
  const record = requireSession(context, params.sessionId);
  assertExpectedRevision(record, params.expectedRevision);
  ensureNoActiveTurn(record, "Cannot fork while a prompt is running");
  const forkTarget = await resolveForkTarget(context, record, params.target);
  const parentMode = record.app.getMode();
  const parentModel = record.app.getModel();
  const parentThoughtLevel = record.app.getThoughtLevel();
  const fork = await record.app.forkFromCheckpoint({
    targetCheckpointId: forkTarget.targetCheckpointId,
    targetMessageId: forkTarget.targetMessageId,
  });
  return await registerForkedSession(context, record, fork, {
    runtimeConfig: {
      mode: parentMode,
      model: parentModel,
      thoughtLevel: parentThoughtLevel,
    },
    inheritLatestTarget: true,
  });
}

/**
* Only child records are registered after core copy is completed. V4 running fork reuses this host life cycle,
* But it will not enter the active-turn guard / workspace fork of forkSession; runtimeConfig can be fixed at the fork point.
 */
export async function registerForkedSession(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
  fork: Awaited<ReturnType<ZCodeProtocolSessionRecord["app"]["forkFromCheckpoint"]>>,
  options: {
    runtimeConfig: {
      mode: CollaborationMode;
      model: string;
      thoughtLevel?: string;
      followupMode?: "queue" | "guide";
    };
    inheritLatestTarget: boolean;
  },
) {
  const parentMode = options.runtimeConfig.mode;
  const parentModel = options.runtimeConfig.model;
  const parentThoughtLevel = options.runtimeConfig.thoughtLevel;
  const parentFollowupMode = options.runtimeConfig.followupMode;
  const forkedSession = await getPersistedSession(context, fork.forkedSessionId);
  if (!forkedSession) {
    throw new Error(`Persisted child session not found: ${fork.forkedSessionId}`);
  }
  const forkRecord = await materializeSessionRecord(
    context,
    {
      sessionId: fork.forkedSessionId,
      mode: parentMode,
// Copy history is also allowed for unbound parent sessions; default models cannot be selected or empty strings parsed due to forking.
      model: optionalModelSelectionFromString(parentModel),
      ...(fork.parentSessionId ? { parentSessionId: fork.parentSessionId } : {}),
      taskType: forkedSession.taskType,
      workspace: record.workspace,
    },
    fork.forkedSessionId as SessionId,
    true,
    { kind: "inherit", parent: record },
  );
  context.assertServing?.();
  context.sessions.set(fork.forkedSessionId, forkRecord);
  await runSessionModelConfigMutation(forkRecord.app, async () => {
// fork is a branch of the parent session's running state; if only the message is copied, the new record will be restored from the workspace default value.
// So the current model/mode after forking will be overwritten by the latest default settings. Here the parent session settings are explicitly inherited before resume.
    if (forkRecord.app.getMode() !== parentMode) {
      await forkRecord.app.setMode(parentMode);
      forkRecord.stateRevision++;
    }
    if (parentModel && forkRecord.app.getModel() !== parentModel) {
      await forkRecord.app.setModel(parentModel);
      forkRecord.stateRevision++;
    }
    if (parentThoughtLevel && forkRecord.app.getThoughtLevel() !== parentThoughtLevel) {
      await forkRecord.app.setThoughtLevel(parentThoughtLevel);
      forkRecord.stateRevision++;
    }
    if (parentFollowupMode && parentFollowupMode !== "queue") {
      await forkRecord.app.setFollowupMode(parentFollowupMode);
      forkRecord.stateRevision++;
    }
  });
  if (options.inheritLatestTarget) {
    await inheritForkedSessionTarget(context, record, forkRecord, fork.forkedSessionId);
  }
  await forkRecord.app.resume();
  return {
    forkedSessionId: fork.forkedSessionId,
    parentSessionId: fork.parentSessionId,
    targetMessageId: fork.targetMessageId,
    targetCheckpointId: fork.targetCheckpointId,
    response: fork.response,
    snapshot: await snapshot(context, forkRecord, forkedSession),
  };
}

async function inheritForkedSessionTarget(
  context: ZCodeProtocolAgentServerContext,
  parentRecord: ZCodeProtocolSessionRecord,
  forkRecord: ZCodeProtocolSessionRecord,
  forkedSessionId: string,
) {
  const parentTarget = await parentRecord.app.readTarget();
  if (!parentTarget || !context.deps.sessionStore) {
    return;
  }
  const existingChildTarget = await context.deps.sessionStore.readTarget({
    sessionID: forkedSessionId as SessionId,
  });
  if (existingChildTarget) {
    return;
  }
  if (!context.deps.sessionStore.cloneTargetForFork) {
    return;
  }

// core fork now copies goal state; bootstrap only retains the backend of the old app/fake app.
// You cannot setTarget to create a new targetId, otherwise the verifier timeline will be disconnected from the copied transcript.
  await context.deps.sessionStore.cloneTargetForFork({
    sessionID: forkedSessionId as SessionId,
    source: parentTarget,
    status: parentTarget.status,
  });
  forkRecord.stateRevision++;
  forkRecord.updatedAt = Date.now();
}

async function runPromptTurnInBackground(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
  params: {
    abortController: AbortController;
    intent?: SendInputOptions["intent"];
    modelExecution?: SendInputOptions["modelExecution"];
    attachments?: unknown[];
    browserAmbientContext?: {
      tabCount: number;
      currentUrl?: string;
    };
    inputId?: string;
    queryId?: QueryId;
    content: string;
    toolDenylist?: readonly string[];
  } & TurnBackgroundAttribution,
): Promise<void> {
  const startedAt = Date.now();
  context.logger?.info("ZCode Protocol background turn started", {
    inputId: params.inputId,
    queryId: params.queryId,
    sessionId: record.app.sessionId,
    textLength: params.content.length,
    workspacePath: record.workspace.workspacePath,
  });
  let mutationReason = "prompt_completed";
  const previousAutomationId = record.activeAutomationId;
  const previousOffPeakTaskId = record.activeOffPeakTaskId;
  const activeAutomationId = resolvePromptTurnAutomationId(params);
  const activeOffPeakTaskId = resolvePromptTurnOffPeakTaskId(params);
  const turnToolDisallowlist = buildPromptTurnToolDisallowlist(
    params,
    activeAutomationId,
    activeOffPeakTaskId,
  );
  if (activeAutomationId) {
// Attachment input still goes through session/send; automation dispatch may miss the automationId.
// But inputId will retain automation-* runId. Mark it here to prevent CronCreate from binding active
// Create scheduled tasks recursively in the session.
    record.activeAutomationId = activeAutomationId;
  }
  if (activeOffPeakTaskId) {
//Distribute the same type of pocket mark in idle time for offpeak-port to reject recursive OffPeakCreate.
    record.activeOffPeakTaskId = activeOffPeakTaskId;
  }
  try {
    const admission = await record.app.sendInput(
      {
        attachments: mapProtocolPromptAttachments(params.attachments),
        text: params.content,
      },
      {
        abortSignal: params.abortController.signal,
        intent: params.intent,
        modelExecution: params.modelExecution,
        browserAmbientContext: params.browserAmbientContext,
        inputId: params.inputId,
        queryId: params.queryId,
        ...(params.automationId
          ? { automationId: params.automationId }
          : params.offPeakTaskId
            ? {
                offPeakTaskId: params.offPeakTaskId,
                ...(params.offPeakRunType ? { offPeakRunType: params.offPeakRunType } : {}),
              }
            : {}),
// legacy session/send can also reuse active runtime; the model can still be seen when only port rejection is done
// CronCreate, and possibly change CronDelete on failure. Automation turn and cron task session follow-up input
// All are directly removed from the current round of provider tool interface.
        ...(turnToolDisallowlist ? { toolDisallowlist: turnToolDisallowlist } : {}),
      },
    );
// admission is not the end of the round. Cleaning up in advance will lose automation ownership and Stop controller;
// ACK has been returned by session/send, and the background will only release this round of resources after the actual completion.
    if (admission.kind === "rejected") {
      throw new ProtocolRequestError(-32010, `Prompt admission rejected: ${admission.reason}`);
    }
    if (admission.kind === "started_turn") await admission.completion;
    context.logger?.info("ZCode Protocol background turn completed", {
      durationMs: Date.now() - startedAt,
      inputId: params.inputId,
      queryId: params.queryId,
      sessionId: record.app.sessionId,
      workspacePath: record.workspace.workspacePath,
    });
  } catch (error) {
    mutationReason = "prompt_failed";
    context.logger?.warn("ZCode Protocol background turn failed", {
      durationMs: Date.now() - startedAt,
      error: error instanceof Error ? error.message : String(error),
      inputId: params.inputId,
      sessionId: record.app.sessionId,
      workspacePath: record.workspace.workspacePath,
    });
  } finally {
    if (record.activeAbortController === params.abortController) {
// prompt_completed/prompt_failed is the ready boundary of the downstream host queue.
// The active lock must be released first before broadcasting the status, otherwise the mobile phone will be drained immediately after receiving ready.
// session/send is rejected as "A prompt is already running for this session".
      record.activeAbortController = undefined;
      context.logger?.info("ZCode Protocol background turn cleared active controller", {
        durationMs: Date.now() - startedAt,
        inputId: params.inputId,
        sessionId: record.app.sessionId,
        workspacePath: record.workspace.workspacePath,
      });
    }
    record.activeAutomationId = previousAutomationId;
    record.activeOffPeakTaskId = previousOffPeakTaskId;
  }
  await afterStateMutation(context, record, mutationReason);
}

function buildPromptTurnToolDisallowlist(
  params: {
    automationId?: string;
    offPeakTaskId?: string;
    inputId?: string;
    toolDenylist?: readonly string[];
  },
  activeAutomationId = params.automationId,
  activeOffPeakTaskId = params.offPeakTaskId,
): readonly string[] | undefined {
  const tools = new Set(params.toolDenylist ?? []);
  if (activeAutomationId) tools.add("CronCreate");
// Hide the distribution wheel during idle time OffPeakCreate (anti-recursive self-derivation); OffPeakList is read-only and reserved.
// Note that the automation round does not add OffPeakCreate - the cron round is released (regularly derived idle time tasks).
// SendMessage / Workflow is also hidden and has the same value as V4 prompt-turn and core turn-loop-state.
  if (activeOffPeakTaskId) {
    for (const toolName of ["OffPeakCreate", "SendMessage", "Workflow"]) tools.add(toolName);
  }
  return tools.size > 0 ? [...tools] : undefined;
}

function resolvePromptTurnAutomationId(params: {
  automationId?: string;
  inputId?: string;
}): string | undefined {
  const explicit = params.automationId?.trim();
  if (explicit) return explicit;
  const inputId = params.inputId?.trim();
  if (!inputId?.startsWith("automation-")) return undefined;
  const separatorIndex = inputId.indexOf(":");
  const automationId = separatorIndex >= 0 ? inputId.slice(0, separatorIndex) : inputId;
  return automationId.length > "automation-".length ? automationId : undefined;
}

function resolvePromptTurnOffPeakTaskId(params: {
  offPeakTaskId?: string;
  inputId?: string;
}): string | undefined {
  const explicit = params.offPeakTaskId?.trim();
  if (explicit) return explicit;
// Bottom line: The inputId distributed during the continuation is in the shape of `offpeak-<uuid>:resume:<uuid>` (the first traceId has no fixed prefix.
// The main signal must be explicit offPeakTaskId).
  const inputId = params.inputId?.trim();
  if (!inputId?.startsWith("offpeak-")) return undefined;
  const separatorIndex = inputId.indexOf(":");
  const offPeakTaskId = separatorIndex >= 0 ? inputId.slice(0, separatorIndex) : inputId;
  return offPeakTaskId.length > "offpeak-".length ? offPeakTaskId : undefined;
}

async function continueGoalAfterChange(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
  params: {
    inputId?: string;
    reason: string;
    target: ProtocolGoalTarget;
    title: string;
  },
) {
  const response = formatGoalChanged(params.title, params.target);
  const isPlanMode = record.app.runtime.getPlanEnabled?.() ?? record.app.getMode?.() === "plan";
// continueActiveTarget is a required capability of App; whether it can continue only depends on the current mode and whether there is an active turn.
  const canContinue = !isPlanMode && !record.activeAbortController;

  if (canContinue) {
    await ensureSessionModelAvailableForNextTurn(context, record);
    const abortController = new AbortController();
    record.activeAbortController = abortController;
    void runWithSessionResidencyFinalization(record, () =>
      runGoalContinuationInBackground(context, record, {
        abortController,
        inputId: params.inputId,
      }),
    ).catch(() => {
// The failure of the background goal continuation will be reported through the status/event flow downgrade; here is a safety net to prevent unhandled rejection.
    });
  }

  const snapshotAfterGoal = await afterStateMutation(context, record, params.reason);
  return {
    response: isPlanMode ? appendPlanModeGoalContinuationNote(response) : response,
    snapshot: snapshotAfterGoal,
    startedTurn: canContinue,
  };
}

async function runGoalContinuationInBackground(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
  params: {
    abortController: AbortController;
    inputId?: string;
  },
): Promise<void> {
  let mutationReason = "goal_continuation_completed";
  try {
    await record.app.continueActiveTarget?.({
      abortSignal: params.abortController.signal,
      inputId: params.inputId,
    });
  } catch {
    mutationReason = "goal_continuation_failed";
  } finally {
    if (record.activeAbortController === params.abortController) {
// The active lock should be released immediately after the continuation itself is completed; snapshot/command discovery is just a subsequent broadcast,
// If the lock continues to be occupied, consecutive `/goal` will be misjudged as an active turn.
      record.activeAbortController = undefined;
    }
  }
  await afterStateMutation(context, record, mutationReason);
}

export async function stopSession(context: ZCodeProtocolAgentServerContext, rawParams: unknown) {
  const params = parseParams(zcodeSessionStopParamsSchema, rawParams);
  const record = requireSession(context, params.sessionId);
  const hadActivePrompt = Boolean(record.activeAbortController);
  let pausedTarget: ProtocolGoalTarget | null = null;
  context.logger?.info("ZCode Protocol session/stop received", {
    hadActivePrompt,
    sessionId: params.sessionId,
    workspacePath: record.workspace.workspacePath,
  });
  if (hadActivePrompt) {
    pausedTarget = await pauseActiveGoalForSessionStop(context, record, {
      sessionId: params.sessionId,
    });
  }
  record.activeAbortController?.abort(new Error("ZCode Protocol session stopped"));
  if (pausedTarget) {
    await afterStateMutation(context, record, "session_stop_goal_paused");
  }
  return {};
}

async function pauseActiveGoalForSessionStop(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
  params: { sessionId: string },
): Promise<ProtocolGoalTarget | null> {
  if (!record.app.readTarget || !record.app.updateTargetStatus) {
    return null;
  }
  const target = await record.app.readTarget();
  if (!target || target.status !== "active") {
    return null;
  }

  try {
// When Stop only aborts the controller, if the goal verifier does not receive the abort in time,
// The active goal will be retained while the queue still holds stopRequested, causing subsequent queues to be unable to drain.
    const pausedTarget = await record.app.updateTargetStatus("paused");
    if (pausedTarget) {
      context.logger?.info("ZCode Protocol session/stop paused active goal", {
        sessionId: params.sessionId,
        targetId: pausedTarget.targetID,
        workspacePath: record.workspace.workspacePath,
      });
    }
    return pausedTarget;
  } catch (error) {
    context.logger?.warn("ZCode Protocol session/stop failed to pause active goal", {
      error: error instanceof Error ? error.message : String(error),
      sessionId: params.sessionId,
      targetId: target.targetID,
      workspacePath: record.workspace.workspacePath,
    });
    return null;
  }
}

export async function cancelBackgroundTask(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
) {
  const params = parseParams(zcodeSessionCancelBackgroundTaskParamsSchema, rawParams);
  const record = requireSession(context, params.sessionId);
  context.logger?.info("ZCode Protocol session/cancelBackgroundTask received", {
    sessionId: params.sessionId,
    taskId: params.taskId,
    workspacePath: record.workspace.workspacePath,
  });

  if (!record.app.cancelBackgroundTask) {
    throw new ProtocolRequestError(
      -32031,
      "Background task cancellation is not supported by this session runtime",
      {
        sessionId: params.sessionId,
        taskId: params.taskId,
      },
    );
  }

  const result = await record.app.cancelBackgroundTask(params.taskId);
// Background bash cancellation is a running state change and will not go through the normal prompt final state.
// Send state.updated immediately after cancellation, so that both desktop continuous and mobile phone replayable can refresh the projection through their respective snapshot boundaries.
  await afterStateMutation(context, record, "background_task_cancelled");
  return result;
}

export async function setModel(context: ZCodeProtocolAgentServerContext, rawParams: unknown) {
  const params = parseParams(zcodeSessionSetModelParamsSchema, rawParams);
  const record = requireSession(context, params.sessionId);
  assertExpectedRevision(record, params.expectedRevision);
  await runSessionModelConfigMutation(record.app, async () => {
// Complete session configuration commands cannot be discarded via identity string reasoning; atomically verified/saved by shared setter.
    await record.app.setModel(params.model);
  });
  return await afterStateMutation(context, record, "model_changed");
}

export async function setThoughtLevel(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
) {
  const params = parseParams(zcodeSessionSetThoughtLevelParamsSchema, rawParams);
  if (!params.thoughtLevel) {
    throw new ProtocolRequestError(-32602, "thoughtLevel is required");
  }
  const thoughtLevel = params.thoughtLevel;
  const record = requireSession(context, params.sessionId);
  assertExpectedRevision(record, params.expectedRevision);
  await runSessionModelConfigMutation(record.app, async () => {
    await record.app.setThoughtLevel(thoughtLevel);
  });
  return await afterStateMutation(context, record, "thought_level_changed");
}

export async function setMode(context: ZCodeProtocolAgentServerContext, rawParams: unknown) {
  const params = parseParams(zcodeSessionSetModeParamsSchema, rawParams);
  const record = requireSession(context, params.sessionId);
  assertExpectedRevision(record, params.expectedRevision);
  await record.app.setMode(params.mode);
  return await afterStateMutation(context, record, "mode_changed");
}

export async function closeSession(context: ZCodeProtocolAgentServerContext, rawParams: unknown) {
  const params = parseParams(zcodeSessionCloseParamsSchema, rawParams);
  const record = requireSession(context, params.sessionId);
  if (!shouldCloseSessionForExpectedPersistence(record.persistence, params.expectedPersistence)) {
// Connection switching and cross-end initialization may be concurrent. session/send will first promote deferred to
// immediate; conditional closing must be judged atomically on the Agent record and cannot rely on the old snapshot of the renderer.
    return { closed: false };
  }
  record.unsubscribe?.();
  await record.app.close?.();
// v4 channel: The session is closed and the publisher/subscription schedule is cleared at the same time; when the session is reopened, snapshot cold start is performed.
// dispose must be deleted before the registry - gateway relies on getSessionWorkspaceId
// (Read context.sessions) Locate the workspace to push session.removed to sessions-index subscribers.
  context.v4Gateway?.disposeSession(params.sessionId);
  context.sessions.delete(params.sessionId);
//The memory event store is released with the record.
  await record.eventStore.deleteSession(params.sessionId as SessionId);
  return { closed: true };
}

function shouldCloseSessionForExpectedPersistence(
  currentPersistence: ZCodeSessionPersistence,
  expectedPersistence?: ZCodeSessionPersistence,
): boolean {
  return expectedPersistence === undefined || currentPersistence === expectedPersistence;
}

export async function generateWorkspaceText(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
  abortSignal?: AbortSignal,
) {
  const params = parseParams(zcodeWorkspaceGenerateTextParamsSchema, rawParams);
  const active = Array.from(context.sessions.values()).find(
    (record) => record.workspace.workspaceKey === params.workspace.workspaceKey,
  );
  const input = {
    selection: params.selection,
    ...(params.prompt ? { prompt: params.prompt } : {}),
    ...(params.messages ? { messages: params.messages } : {}),
    ...(params.tools
      ? {
          tools: params.tools.map((tool) => ({
            name: tool.name,
            description: tool.description,
            inputSchema: tool.inputSchema,
          })),
        }
      : {}),
    querySource: params.querySource,
    ...(params.maxOutputTokens ? { maxOutputTokens: params.maxOutputTokens } : {}),
  };
  const app =
    active?.app ??
    (await createWorkspaceZCodeApp(context, params.workspace, {
      env: context.deps.env,
      eventStore: context.deps.createSessionEventStore("workspace-generate-text"),
      runtimeConfig: {
        workingDirectory: params.workspace.workspacePath,
      },
      sessionStore: context.deps.sessionStore,
      version: context.deps.version,
    }));

  try {
    const result = await app.generateWorkspaceText(input, { abortSignal });
    return {
      text: result.text,
      selection: result.selection,
      finishReason: result.finishReason,
      ...(result.usage ? { usage: result.usage } : {}),
      ...(result.toolCalls ? { toolCalls: result.toolCalls } : {}),
    };
  } finally {
    if (!active) {
      await app.close?.();
    }
  }
}

function ensureNoActiveTurn(record: ZCodeProtocolSessionRecord, message: string): void {
  if (!record.activeAbortController) {
    return;
  }
  throw new ProtocolRequestError(-32010, message);
}

function appendPlanModeGoalContinuationNote(response: string): string {
  return `${response}\n\n${PLAN_MODE_GOAL_CONTINUATION_SKIPPED_MESSAGE}`;
}

function formatGoalSummary(target: ProtocolGoalTarget | null): string {
  if (!target) {
    return "No goal is set. Use /goal <objective> to set one.";
  }

  return formatGoalChanged(`Goal ${target.status}`, target);
}

function formatGoalChanged(title: string, target: ProtocolGoalTarget): string {
  const lines = [title, `Objective: ${target.objective}`];
  if (target.tokensUsed !== undefined || target.tokenBudget !== undefined) {
    const budget =
      target.tokenBudget === null || target.tokenBudget === undefined
        ? "none"
        : target.tokenBudget.toString();
    lines.push(`Usage: ${target.tokensUsed ?? 0} tokens / ${budget}`);
  }
  if (target.timeUsedSeconds !== undefined) {
    lines.push(`Time: ${target.timeUsedSeconds} seconds`);
  }
  return lines.join("\n");
}

function assertExpectedProviderRevision(
  _context: ZCodeProtocolAgentServerContext,
  _record: ZCodeProtocolSessionRecord,
  expectedProviderRevision: string | undefined,
): void {
// Compatible with old client fields. Provider revision is managed by the process Registry itself and no longer accepts Host CAS.
  void expectedProviderRevision;
}

export async function ensureSessionModelAvailableForNextTurn(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
): Promise<void> {
  if (await ensureSessionModelAvailable(context, record)) {
    record.stateRevision++;
  }
}

async function resolveForkTarget(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
  target: ZCodeSessionHistoryTarget,
): Promise<{ targetCheckpointId?: string; targetMessageId?: string }> {
  if (target.kind === "latestCheckpoint") {
    return {};
  }
  if (target.kind === "checkpoint") {
    return { targetCheckpointId: target.checkpointId };
  }
  if (target.kind === "message") {
    return { targetMessageId: target.messageId };
  }
  return {
    targetMessageId: await resolveTurnMessageId(context, record, target.turnIndex, "assistant"),
  };
}

async function resolveTurnMessageId(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
  turnIndex: number,
  role: "assistant" | "user",
): Promise<MessageId> {
  const messages = (await readActiveSessionMessages(context, record)).filter(
    isUserVisibleSessionMessage,
  );
  let currentTurn = -1;
  let lastAssistantForTargetTurn: MessageId | undefined;
  for (const message of messages) {
    if (message.info.role === "user") {
      if (role === "assistant" && currentTurn === turnIndex && lastAssistantForTargetTurn) {
// The legacy turn target of fork/rewind is the "assistant of the target turn",
// Not "scan to the end of history and still be in the target round". When encountering the next user, clear the ones found
// assistant, so that only the last round can be forked; synthetics such as compact summary and fork notice
// user will turn the last round into a non-final round. Return directly to the stable target before leaving the target turn.
        return lastAssistantForTargetTurn;
      }
      currentTurn += 1;
      if (role === "user" && currentTurn === turnIndex) {
        return message.info.id as MessageId;
      }
      continue;
    }
    if (message.info.role === "assistant" && currentTurn === turnIndex) {
      lastAssistantForTargetTurn = message.info.id as MessageId;
    }
  }
  if (role === "assistant" && lastAssistantForTargetTurn) {
    return lastAssistantForTargetTurn;
  }
  throw new ProtocolRequestError(
    zcodeProtocolErrorCodes.sessionUnavailable,
    `Cannot resolve ${role} message for turnIndex=${turnIndex}`,
  );
}

export async function getTaskTokenUsage(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
) {
  const params = parseParams(zcodeTaskTokenUsageParamsSchema, rawParams ?? {});
  const usageStore = context.deps.sessionStore as Partial<UsageStorePort> | undefined;
  if (!usageStore?.queryTaskUsage) {
    return {
      sessionId: params.sessionId,
      totalTokens: 0,
      inputTokens: 0,
      outputTokens: 0,
      reasoningTokens: 0,
      cacheCreationTokens: 0,
      cacheReadTokens: 0,
      modelRequestCount: 0,
      modelErrorCount: 0,
      inputBaselineBySource: {},
    };
  }

  const usage = await usageStore.queryTaskUsage({
    sessionID: params.sessionId as SessionId,
  });
  return {
    sessionId: params.sessionId,
    totalTokens: usage.totalTokens,
    inputTokens: usage.inputTokens,
    outputTokens: usage.outputTokens,
    reasoningTokens: usage.reasoningTokens,
    cacheCreationTokens: usage.cacheCreationTokens,
    cacheReadTokens: usage.cacheReadTokens,
    modelRequestCount: usage.modelRequestCount,
    modelErrorCount: usage.modelErrorCount,
    inputBaselineBySource: usage.inputBaselineBySource,
  };
}

/**
* Pure configuration event collection: only changes the session selection (model/collaboration mode) and does not represent user message activity.
* These events do not bump record.updatedAt(=sessions-index lastActivityAt),
* Avoid configuration operations driving the sidebar list to be rearranged by activity time.
* - ModelSelected / SessionModeChanged: pure configuration changes (cut model/cut mode).
* - SessionTitleUpdated: The title is session metadata; cold recovery will reissue the title event for v4 projection,
* Do not treat historical tasks as if they have just been active and push them to the top of the list.
* - SessionResumed: Opening/resuming a session is for reading, not activity. Cold recovery path just changed
* record.updatedAt is backfilled with the real time of the store (see the description in resumeSession op),
* If the resume event triggers Date.now(), the click/refresh task will be pushed to the front of the list and rearranged.
* - WorkspaceHookAdmissionUpdated: Cold recovery re-evaluates workspace hook admission status, does not represent user activity;
* If the blacklist is missed, the admission status event after SessionResumed will display the historical task as "Just now".
* - HookRun*: hook lifecycle is the internal execution details of turn/session; a normal turn already has messages, tools, etc.
* The activity event is responsible for updating the time. The SessionStart hook of cold recovery cannot create a user activity alone.
 */
function isNonActivitySessionEvent(event: SessionEvent): boolean {
  return (
    event.type === SessionEventType.ModelSelected ||
    event.type === SessionEventType.SessionModeChanged ||
    event.type === SessionEventType.SessionTitleUpdated ||
    event.type === SessionEventType.SessionResumed ||
    event.type === SessionEventType.WorkspaceHookAdmissionUpdated ||
    event.type === SessionEventType.HookRunStarted ||
    event.type === SessionEventType.HookRunProgress ||
    event.type === SessionEventType.HookRunCompleted ||
    event.type === SessionEventType.HookRunFailed ||
    event.type === SessionEventType.HookRunBlocked
  );
}

/**
* `record.persistence` is the protocol-side image of the runtime "session has been persisted" fact: deferred = draft (do not enter session/list,
* Without entering sessions-index, it can be closed and withdrawn by the condition of expectedPersistence="deferred"). Starting path at accepted moment
* Preemptive promotion (closing the race condition requires atomic judgment, see startPromptTurn / admission), but **any** persisted for the first time by the runtime
* The path must allow the session to leave draft - the session started directly by the hub is dropped by runtime and controlledOnly
* Start the round, there is no starter, the record is always deferred, sessions-index treats it as a draft and skips it, and the sidebar never appears.
* So the only entry of the event stream is aligned according to the runtime fact: after persistence, the runtime must send at least one event
* (SessionTitleUpdated first_input), the following ingest will push the session to sessions-index.
* The runtime of the test stub may not have this method (`as never`), and its absence means it is not aligned.
 */
function reconcileRecordPersistence(record: ZCodeProtocolSessionRecord): void {
  if (record.persistence !== "deferred") return;
  if (record.app.runtime?.isSessionPersisted?.() === true) record.persistence = "immediate";
}

export function onSessionEvent(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
  event: SessionEvent,
): void {
  const computerUseOperationEvent = mapComputerUseOperationEvent(event);
  if (computerUseOperationEvent) {
    try {
// The desktop v4 main chain does not have the deliveryKind subscription of the old session/event, and the top prompt cannot continue.
// Rely on the old gate; only content-free lifecycle metadata is sent here, in parallel with v4 projection, without blocking each other.
      context.notify({
        method: zcodeProtocolMethods.computerUseOperationEvent,
        params: computerUseOperationEvent,
      });
// This notify used to only have logs when an error was thrown, and the successful path was traceless, so "the agent was not sent at all"
// Indistinguishable from "sent but silently discarded by the host". The CLI's own jsonl is at debug level, which is enough to close the loop to the source.
      context.logger?.debug("Computer Use operation lifecycle notification sent", {
        event: "zcode_protocol.computer-use.operation-event.sent",
        eventId: computerUseOperationEvent.eventId,
        sessionId: computerUseOperationEvent.sessionId,
        kind: computerUseOperationEvent.kind,
        turnId:
          "turnId" in computerUseOperationEvent ? computerUseOperationEvent.turnId : undefined,
      });
    } catch (error) {
      context.logger?.warn("Computer Use operation lifecycle notification failed", {
        error: error instanceof Error ? error.message : String(error),
        event: "zcode_protocol.computer-use.operation-event.failed",
        eventId: computerUseOperationEvent.eventId,
        sessionId: computerUseOperationEvent.sessionId,
        turnId:
          "turnId" in computerUseOperationEvent ? computerUseOperationEvent.turnId : undefined,
      });
    }
  }
  if (String(event.sessionId) !== record.app.sessionId) {
// The subagent runtime reuses the parent runtime's external event sink, but raw child events
// Still belongs to child session. Unconditionally using parent record id ingest will cause the child topic to open
// No longer updated, and may also incorrectly bump the parent task activity time. Here you must first press the event's own sessionId
//Route and return; the parent mirror event itself uses the parent sessionId and continues to follow the existing link below.
    context.v4Gateway?.ingestDetachedLiveSession(
      String(event.sessionId),
      event,
      record.app.sessionId,
    );
    return;
  }
// record.updatedAt is the lastActivityAt fact source of sessions-index (v4-bridge
// getSessionIndexMeta), the UI sidebar is sorted by it. Pure configuration/restore/title metadata events are not user session activities,
// There was no difference before. The bump will cause "cut model/click to open task" to push the task to the top of the list and trigger the reordering of the entire table.
  const nonActivityEvent = isNonActivitySessionEvent(event);
  if (event.type === SessionEventType.ModelSelected) {
//Restore candidates only serve the initial UI; after the user's new selection takes effect, it cannot be resurrected by the old candidate when it is subsequently cleared.
    delete record.restoredModelSelection;
  }
  if (!nonActivityEvent) {
    record.updatedAt = Date.now();
  }
  reconcileRecordPersistence(record);
// The debugging bypass does not depend on the chat subscription; it is placed before the deliveryKind judgment to prevent the diagnosis from being cut off again after the old subscription is exited.
  observeSessionDebug(record, event);
// v4 channel: authoritative events are unconditionally fed to the v4 projection/publisher - v4 subscription does not rely on the old protocol
// deliveryKind is in subscription state, and the frame rhythm is scheduled by the gateway according to the subscriber profile.
  context.v4Gateway?.ingest(record.app.sessionId, event);
  if (!record.deliveryKind) return;
  const deltaInfo = readStreamingDeltaBatchableEvent(event);
  if (deltaInfo && !shouldHideProtocolSessionEvent(record, event)) {
    const batchState = getLiveProtocolStreamingDeltaBatchState(record, record.deliveryKind);
    if (batchState.batch && batchState.batch.batchKey !== deltaInfo.batchKey) {
      flushLiveProtocolStreamingDeltaBatch(context, record, record.deliveryKind);
    }
    batchState.batch = mergeProtocolStreamingDeltaBatch(batchState.batch, event, deltaInfo);
    if (
      deltaInfo.shouldFlushFirstDelta &&
      !batchState.flushedFirstDeltaBatchKeys.has(deltaInfo.batchKey)
    ) {
      flushLiveProtocolStreamingDeltaBatch(context, record, record.deliveryKind);
      batchState.flushedFirstDeltaBatchKeys.add(deltaInfo.batchKey);
      return;
    }
    if (
      batchState.batch.delta.length >= batchState.batch.maxChars ||
      shouldFlushProtocolStreamingDeltaBatchForInterval(
        batchState.batch,
        batchState.lastFlushAtByBatchKey,
      )
    ) {
      flushLiveProtocolStreamingDeltaBatch(context, record, record.deliveryKind);
    }
    return;
  }

  resetLiveProtocolStreamingDeltaBatch(context, record, record.deliveryKind);
  sendProtocolSessionEvent(context, record, event, record.deliveryKind);
}

function flushLiveProtocolStreamingDeltaBatch(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
  deliveryKind?: ZCodeDeliveryKind,
): void {
  const batchState = getLiveProtocolStreamingDeltaBatchState(record, deliveryKind);
  if (!batchState.batch) {
    return;
  }
  rememberProtocolStreamingDeltaBatchFlush(batchState.lastFlushAtByBatchKey, batchState.batch);
  const event = materializeProtocolStreamingDeltaBatch(batchState.batch);
  batchState.batch = undefined;
//Performance fix: provider will chop Write/Edit input and reasoning/text output into a large number of very small deltas.
// Merge adjacent diffs at protocol boundaries with a deterministic byte budget, the renderer still appends by delta, but is no longer woken up for each packet.
// Use SessionEvent.timestamp for low-frequency flush for text somatosensory. Live setTimeout cannot be used, otherwise replay cannot reproduce the boundary.
  sendProtocolSessionEvent(context, record, event, deliveryKind);
}

function resetLiveProtocolStreamingDeltaBatch(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
  deliveryKind?: ZCodeDeliveryKind,
): void {
  const batchState = getLiveProtocolStreamingDeltaBatchState(record, deliveryKind);
  flushLiveProtocolStreamingDeltaBatch(context, record, deliveryKind);
  batchState.flushedFirstDeltaBatchKeys.clear();
  batchState.lastFlushAtByBatchKey.clear();
}

function sendProtocolSessionEvent(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
  event: SessionEvent,
  deliveryKind?: ZCodeDeliveryKind,
): void {
  const mappedEvent = mapProtocolSessionEvent(context, record, event, deliveryKind);
  if (!mappedEvent) return;
  context.notify({
    method: "session/event",
    params: mappedEvent,
  });
  logProtocolSessionEventSent(context, event, mappedEvent);
}

function integratedTerminalShellToExecutionSelection(
  selection: IntegratedTerminalShellSelection | undefined,
): ExecutionShellSelection | undefined {
  if (!selection || selection.mode === "auto") {
    return undefined;
  }
  return {
    display: {
      name: selection.dialect === "git-bash" ? "Git Bash" : "CMD",
    },
    dialect: selection.dialect,
    id: selection.id,
    label: selection.label,
    path: selection.path,
    source: "user-config",
  };
}

function resolveProtocolBashShellSelection(
  context: ZCodeProtocolAgentServerContext,
  selection: IntegratedTerminalShellSelection | undefined,
): ExecutionShellSelection {
  const configuredSelection = integratedTerminalShellToExecutionSelection(selection);
  return resolveEffectiveBashShellSelection({
    env: context.deps.env ?? process.env,
    override: configuredSelection,
    platform: context.deps.platform ?? process.platform,
  }).selection;
}

async function requestSessionRuntimePreferences(
  context: ZCodeProtocolAgentServerContext,
  sessionId: SessionId,
  scope: ZCodeSessionRuntimePreferencesScope,
  trace?: ZCodeProtocolTrace,
): Promise<ZCodeSessionRuntimePreferencesResult> {
  const startedAt = Date.now();
// ZCodeProtocolTrace.traceId is the string of the protocol layer; LogContext requires branded TraceId.
//According to the existing conventions of this document, the assertion ends to avoid repeated conversion of multiple log structures.
  const traceId = trace?.traceId as TraceId | undefined;
  context.logger?.debug("ZCode Protocol runtime preferences request started", {
    event: "zcode_protocol.runtime_preferences.request_started",
    module: "bootstrap.zcode_protocol",
    scope,
    sessionId,
    traceId,
  });
  try {
    const result = await context.requestClient(
      zcodeProtocolMethods.sessionRequestRuntimePreferences,
      { sessionId, scope },
      zcodeSessionRuntimePreferencesResultSchema,
      {
        timeoutMs: ZCODE_SESSION_RUNTIME_PREFERENCES_REQUEST_TIMEOUT_MS,
        ...(trace ? { trace } : {}),
      },
    );
    context.logger?.debug("ZCode Protocol runtime preferences response received", {
      durationMs: Math.max(0, Date.now() - startedAt),
      event: "zcode_protocol.runtime_preferences.response_received",
      module: "bootstrap.zcode_protocol",
      scope,
      sessionId,
      traceId,
    });
    return result;
  } catch (error) {
    const errorCode = error instanceof ProtocolRequestError ? error.code : undefined;
    const errorMessage = error instanceof Error ? error.message : String(error);
    const diagnostic = {
      durationMs: Math.max(0, Date.now() - startedAt),
      errorCode,
      errorMessage,
      event: "zcode_protocol.runtime_preferences.request_failed",
      module: "bootstrap.zcode_protocol",
      scope,
      sessionId,
      traceId,
    };
    if (errorCode === -32022) {
// Diagnosis: Preference request timeout occurs before runtime registration; record session/scope to distinguish
// "Host did not receive/reply the packet" and "other stages of the recovery process failed".
      context.logger?.warn("ZCode Protocol runtime preferences request timed out", diagnostic);
    } else if (errorCode !== -32601 && errorCode !== -32020) {
      context.logger?.warn("ZCode Protocol runtime preferences request failed", diagnostic);
    } else {
      context.logger?.debug(
        "ZCode Protocol runtime preferences compatibility fallback",
        diagnostic,
      );
    }
    if (error instanceof ProtocolRequestError && (error.code === -32601 || error.code === -32020)) {
// Compatible with old Host or pure CLI creation path without Host; Memory is turned off by default by the product.
// Enhanced search remains enabled by default, other protocol/transport errors still prevent runtime creation.
      return {
        askUserQuestionAutoResolutionEnabled: true,
        memoryEnabled: false,
        modelContextBudgetStrategy: DEFAULT_ZCODE_MODEL_CONTEXT_BUDGET_STRATEGY,
        nativeSearchEnhancementsEnabled: true,
      };
    }
    throw error;
  }
}

async function resolveSessionStartupPreferences(
  context: ZCodeProtocolAgentServerContext,
  sessionId: SessionId,
  source: SessionStartupPreferencesSource,
  trace?: ZCodeProtocolTrace,
): Promise<SessionStartupPreferences> {
  if (source.kind === "inherit") {
    const inheritedShellSelection = source.parent.app.runtime.getSessionShellSelection();
    return {
      memoryEnabled: source.parent.memoryEnabled,
      modelContextBudgetStrategy: DEFAULT_ZCODE_MODEL_CONTEXT_BUDGET_STRATEGY,
      nativeSearchEnhancementsEnabled: source.parent.nativeSearchEnhancementsEnabled,
      resolveInitialBashShellSelection: async () => inheritedShellSelection,
    };
  }

  const runtimePreferences = await requestSessionRuntimePreferences(
    context,
    sessionId,
    "runtime-materialization",
    trace,
  );
// Must be applied before the session runtime can issue the first question; closing the path will wait for existing snooze persistence to complete.
  await context.v4Interactions.initializeAskUserQuestionAutoResolutionEnabled(
    runtimePreferences.askUserQuestionAutoResolutionEnabled,
  );
  return {
    memoryEnabled: runtimePreferences.memoryEnabled,
    modelContextBudgetStrategy: DEFAULT_ZCODE_MODEL_CONTEXT_BUDGET_STRATEGY,
    nativeSearchEnhancementsEnabled: runtimePreferences.nativeSearchEnhancementsEnabled,
    resolveInitialBashShellSelection: async () => {
      const executionPreferences = await requestSessionRuntimePreferences(
        context,
        sessionId,
        "user-execution",
        trace,
      );
      return resolveProtocolBashShellSelection(
        context,
        executionPreferences.integratedTerminalShell,
      );
    },
  };
}

async function materializeSessionRecord(
  context: ZCodeProtocolAgentServerContext,
  params: ZCodeSessionRecordParams,
  sessionId: SessionId,
  resume: boolean,
  source: SessionStartupPreferencesSource,
  trace?: ZCodeProtocolTrace,
): Promise<ZCodeProtocolSessionRecord> {
  const startupPreferences = await resolveSessionStartupPreferences(
    context,
    sessionId,
    source,
    trace,
  );
  return createRecord(context, params, sessionId, resume, startupPreferences, trace);
}

async function createRecord(
  context: ZCodeProtocolAgentServerContext,
  params: ZCodeSessionRecordParams,
  sessionId: SessionId,
  resume: boolean,
  startupPreferences: SessionStartupPreferences,
  trace?: ZCodeProtocolTrace,
): Promise<ZCodeProtocolSessionRecord> {
  const workspace =
    "workspace" in params && params.workspace
      ? params.workspace
      : buildWorkspaceRef({ workspacePath: context.deps.cwd ?? process.cwd() });
  const eventStore = context.deps.createSessionEventStore(sessionId);
  const traceContext = createProtocolRootTraceContext(sessionId, trace);
  const initialModel = "model" in params ? params.model : undefined;
  const parentSessionId =
    "parentSessionId" in params && params.parentSessionId
      ? (params.parentSessionId as SessionId)
      : undefined;
  const taskType = params.taskType ?? "interactive";
  const runtimeMcp = protocolMcpServersToRuntimeMcpConfig(params.mcpServers);
  context.logger?.info("ZCode Protocol createRecord MCP config", {
    event: "zcode_protocol.create_record.mcp_config",
    rootTraceId: traceContext.traceId,
    inboundTraceId: trace?.traceId,
    paramMcpServerCount: params.mcpServers?.length ?? 0,
    runtimeHasMcpConfig: Boolean(runtimeMcp),
    runtimeMcpServerCount: Object.keys(runtimeMcp?.servers ?? {}).length,
    sessionId,
    workspaceKey: workspace.workspaceKey,
    workspacePath: workspace.workspacePath,
  });
// automation-port needs to read the real-time model/mode/thought of "this session"; record is created after app.
// Use mutable holders for lazy binding: the record is already ready when CronCreate calls create() in turn.
  let ownSessionRecord: ZCodeProtocolSessionRecord | undefined;
  const app = await createWorkspaceZCodeApp(context, workspace, {
    env: context.deps.env,
    eventStore,
    resume,
    runtimeConfig: {
      mode: "mode" in params ? params.mode : undefined,
      modelSelection: "model" in params ? toRuntimeModelSelection(initialModel) : undefined,
      parentSessionId,
      taskType,
//Dynamic workflow grayscale gate: with offPeakPort
// The same set of reading methods - this time the create/resume parameters take precedence, and when absent, the host is read and synchronized to the workspace level of the process
// Conclusion; neither is false (fail-closed). Here **must write explicit Boolean** and cannot be omitted.
// undefined: core defines "absence" as "not participating in grayscale, retaining all tools" (TUI / headless /
// semantics of workflow_child), sessions created by a trusted Host cannot fall into that exemption.
      dynamicWorkflowEnabled:
        ("dynamicWorkflowEnabled" in params && params.dynamicWorkflowEnabled === true) ||
        context.appRuntimePreferences.dynamicWorkflowEnabled === true,
// The tool allow/deny list on the protocol side is a session-level security boundary and must be entered into runtimeConfig.
// You cannot rely solely on prompt text constraints, otherwise built-in tools and dynamic MCP tools may still cross the call surface.
      toolAllowlist: "toolAllowlist" in params ? params.toolAllowlist : undefined,
      toolDisallowlist: "toolDenylist" in params ? params.toolDenylist : undefined,
      nativeSearchEnhancementsEnabled: startupPreferences.nativeSearchEnhancementsEnabled,
      modelContextBudgetStrategy: startupPreferences.modelContextBudgetStrategy,
// Memory Settings is a master switch in addition to the existing CLI features.memory/use. only when closed
// Write override to prevent the enable value from overwriting the user's existing CLI disable configuration.
      ...(startupPreferences.memoryEnabled ? {} : { memory: { enabled: false } }),
// desktop-continuous session/create first parses the enabled MCP of ~/.zcode/.agents by the UI,
// But the protocol app-server itself will not read the MCP store on the UI/main side; createRecord did not read it before
// params.mcpServers injects runtimeConfig, causing runtimeHasMcpConfig=false in the log and the tool never starts.
// MCP is a runtime startup configuration, so runtimeConfig.mcp must be written once at the session creation/restoration boundary.
      ...(runtimeMcp ? { mcp: runtimeMcp } : {}),
// Previously only the TUI path (tui-prompt-handler) was injected into titleGeneration,
// The session (desktop/web/mobile) created by ZCode Protocol app-server is not transmitted, resulting in
// shouldAttemptSessionTitleGeneration's `if (!config.titleGeneration) return false`
// Always hit, the request for the model to generate title is never triggered, and the sidebar title always stops at the user query of first_input.
// Add a default configuration here to enable it; whether it is generated or not is still determined by the runtime according to parent/taskType/turnNumber.
// The automation execution session explicitly turns off secondary naming to prevent the answer content from covering the original user query title.
      titleGeneration: params.titleGenerationEnabled === false ? { enabled: false } : {},
      workingDirectory: workspace.workspacePath,
// Identity isolation is separated from path execution: core only writes identity to session.workspace_id,
// workingDirectory is still the actual path on the remote machine; the local workspace remains undefined.
      workspaceIdentity: workspace.workspaceIdentity as WorkspaceId | undefined,
    },
// ZCode Protocol app-server has not injected a waitable interactive broker before.
// When core encounters permission / AskUserQuestion, it can only reject it by default, and the UI will never receive blocking requests.
// Here, the blocking interaction is converted into a server-to-client JSON-RPC request, and the app releases the runtime through response.
    permissionBroker: createProtocolInteractionBroker(context),
    automationPort: createProtocolAutomationPort(context, () => ownSessionRecord),
//Only access the tool surface that Host has opened; no injection by default. Reuse the existing asynchronous factory,
// Do not restore the old deferred ModelAdapter/Registry overlay, nor change the Session Selection.
    ...(("offPeakToolEnabled" in params && params.offPeakToolEnabled === true) ||
    context.appRuntimePreferences.offPeakToolEnabled === true
      ? { offPeakPort: createProtocolOffPeakPort(context, () => ownSessionRecord) }
      : {}),
    resolveInitialBashShellSelection: startupPreferences.resolveInitialBashShellSelection,
// browser-use: agent.browsers.* This converts the command into interaction/browserExecute reverse request.
    browserControlPort: createProtocolBrowserControlBroker(context),
// Protocol server is a trusted Desktop/Web/Mobile Host; the grayscale switch is explicitly injected from here.
//Does not read from the workspace/project configuration or environment variables. You can still roll back to the hard block by deleting this field when closing.
    workspaceHookTrustEnabled: true,
// Settings Trust without session has bypassed managed policy; session Runtime and
// workspace RPC must share the same provider held by the Host and cannot create default policies separately.
    workspaceHookPolicyProvider: context.deps.workspaceHookPolicyProvider,
    workspaceHookReviewHost: {
      taskId: sessionId,
      runId: `workspace-hook-run:${sessionId}:${crypto.randomUUID()}`,
      workspaceLabel:
        workspace.workspacePath.split(/[\\/]/u).filter(Boolean).at(-1) ?? workspace.workspacePath,
      ...(workspace.remoteSessionId ? { remoteSessionId: workspace.remoteSessionId } : {}),
    },
    sessionId,
    sessionStore: context.deps.sessionStore,
    traceContext,
    version: context.deps.version,
    modelIoFullRetentionEnabled: context.appRuntimePreferences.modelIoFullRetentionEnabled,
  });
  const now = Date.now();
  const record: ZCodeProtocolSessionRecord = {
    app,
    createdAt: now,
    eventStore,
    memoryEnabled: startupPreferences.memoryEnabled,
    modelContextBudgetStrategy: startupPreferences.modelContextBudgetStrategy,
    nativeSearchEnhancementsEnabled: startupPreferences.nativeSearchEnhancementsEnabled,
    ...(parentSessionId ? { parentSessionId } : {}),
    persistence: "persistence" in params ? (params.persistence ?? "immediate") : "immediate",
    protocolEventSequences: new Map(),
    protocolToolInputTransmissions: new Map(),
    stateRevision: 0,
    taskType,
    traceContext,
    updatedAt: now,
    workspace,
  };
  const unsubscribeSessionEvents = app.runtime.subscribeEvents({
    onSessionEvent: (event) => onSessionEvent(context, record, event),
  });
  record.unsubscribe = () => {
    unsubscribeSessionEvents();
  };
// Bind the belonging session for automation-port to read the real-time model/mode/thought of this session.
  ownSessionRecord = record;
  return record;
}

async function readPersistedSessionMessages(
  context: ZCodeProtocolAgentServerContext,
  sessionId: string,
): Promise<MessageWithParts[]> {
  return await (context.deps.sessionStore?.messages({
    sessionID: sessionId as SessionId,
  }) ?? []);
}

function derivePersistedSessionMode(
  messages: readonly MessageWithParts[],
): ZCodeSessionCreateParams["mode"] | undefined {
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const info = messages[index]?.info;
    if (info?.role === "assistant" && isZCodeSessionMode(info.mode)) return info.mode;
  }
  return undefined;
}

function isZCodeSessionMode(
  value: unknown,
): value is NonNullable<ZCodeSessionCreateParams["mode"]> {
  return (
    value === "plan" ||
    value === "build" ||
    value === "edit" ||
    value === "yolo" ||
    value === "auto"
  );
}

function toRuntimeModelSelection(
  model: ZCodeSessionCreateParams["model"],
): ModelSelection | undefined {
  if (!model) return undefined;
  return {
    modelId: model.modelId,
    providerId: model.providerId,
    ...(model.options ? { options: model.options } : {}),
  };
}

interface SnapshotPhaseDurationsMs {
  contextUsageBreakdownEvents: number;
  eventSeq: number;
  goalVerificationEvents: number;
  persistedMessages: number;
  persistedSession: number;
  rewindEvents: number;
  target: number;
  todos: number;
  buildSnapshot: number;
}

interface SnapshotWithDiagnostics {
  snapshot: Awaited<ReturnType<typeof buildSessionSnapshot>>;
  phaseDurationsMs: SnapshotPhaseDurationsMs;
}

async function snapshot(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
  knownSession?: SessionInfo | null,
  options: {
    messageLimit?: number;
    modelAvailability?: "all" | "current";
  } = {},
) {
  return (await snapshotWithDiagnostics(context, record, knownSession, options)).snapshot;
}

async function snapshotWithDiagnostics(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
  knownSession?: SessionInfo | null,
  options: {
    messageLimit?: number;
    modelAvailability?: "all" | "current";
  } = {},
): Promise<SnapshotWithDiagnostics> {
  const snapshotStartedAt = Date.now();
  const phaseDurationsMs: SnapshotPhaseDurationsMs = {
    contextUsageBreakdownEvents: 0,
    eventSeq: 0,
    goalVerificationEvents: 0,
    persistedMessages: 0,
    persistedSession: 0,
    rewindEvents: 0,
    target: 0,
    todos: 0,
    buildSnapshot: 0,
  };
  const measureSnapshotPhase = async <T>(
    phase: keyof Omit<SnapshotPhaseDurationsMs, "buildSnapshot">,
    read: () => Promise<T>,
  ): Promise<T> => {
    const phaseStartedAt = Date.now();
    try {
      return await read();
    } finally {
      phaseDurationsMs[phase] = Date.now() - phaseStartedAt;
    }
  };
  const [
    eventSeq,
    persistedMessages,
    session,
    rewindEvents,
    persistedTarget,
    persistedTodos,
    persistedGoalVerificationEvents,
    persistedContextUsageBreakdownEvents,
  ] = await Promise.all([
    measureSnapshotPhase("eventSeq", () =>
      getProtocolEventSeq(context, record, record.deliveryKind),
    ),
    measureSnapshotPhase("persistedMessages", () => readSessionMessages(context, record)),
    knownSession === undefined
      ? measureSnapshotPhase("persistedSession", () =>
          getPersistedSession(context, record.app.sessionId),
        )
      : knownSession,
    measureSnapshotPhase("rewindEvents", () => readSessionRewindEvents(record)),
    measureSnapshotPhase("target", () => readSnapshotTarget(context, record)),
    measureSnapshotPhase("todos", () => readSnapshotTodos(context, record)),
    measureSnapshotPhase("goalVerificationEvents", () =>
      readSessionTargetCompletionVerificationEvents(context, record),
    ),
    measureSnapshotPhase("contextUsageBreakdownEvents", () =>
      readSessionContextUsageBreakdownEvents(record),
    ),
  ]);
  const messages = projectActiveSessionMessages(persistedMessages, session, rewindEvents);
  const buildStartedAt = Date.now();
  const builtSnapshot = await buildSessionSnapshot({
    app: record.app,
    deliveryKind: record.deliveryKind,
    eventSeq,
    fallbackCreatedAt: record.createdAt,
    fallbackUpdatedAt: record.updatedAt,
    lastError: record.restoreWarning,
    messages: limitMessages(messages, options.messageLimit),
    modelAvailability: options.modelAvailability,
    persistedContextUsageBreakdownEvents,
    persistedGoalVerificationEvents,
    session,
    slashCommandOptions: {
// The `/` directory of the session snapshot and the workspace presentation must give the same grayscale conclusion.
// Otherwise, `workflow` can still be seen in the sidebar panel in the closed state.
      dynamicWorkflowEnabled: context.appRuntimePreferences.dynamicWorkflowEnabled,
      env: context.deps.env,
      logger: context.logger,
    },
    stateRevision: record.stateRevision,
    target: persistedTarget,
    todos: persistedTodos,
    workspace: record.workspace,
  });
  phaseDurationsMs.buildSnapshot = Date.now() - buildStartedAt;
  const totalDurationMs = Date.now() - snapshotStartedAt;
  if (totalDurationMs >= SLOW_SNAPSHOT_LOG_THRESHOLD_MS) {
// In the SSH scenario, the slowness is concentrated in the session snapshot, but the old log only has the total time taken.
// Record each sub-phase when the threshold is exceeded to facilitate the differentiation of sqlite waiting, message reading and projection construction.
    context.logger?.warn("ZCode Protocol session snapshot slow", {
      durationMs: totalDurationMs,
      event: "zcode_protocol.session_snapshot.slow",
      messageCount: messages.length,
      module: "bootstrap.zcode_protocol",
      phaseDurationsMs,
      sessionId: record.app.sessionId,
      status: "completed",
      workspaceKey: record.workspace.workspaceKey,
      workspacePath: record.workspace.workspacePath,
    });
  }
  return { snapshot: builtSnapshot, phaseDurationsMs };
}

async function readSnapshotTarget(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
) {
  try {
    return await record.app.readTarget();
  } catch (error) {
// goal is the persistent business state of session_target; snapshot cannot be swallowed silently when recovery fails.
// Otherwise the UI will mistakenly think that the historical session has no target. Here the runtime projection is kept and protocol layer logs are recorded.
    context.logger?.warn("Failed to read persisted session goal for protocol snapshot", {
      errorMessage: error instanceof Error ? error.message : String(error),
      event: "zcode_protocol.snapshot.target_read_failed",
      sessionId: record.app.sessionId,
    });
    return undefined;
  }
}

async function readSnapshotTodos(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
) {
  try {
    return await record.app.readTodos();
  } catch (error) {
// todo is the authoritative short-term plan in DB; if recovery read fails, the UI needs to see the clear log,
// However, the protocol snapshot can still use the message history to continue restoring the first screen.
    context.logger?.warn("Failed to read persisted session todos for protocol snapshot", {
      errorMessage: error instanceof Error ? error.message : String(error),
      event: "zcode_protocol.snapshot.todos_read_failed",
      sessionId: record.app.sessionId,
    });
    return [];
  }
}

async function readSessionTargetCompletionVerificationEvents(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
): Promise<SessionEvent[]> {
  const sessionStore = context.deps.sessionStore;
  if (!sessionStore?.sessionEntries) {
    return [];
  }
  try {
    const entries = await sessionStore.sessionEntries({
      sessionID: record.app.sessionId as SessionId,
      type: SESSION_ENTRY_TARGET_COMPLETION_VERIFICATION,
    });
    return entries
      .map((entry, index) =>
        sessionEntryToTargetCompletionVerificationEvent(entry, record.app.sessionId, index),
      )
      .filter((event): event is SessionEvent => event !== null);
  } catch (error) {
// goal verifier timeline is a persistent fact for UI rounds; cannot be affected by read failure
// The session body is restored, but the log must be left, otherwise it will be difficult to troubleshoot packet loss after cold start.
    context.logger?.warn("Failed to read persisted goal verification entries", {
      errorMessage: error instanceof Error ? error.message : String(error),
      event: "zcode_protocol.snapshot.goal_verification_entries_read_failed",
      sessionId: record.app.sessionId,
    });
    return [];
  }
}

function sessionEntryToTargetCompletionVerificationEvent(
  entry: {
    id: string;
    sessionID: SessionId;
    time: { created: number; updated: number };
    data: unknown;
  },
  fallbackSessionId: string,
  index: number,
): SessionEvent | null {
  const data = asRecord(entry.data);
  const payload = readTargetCompletionVerificationPayload(data.payload);
  if (!payload) {
    return null;
  }
  const eventId = stringValue(data.eventId) ?? entry.id;
  const traceId = stringValue(data.traceId) ?? "trace_restored_goal_verification";
  const turnId = stringValue(data.turnId);
  return {
    id: eventId as EventId,
    payload,
    sequenceNumber: numberValue(data.sequenceNumber) ?? index + 1,
    sessionId: (entry.sessionID ?? fallbackSessionId) as SessionId,
    timestamp: new Date(entry.time.updated || entry.time.created),
    traceId: traceId as TraceId,
    ...(turnId ? { turnId: turnId as TurnId } : {}),
    type: SessionEventType.TargetCompletionVerification,
  };
}

function readTargetCompletionVerificationPayload(
  value: unknown,
): TargetCompletionVerificationPayload | null {
  const record = asRecord(value);
  const targetId = stringValue(record.targetId);
  const verificationId = stringValue(record.verificationId);
  const status = stringValue(record.status);
  if (!targetId || !verificationId || !isTargetCompletionVerificationStatus(status)) {
    return null;
  }
  const verification = asGoalCompletionVerification(record.verification);
  const goalIteration = numberValue(record.goalIteration);
  const anchorAssistantMessageId = stringValue(record.anchorAssistantMessageId);
  const anchorTurnId = stringValue(record.anchorTurnId);
  return {
    targetId,
    verificationId,
    status,
    ...(verification ? { verification } : {}),
    ...(goalIteration ? { goalIteration } : {}),
    ...(anchorAssistantMessageId
      ? { anchorAssistantMessageId: anchorAssistantMessageId as MessageId }
      : {}),
    ...(anchorTurnId ? { anchorTurnId: anchorTurnId as TurnId } : {}),
  };
}

function isTargetCompletionVerificationStatus(
  status: string | undefined,
): status is TargetCompletionVerificationPayload["status"] {
  return (
    status === "started" ||
    status === "completed" ||
    status === "failed_closed" ||
    status === "cancelled"
  );
}

function asGoalCompletionVerification(
  value: unknown,
): TargetCompletionVerificationPayload["verification"] | undefined {
  const record = asRecord(value);
  if (typeof record.passed !== "boolean") {
    return undefined;
  }
  const reason = stringValue(record.reason);
  if (!reason) {
    return undefined;
  }
  return {
    nextAction: stringValue(record.nextAction),
    passed: record.passed,
    reason,
  };
}

function limitMessages<T>(messages: T[], limit?: number): T[] {
  return limit && limit > 0 ? messages.slice(-limit) : messages;
}

async function readSessionMessages(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
) {
  if (!context.deps.sessionStore) return [];
  return await context.deps.sessionStore.messages({
    sessionID: record.app.sessionId as SessionId,
  });
}

async function readSessionContextUsageBreakdownEvents(
  record: ZCodeProtocolSessionRecord,
): Promise<SessionEvent[]> {
  const events = await record.eventStore.getEvents(record.app.sessionId as SessionId);
  return events.filter((event) => event.type === SessionEventType.ModelComplete);
}

async function readActiveSessionMessages(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
): Promise<MessageWithParts[]> {
  const [messages, session, rewindEvents] = await Promise.all([
    readSessionMessages(context, record),
    getPersistedSession(context, record.app.sessionId),
    readSessionRewindEvents(record),
  ]);
  return projectActiveSessionMessages(messages, session, rewindEvents);
}

async function readSessionRewindEvents(
  record: ZCodeProtocolSessionRecord,
): Promise<SessionEvent[]> {
  const events = await record.eventStore.getEvents(record.app.sessionId as SessionId);
  return events.filter((event) => event.type === SessionEventType.RewindTriggered);
}

function projectActiveSessionMessages(
  messages: MessageWithParts[],
  session: SessionInfo | null | undefined,
  rewindEvents: readonly SessionEvent[],
): MessageWithParts[] {
  const activeConversationRewinds: ActiveConversationRewindPayload[] = [];
  const sessionRewindPayload = activeConversationRewindPayloadFromSession(session);
  if (sessionRewindPayload) {
    activeConversationRewinds.push(sessionRewindPayload);
  }
  let activeMessages = applyRewindBranch(messages, messages, {
    createdMessageId: sessionRewindPayload?.createdMessageId,
    keptMessageIds: session?.revert?.keptMessageIDs,
    targetMessageId: sessionRewindPayload?.targetMessageId,
  });

  for (const event of rewindEvents) {
    const payload = parseActiveConversationRewindPayload(event);
    if (!payload?.targetMessageId) {
      continue;
    }
    activeConversationRewinds.push(payload);
    activeMessages = applyRewindBranch(activeMessages, messages, payload);
  }

  return filterRewindCommandAckMessages(activeMessages, activeConversationRewinds);
}

type ActiveConversationRewindPayload = {
  createdMessageId: MessageId;
  targetMessageId: MessageId;
};

function activeConversationRewindPayloadFromSession(
  session: SessionInfo | null | undefined,
): ActiveConversationRewindPayload | null {
  const createdMessageId = session?.revert?.createdMessageID;
  const targetMessageId = session?.revert?.targetMessageID;
  if (!createdMessageId || !targetMessageId) {
    return null;
  }
  return { createdMessageId, targetMessageId };
}

function parseActiveConversationRewindPayload(
  event: SessionEvent,
): ActiveConversationRewindPayload | null {
  try {
    const payload = parseRewindTriggeredPayload(event.payload);
    if (
      payload.strategy !== RewindStrategy.ActiveChain ||
      (payload.scope !== RewindScope.Conversation && payload.scope !== RewindScope.Both) ||
      !payload.createdMessageId ||
      !payload.targetMessageId
    ) {
// core will record unavailable events for unavailable rewinds, but such events do not have createdMessageId.
// It is not the starting point of a new active branch. The protocol projection cannot use it to clip messages, otherwise continuous editing will empty the visible messages.
      return null;
    }
    return {
      createdMessageId: payload.createdMessageId,
      targetMessageId: payload.targetMessageId,
    };
  } catch {
    return null;
  }
}

function filterRewindCommandAckMessages(
  messages: MessageWithParts[],
  activeConversationRewinds: readonly ActiveConversationRewindPayload[],
): MessageWithParts[] {
  if (activeConversationRewinds.length === 0) {
    return messages;
  }
  const targetByNoticeMessageId = new Map<MessageId, MessageId>();
  for (const rewind of activeConversationRewinds) {
    targetByNoticeMessageId.set(rewind.createdMessageId, rewind.targetMessageId);
  }
  return messages.filter((message) => !isRewindCommandAckMessage(message, targetByNoticeMessageId));
}

function isRewindCommandAckMessage(
  message: MessageWithParts,
  targetByNoticeMessageId: ReadonlyMap<MessageId, MessageId>,
): boolean {
  if (message.info.role !== "assistant") {
    return false;
  }
  const targetMessageId = targetByNoticeMessageId.get(message.info.parentID);
  if (!targetMessageId) {
    return false;
  }
  const text = message.parts
    .filter((part) => part.type === "text")
    .map((part) => part.text)
    .join("")
    .trim();
  const ackText = `Rewound conversation to before message ${targetMessageId}.`;
// Internal `/rewind conversation <messageId>`'s success receipt may be completed by old event projection
// assistant text. It's just a control command ACK for edit/retry and can no longer be rendered to the user when refreshed or restored.
  return text === ackText || text.endsWith(`\n${ackText}`);
}

function isUserVisibleSessionMessage(message: MessageWithParts): boolean {
  if (message.info.role !== "user") {
    return true;
  }
  const policy = getConversationMessageProjectionPolicy(message);
  return policy === "realUserInput" || policy === "timelineOnly";
}

function applyRewindBranch(
  activeMessages: MessageWithParts[],
  allMessages: MessageWithParts[],
  options: {
    createdMessageId?: MessageId;
    keptMessageIds?: readonly MessageId[];
    targetMessageId?: MessageId;
  },
): MessageWithParts[] {
  if (!options.targetMessageId) {
    return activeMessages;
  }

  if (options.keptMessageIds) {
// After continuous rewind, the target prefix in the ledger is not equal to the active branch prefix.
// Prioritize using the keptMessageIDs saved by the runtime during rewind to prevent old branches from being resurrected in snapshot/readMessages.
    const messagesById = new Map(allMessages.map((message) => [message.info.id, message]));
    const keptMessages = options.keptMessageIds
      .map((messageId) => messagesById.get(messageId))
      .filter((message): message is MessageWithParts => message !== undefined);
    if (!options.createdMessageId) {
      return keptMessages;
    }

    const createdIndex = allMessages.findIndex(
      (message) => message.info.id === options.createdMessageId,
    );
    return createdIndex >= 0 ? [...keptMessages, ...allMessages.slice(createdIndex)] : keptMessages;
  }

  const targetIndex = activeMessages.findIndex(
    (message) => message.info.id === options.targetMessageId,
  );
  if (targetIndex < 0) {
    return activeMessages;
  }
  const keptMessages = activeMessages.slice(0, targetIndex);
  if (!options.createdMessageId) {
    return keptMessages;
  }

  const createdIndex = allMessages.findIndex(
    (message) => message.info.id === options.createdMessageId,
  );
  if (createdIndex < 0) {
    return keptMessages;
  }

// The persistent ledger of conversation rewind will retain the old branch that was rolled back.
// UI snapshot/readMessages must project the active branch, otherwise you will see both the original text and the new message after editing and reposting.
  return [...keptMessages, ...allMessages.slice(createdIndex)];
}

async function getPersistedSession(
  context: ZCodeProtocolAgentServerContext,
  sessionId: string,
): Promise<SessionInfo | null> {
  const sessionStore = context.deps.sessionStore;
  if (!sessionStore) return null;
  const session = await sessionStore.getSession(sessionId as SessionId);
  if (!session) return null;
// Protocol cold recovery first uses session.path to create a runtime, and the old version of identity/cwd mixed data must be cleared before materialization.
  return await repairPersistedRemoteSessionPaths(sessionStore, session, {
    onPersistenceFailure: (error) => {
      context.logger?.warn("Session path repair persistence failed; using in-memory repair", {
        errorMessage: error instanceof Error ? error.message : String(error),
        event: "zcode_protocol.session.path_repair.persist_failed",
        sessionId,
      });
    },
  });
}

/**
* Pure configuration class mutation reason: only the session selection is changed, user activity is not counted.
* Same rule as isConfigOnlySessionEvent - record.updatedAt is sessions-index
* Source of fact for lastActivityAt / snapshot.session.updatedAt (task index sort time),
* Cutting models/cutting thinking depth/cutting modes cannot push tasks to the top of the list.
 */
const CONFIG_ONLY_MUTATION_REASONS = new Set([
  "model_changed",
  "thought_level_changed",
  "mode_changed",
]);

export async function afterStateMutation(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
  reason: string,
) {
  if (!record.activeAbortController) {
// The provider registry may delete the current model during a turn run. Requests in transit cannot be rewritten during updates; turn/compact/
// goal continuation releases the active lock and passes through this safety boundary uniformly, and immediately completes the same Agent fallback.
// There is no need to wait for the user to send next time, and no need to transfer the overall responsibility to the renderer again.
    await ensureSessionModelAvailable(context, record);
  }
  record.stateRevision++;
  const configOnlyMutation = CONFIG_ONLY_MUTATION_REASONS.has(reason);
  if (!configOnlyMutation) {
    record.updatedAt = Date.now();
  }
  const stateSnapshot = await snapshot(context, record, undefined, {
    modelAvailability: "current",
  });
  emitStateUpdated(context, record, reason, stateSnapshot.settings);
  return stateSnapshot;
}

function afterPromptAccepted(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
  reason: string,
) {
  record.stateRevision++;
  record.updatedAt = Date.now();
  emitStateUpdated(context, record, reason, { status: "running" });
  return {
    accepted: true as const,
    sessionId: record.app.sessionId,
    stateRevision: record.stateRevision,
  };
}

function emitStateUpdated(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
  reason: string,
  patch: unknown,
): void {
  const notification: ZCodeStateUpdatedNotification = {
    patch,
    reason,
    revision: record.stateRevision,
    scope: "session",
    sessionId: record.app.sessionId,
    type: "state.updated",
    workspace: record.workspace,
  };
  context.notify({ method: "state.updated", params: notification });
}
