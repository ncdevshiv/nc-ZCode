import {
  SessionEventType,
  createChildTraceContext,
  createMessageId,
  createPartId,
  createTurnId,
} from "../deps.js";
import type {
  MessageId,
  SyntheticUserMessageSource,
  TraceContext,
  TurnInputIntentMetadata,
  WorkflowLaunchMeta,
} from "../deps.js";
import { buildUserContentFromTurn } from "../helpers/index.js";
import { realUserRuntimeMetadata } from "../../agent/message-history.js";
import type { AgentRuntimeInternal } from "../internal.js";
import type { ControlOnlyTurnRuntimeCommand } from "../command-queue.js";
import { buildProjectionAnchor } from "./projection-anchor.js";
import { maybeStartGoalSummaryTitleGeneration } from "./goal-summary-title.js";
import { maybeStartSessionTitleGeneration } from "./session-title.js";

/**
* Common boundary for controlOnly user wheels (shared by `/goal` external queries and hubs launching workflows directly).
 *
* Both entrances do not use the ordinary `executeTurn`, but both have to drop a persistent message visible to the user, role=user**, and feed it into
* runtime history (can be seen by the model in the next turn), and add a set of complete turn boundaries without model output
* (`TurnStarted{executionKind:"controlOnly"}` + `TurnComplete{response:""}`) - otherwise live
* ProductProjection cannot receive TurnStarted, query will not appear until cold recovery, and the 0ms control wheel will be changed by modelChange
* The marker is incorrectly displayed as "Worked for 1 second". The only real difference is how the message lands (real user prompt vs.
* synthetic user message of `workflowLaunch` metadata) and title/subsequent side effects, each called back by `persistMessage`
* Injected with `afterTurnBoundary` callback.
 */
export async function emitControlOnlyUserTurn(
  this: AgentRuntimeInternal,
  options: {
    messageId: MessageId;
/** The first input title seed of `ensureSessionPersisted` (canonicalized objective for /goal, workflow name for startup). */
    titleInput: string;
/** Visible text into the runtime history (the model reads it next turn). */
    historyText: string;
/** `TurnStarted.input` (downgraded rendering of old client/TUI). */
    turnInput: string;
    traceContext: TraceContext;
    inputId?: string;
    inputSource?: SyntheticUserMessageSource;
    workflowLaunch?: WorkflowLaunchMeta;
    intent?: TurnInputIntentMetadata;
/** Drop this user message (real or synthetic) - the only substantial difference between the two paths. */
    persistMessage: () => Promise<void>;
/** Optional side effects (title sidecar, etc.) after the session is first dropped and before turnNumber is advanced. */
    afterTurnBoundary?: () => void;
  },
): Promise<void> {
  const { messageId, traceContext } = options;
// /goal entry may be written into runtime history earlier than the first turn; if addUser is added first,
// Subsequent lazy context init will rebuild messageHistory and flush this real user query.
  await this.ensureContextInitialized(traceContext);
  await this.ensureSessionPersisted(options.titleInput, traceContext);
// This type of entry itself does not use ordinary submitPrompt, but it carries the user's true intention. Write here at the same time
// runtime history and session store make model context, desktop continuous, and mobile phone replayable snapshot
// Use the same visible user intent.
  this.messageHistory.addUser(
    buildUserContentFromTurn(options.historyText, []),
    realUserRuntimeMetadata(),
  );
  await options.persistMessage();
// If you don’t enter the entrance of executeTurn, only transcript will be dropped into the database, and live ProductProjection will not receive it.
// TurnStarted, causing the query to wait for cold recovery before appearing. Here is a complete set of model-free output for this real user input.
// turn boundary; subsequent (goal continuation / notification-driven round) will still open another turn, and the second user bubble will not be generated.
  const turnId = createTurnId();
  const turnTraceContext = createChildTraceContext(traceContext, {
    turnId,
    attributes: { turnNumber: this.turnNumber },
  });
  await this.appendEvent(
    this.createEvent(
      SessionEventType.TurnStarted,
      {
        turnNumber: this.turnNumber,
        input: options.turnInput,
        messageId,
        ...(options.inputId ? { inputId: options.inputId } : {}),
// It can be seen that the query needs to be turned independently to be displayed in real time, but it does not execute the Agent itself. The status is missing
// At this time, the modelChange marker will cause the UI to display the 0ms control loop as "Worked for 1 second" and briefly overwrite it
        // session running/activeWorks。
        executionKind: "controlOnly",
        ...(options.inputSource ? { inputSource: options.inputSource } : {}),
        ...(options.workflowLaunch ? { workflowLaunch: options.workflowLaunch } : {}),
        ...(options.intent ? { intent: options.intent } : {}),
      },
      turnTraceContext,
    ),
    turnTraceContext,
  );
  await this.appendEvent(
    this.createEvent(
      SessionEventType.TurnComplete,
      {
        response: "",
        tokenCount: 0,
        toolCallCount: 0,
        duration: 0,
        resultType: "success",
        ...(options.inputId ? { inputId: options.inputId } : {}),
      },
      turnTraceContext,
    ),
    turnTraceContext,
  );
  options.afterTurnBoundary?.();
  this.turnNumber += 1;
  this.messageHistory.setCacheMiss();
}

