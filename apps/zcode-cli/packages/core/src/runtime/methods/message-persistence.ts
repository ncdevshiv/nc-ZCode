import type { RuntimeInputPresentation } from "@zcode/contracts";
import { createModelId, createModelProviderId } from "@zcode/contracts";
import { SessionEventType, createPartId, traceContextToLogContext } from "../deps.js";
import type {
  EnvInfo,
  MessageId,
  MessagePart,
  MessageVisibility,
  Model,
  SessionId,
  SessionProjection,
  SessionStorePort,
  TraceContext,
  SyntheticUserMessageSource,
  TurnInputIntentMetadata,
  TurnExecutionKind,
} from "../deps.js";
import { emptyTokenUsageInfo, toTokenUsageInfo } from "../helpers/index.js";
import type { ResolvedTurnAttachment } from "../types.js";
import type { AgentRuntimeInternal } from "../internal.js";
import {
  buildSyntheticUserNoticeMessageMetadata,
  buildSyntheticUserNoticePartMetadata,
  buildSyntheticUserNoticeSemantics,
} from "./synthetic-notice-metadata.js";
import { buildPersistedConversationInputIntent } from "./input-intent-persistence.js";
import { buildProjectionAnchor, mapSyntheticSourceToAnchorOrigin } from "./projection-anchor.js";

export async function persistUserPrompt(
  this: AgentRuntimeInternal,
  messageID: MessageId,
  input: string,
  attachments: ResolvedTurnAttachment[] | undefined,
  traceContext: TraceContext,
  options?: {
    /**
* drain injected input puts delivery semantics into persistent facts (metadata.turnSteerDelivery),
* Cold recovery restores the segmentation of "queue=independent round / guide=inline current round" accordingly, which is consistent with the live structure.
     */
    steerDelivery?: "guide" | "queue";
    inputPresentation?: RuntimeInputPresentation;
    /**
* (promotion atomicity): When the ledger id is given and the store supports it, the ledger is set to promoted
* Follow the same process as message/parts persistence - to prevent "queue has been consumed but transcript is not available"
* User message" orphan window (old drain cross-store no transactions).
     */
    sessionInputId?: string;
/** V4 command idempotent anchor; must come from CLI admission, and cannot change the id when draining. */
    sourceCommandId?: string;
    clientId?: string;
    intent?: TurnInputIntentMetadata;
/** Execution semantics required for cold recovery; cannot exist only in live TurnStarted. */
    executionKind?: TurnExecutionKind;
/** Starting point for engine appended text; also stored for cold recovery. */
    epilogueStart?: number;
  },
): Promise<void> {
  this.latestConversationMessageId = messageID;
  if (!this.sessionStore) return;

  const created = Date.now();
  const tools = Object.fromEntries(this.getTools().map((tool) => [tool.name, true]));
  const conversationInputIntent = buildPersistedConversationInputIntent(
    input,
    options?.intent,
    "drained",
  );
  const message: Parameters<SessionStorePort["saveMessage"]>[0] = {
    id: messageID,
    sessionID: this.sessionId,
    role: "user",
    time: {
      created,
    },
    agent: this.config.agentName ?? "zcode-agent",
    modelSelection: this.getSessionModelSelection(),
    contextSnapshot: buildPersistedContextSnapshot(this.config.envInfo),
    semantics: {
      origin: "real_user",
      kind: "user_prompt",
      uiVisibility: "visible",
      providerVisibility: "visible",
      transcriptVisibility: "visible",
    },
    anchor: buildProjectionAnchor(
      traceContext,
      "realUser",
      options?.intent?.sourceCommandId ?? options?.sourceCommandId,
    ),
    system: this.config.systemPrompt,
    tools,
    ...(options?.inputPresentation ||
    options?.steerDelivery ||
    options?.clientId ||
    options?.intent ||
    options?.executionKind ||
    options?.epilogueStart !== undefined
      ? {
          metadata: {
            ...(options?.steerDelivery ? { turnSteerDelivery: options.steerDelivery } : {}),
            ...(options?.inputPresentation ? { inputPresentation: options.inputPresentation } : {}),
            ...(options?.intent ? { inputIntent: options.intent } : {}),
            ...(conversationInputIntent ? { conversationInputIntent } : {}),
            ...((options?.intent?.clientId ?? options?.clientId)
              ? { inputClientId: options?.intent?.clientId ?? options?.clientId }
              : {}),
            ...(options?.executionKind ? { executionKind: options.executionKind } : {}),
            ...(options?.epilogueStart === undefined
              ? {}
              : { epilogueStart: options.epilogueStart }),
          },
        }
      : {}),
  };
  const parts: MessagePart[] = [
    {
      id: createPartId(),
      sessionID: this.sessionId,
      messageID,
      type: "text",
      text: input,
      time: {
        start: created,
        end: created,
      },
    },
    ...(attachments ?? []).map(
      (attachment): MessagePart => ({
        id: createPartId(),
        sessionID: this.sessionId,
        messageID,
        type: "file",
        mime: attachment.mime,
        filename: attachment.filename,
        url: attachment.url,
        source: attachment.source,
        metadata: attachment.metadata,
      }),
    ),
  ];

  if (options?.sessionInputId && this.sessionStore.promoteSessionInput) {
    await this.sessionStore.promoteSessionInput({
      id: options.sessionInputId,
      sessionID: this.sessionId,
      message,
      parts,
    });
    this.logger?.debug("Session input promoted", {
      ...traceContextToLogContext(traceContext),
      event: "session_input.promoted",
      messageId: messageID,
      module: "core.runtime",
      sessionInputId: options.sessionInputId,
      status: "completed",
    });
    const sourceCommandId =
      options.intent?.sourceCommandId ?? options.sourceCommandId ?? options.sessionInputId;
// Promotion event is sent after the transaction is submitted: gateway can only release the live-input pin at this boundary.
// If it is canceled during queue remove/TurnStarted, LRU churn will be transferred before the transcript is placed.
// commands/query degenerates into unknown and executes the same input repeatedly.
    await this.appendEvent(
      this.createEvent(
        SessionEventType.SessionInputPromoted,
        {
          pendingInputId: options.sessionInputId,
          sourceCommandId,
          messageId: messageID,
        },
        traceContext,
      ),
      traceContext,
    );
    return;
  }

  await this.persistMessage(message, traceContext);
  for (const part of parts) {
    await this.persistPart(part, traceContext);
  }
}