/**
* Drop the hub to directly launch the workflow's launch wheel user message.
 *
* It is a message that is **synthetic but semantically belongs to the user's real action**: `synthetic: true` + New
* `source: "workflow_launch"`, but with `origin: "real_user"` / `kind: "user_prompt"` and
* ui/provider/transcript is visible from all three sides - the user clicks "Run" in the hub, which is his true intention, just the GUI
* Use `metadata.workflowLaunch` to draw the tail run card instead of displaying this text. Metadata is also entered into message metadata (cold recovery
* source) and TurnStarted payload (live projection source, written by the caller), both are the same.
 */
export async function persistWorkflowLaunchUserMessage(
  this: AgentRuntimeInternal,
  options: {
    messageID: MessageId;
    text: string;
    meta: WorkflowLaunchMeta;
    traceContext: TraceContext;
  },
): Promise<void> {
  this.latestConversationMessageId = options.messageID;
  if (!this.sessionStore) return;

  const created = Date.now();
  await this.persistMessage(
    {
      id: options.messageID,
      sessionID: this.sessionId,
      role: "user",
      time: { created },
      agent: this.config.agentName ?? "zcode-agent",
// Cold recovery source: transcript-hydration data source === "workflow_launch" + metadata.workflowLaunch
// Rebuild the startup card row.
      metadata: { workflowLaunch: options.meta },
      modelSelection: this.getSessionModelSelection(),
      semantics: {
// The user's real action: It is not the system reminder of agent_runtime. The model will read it with the real user prompt in the next round.
        origin: "real_user",
        kind: "user_prompt",
        source: "workflow_launch",
        uiVisibility: "visible",
        providerVisibility: "visible",
        transcriptVisibility: "visible",
      },
      anchor: buildProjectionAnchor(options.traceContext, "realUser"),
      source: "workflow_launch",
      system: this.config.systemPrompt,
      synthetic: true,
      tools: Object.fromEntries(this.getTools().map((tool) => [tool.name, true])),
      visibility: "user-visible",
    },
    options.traceContext,
  );
  await this.persistPart(
    {
      id: createPartId(),
      sessionID: this.sessionId,
      messageID: options.messageID,
      type: "text",
      text: options.text,
      synthetic: true,
      time: { start: created, end: created },
      metadata: { source: "workflow_launch", visibility: "user-visible" },
    },
    options.traceContext,
  );
}

/**
* Run a queued controlOnly wheel ({@link ControlOnlyTurnRuntimeCommand}): the same way as the central start wheel——
* synthetic `workflow_launch` user message + metadata + complete turn boundary, but the timing is determined by the queue.
 */
export async function runControlOnlyTurnCommand(
  this: AgentRuntimeInternal,
  command: ControlOnlyTurnRuntimeCommand,
): Promise<void> {
  const messageId = createMessageId();
  await emitControlOnlyUserTurn.call(this, {
    messageId,
    titleInput: command.titleInput,
    historyText: command.text,
    turnInput: command.text,
    traceContext: command.traceContext,
    ...(command.inputId === undefined ? {} : { inputId: command.inputId }),
    inputSource: "workflow_launch",
    workflowLaunch: command.workflowLaunch,
    persistMessage: () =>
      persistWorkflowLaunchUserMessage.call(this, {
        messageID: messageId,
        text: command.text,
        meta: command.workflowLaunch,
        traceContext: command.traceContext,
      }),
  });
}

export async function recordExternalUserPrompt(
  this: AgentRuntimeInternal,
  input: string,
  options?: {
    goalSummaryTargetID?: string;
    traceContext?: TraceContext;
    intent?: TurnInputIntentMetadata;
  },
): Promise<MessageId> {
  const traceContext = options?.traceContext ?? this.rootTraceContext;
  const canonicalInput = options?.intent?.text?.trim() || input;
  const messageId = createMessageId();
  await emitControlOnlyUserTurn.call(this, {
    messageId,
    titleInput: canonicalInput,
    historyText: input,
    turnInput: input,
    traceContext,
    inputId: options?.intent?.sourceCommandId,
    intent: options?.intent,
// The /goal command itself does not use the ordinary submitPrompt, but the objective of the goal set for the first time is the user's real
// query; automatic continuation reminder is still marked as model-only by runtime.
    persistMessage: () =>
      this.persistUserPrompt(messageId, input, undefined, traceContext, {
        intent: options?.intent,
        sessionInputId: options?.intent?.queueItemId,
        executionKind: "controlOnly",
      }),
    afterTurnBoundary: () => {
// The first query must start the title sidecar when turnNumber is still 0, otherwise the first round of gate will
// It misjudges it as a subsequent turn, only generates goal summaryTitle and retains first_input session title.
      const titleGenerationStarted = maybeStartSessionTitleGeneration.call(
        this,
        canonicalInput,
        messageId,
        traceContext,
        { goalSummaryTargetID: options?.goalSummaryTargetID },
      );
      if (!titleGenerationStarted && options?.goalSummaryTargetID) {
        maybeStartGoalSummaryTitleGeneration.call(
          this,
          canonicalInput,
          options.goalSummaryTargetID,
          { traceContext },
        );
      }
    },
  });
  return messageId;
}