export async function persistSyntheticUserNotice(
  this: AgentRuntimeInternal,
  messageID: MessageId,
  text: string,
  traceContext: TraceContext,
): Promise<void> {
  await this.persistSyntheticUserNoticeForSession({
    messageID,
    sessionId: this.sessionId,
    source: "rewind",
    text,
    traceContext,
  });
}

export async function persistSyntheticUserNoticeForSession(
  this: AgentRuntimeInternal,
  options: {
    messageID: MessageId;
    sessionId: SessionId;
    source: SyntheticUserMessageSource;
    text: string;
    traceContext: TraceContext;
/** Additional structured metadata will be merged with `{ source }` and written to part.metadata for the UI to identify the message type. */
    metadata?: Record<string, unknown>;
    visibility?: MessageVisibility;
  },
): Promise<void> {
  if (options.sessionId === this.sessionId) {
    this.latestConversationMessageId = options.messageID;
  }
  if (!this.sessionStore) return;

  const created = Date.now();
  const visibility = options.visibility ?? "model-only";
  const messageMetadata = buildSyntheticUserNoticeMessageMetadata(
    options.source,
    visibility,
    options.metadata,
  );
  const partMetadata = buildSyntheticUserNoticePartMetadata(
    options.source,
    visibility,
    options.metadata,
  );
  await this.persistMessage(
    {
      id: options.messageID,
      sessionID: options.sessionId,
      role: "user",
      time: {
        created,
      },
      agent: this.config.agentName ?? "zcode-agent",
      metadata: messageMetadata,
      modelSelection: this.getSessionModelSelection(),
      semantics: buildSyntheticUserNoticeSemantics(options.source, visibility),
      anchor: buildProjectionAnchor(
        options.traceContext,
        mapSyntheticSourceToAnchorOrigin(options.source),
      ),
      source: options.source,
      system: this.config.systemPrompt,
      synthetic: true,
      tools: Object.fromEntries(this.getTools().map((tool) => [tool.name, true])),
      visibility,
    },
    options.traceContext,
  );
  await this.persistPart(
    {
      id: createPartId(),
      sessionID: options.sessionId,
      messageID: options.messageID,
      type: "text",
      text: options.text,
      synthetic: true,
      time: {
        start: created,
        end: created,
      },
      metadata: partMetadata,
    },
    options.traceContext,
  );
}

export async function persistAssistantMessage(
  this: AgentRuntimeInternal,
  messageID: MessageId,
  parentID: MessageId,
  created: number,
  update:
    | {
        completed?: number;
        error?: { name: string; data?: Record<string, unknown> };
        finish?: string;
        tokens?: ReturnType<typeof toTokenUsageInfo>;
      }
    | undefined,
  traceContext: TraceContext,
  model?: Model,
): Promise<void> {
  this.latestConversationMessageId = messageID;
  this.latestAssistantMessageId = messageID;
  if (traceContext.turnId) {
    this.latestAssistantTurnId = traceContext.turnId;
  }
  if (!this.sessionStore) return;
  const selection = this.getSessionModelSelection();
  const providerId =
    model?.providerId ?? (selection && createModelProviderId(selection.providerId));
  const modelId = model?.modelId ?? (selection && createModelId(selection.modelId));

  await this.persistMessage(
    {
      id: messageID,
      sessionID: this.sessionId,
      role: "assistant",
      time: {
        created,
        completed: update?.completed,
      },
      error: update?.error,
      parentID,
// The default model may be switched during the request; the model request path must be explicitly passed in to generate this
// Message model. The default value is only reserved for existing synthetic/fallback paths that do not go through the model results.
      modelId,
      providerId,
      mode: this.config.mode ?? "build",
      planEnabled: this.getPlanEnabled(),
      agent: this.config.agentName ?? "zcode-agent",
      path: {
        cwd: this.workingDirectory,
// cwd can change with Bash cd, root must retain the session's initial workspace identity.
        root: this.workspaceRoot,
      },
      cost: 0,
      tokens: update?.tokens ?? emptyTokenUsageInfo(),
      finish: update?.finish,
      semantics: {
        origin: "agent_runtime",
        kind: "assistant_response",
        uiVisibility: "visible",
        providerVisibility: "visible",
        transcriptVisibility: "visible",
      },
      anchor: buildProjectionAnchor(traceContext),
    },
    traceContext,
  );
}

export async function persistMessage(
  this: AgentRuntimeInternal,
  input: Parameters<SessionStorePort["saveMessage"]>[0],
  traceContext: TraceContext,
  copyFrom?: Parameters<SessionStorePort["saveMessage"]>[1],
): Promise<void> {
  if (!this.sessionStore) return;
  await this.sessionStore.saveMessage(input, copyFrom);
  this.logger?.debug("Session message persisted", {
    ...traceContextToLogContext(traceContext),
    event: "session.message.persisted",
    messageId: input.id,
    module: "core.runtime",
    role: input.role,
    status: "completed",
  });
}

export async function persistPart(
  this: AgentRuntimeInternal,
  input: MessagePart,
  traceContext: TraceContext,
  copyFrom?: Parameters<SessionStorePort["savePart"]>[1],
): Promise<void> {
  if (!this.sessionStore) return;
  await this.sessionStore.savePart(input, copyFrom);
  this.logger?.debug("Session part persisted", {
    ...traceContextToLogContext(traceContext),
    event: "session.part.persisted",
    messageId: input.messageID,
    module: "core.runtime",
    partId: input.id,
    partType: input.type,
    status: "completed",
  });
}

function buildPersistedContextSnapshot(envInfo: EnvInfo | undefined):
  | {
      envInfo: EnvInfo;
    }
  | undefined {
  if (!envInfo) {
    return undefined;
  }

  return { envInfo: { ...envInfo } };
}

export async function rebuildProjection(this: AgentRuntimeInternal): Promise<SessionProjection> {
  const events = await this.eventStore.getEvents(this.sessionId);
  return this.eventReducer.reduce(events);
}
