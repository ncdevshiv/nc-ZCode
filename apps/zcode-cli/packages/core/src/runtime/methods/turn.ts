/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import { beginLocalTurnPreparation, type LocalTtftDetail } from "@zcode/contracts";
import { runtimeInputMetadata } from "../../agent/runtime-input-presentation.js";
import {
  CoreErrorType,
  HookEventName,
  SessionEventType,
  createChildTraceContext,
  createQueryId,
  createModelUsageSummaryFromEvents,
  createMessageId,
  createTurnId,
  runWithContextAsync,
  traceContextToLogContext,
  TurnMachineImpl,
  formatLocalIsoDate,
} from "../deps.js";
import type {
  HookRunResult,
  MessageId,
  MessagePart,
  QueryId,
  SessionEvent,
  SessionGoal,
  TurnState,
} from "../deps.js";
import {
  parseCompactCommand,
  parseRewindCommand,
  createTurnAbortScope,
  throwIfTurnAborted,
  createTurnFailureError,
  isTurnCancellationError,
  appendTurnOutcomeEvent,
  buildDateChangeReminderBody,
  buildRuntimeUserEntriesFromTurn,
  buildUserContentFromTurn,
  logResolvedTurnAttachments,
  resolveTurnAttachments,
  summarizeTurnAttachmentsForEvent,
  runtimeMetadataForSyntheticUserMessageSource,
} from "../helpers/index.js";
import type { ActiveTurnSteeringState, ExecuteTurnOptions, TurnResult } from "../types.js";
import type { ActiveTurnStartReservation } from "../types.js";
import type { AgentRuntimeInternal } from "../internal.js";
import { createRuntimeCommandId } from "../command-queue.js";
import type { PromptRuntimeCommand } from "../command-queue.js";
import { enqueueCancellableRuntimeCommand } from "./runtime-command-submit.js";
import { buildReferencedSessionContextReminderBody } from "../../session-context/read-session-context.js";
import { runRegularTurnLoop } from "./turn-loop.js";
import {
  maybeStartDeferredSessionTitleGeneration,
  maybeStartSessionTitleGeneration,
} from "./session-title.js";
import type { RegularTurnLoopState } from "./turn-loop-state.js";
import { finishOutputTokenRecovery } from "./turn-output-token-continuation.js";
import { recordTurnUsageFact } from "./usage-observability.js";
import { persistStableForkCompletionBoundary } from "./stable-fork-boundary.js";
import {
  closeGoalStateChangeReminderDeferral,
  openGoalStateChangeReminderDeferral,
} from "./goal-state-reminder.js";
import { scheduleProjectMemoryExtraction } from "../helpers/project-memory-extraction.js";
import { appendBrowserTurnScreenshot } from "./browser-turn-screenshot.js";
import { clearBrowserTurnState } from "../../repl/browser-turn-state.js";
import { applySubmissionExecutionState, createTurnModel } from "./turn-model.js";
import { rebuildContextPrefix } from "./context-refresh.js";

const TARGET_RUN_HEARTBEAT_MS = 15_000;

export async function executeTurn(
  this: AgentRuntimeInternal,
  input: string,
  attachments?: TurnState["attachments"],
  options?: ExecuteTurnOptions,
): Promise<TurnResult> {
  return await enqueueCancellableRuntimeCommand<TurnResult, PromptRuntimeCommand>(this, {
    abortSignal: options?.abortSignal,
    createCommand: ({ reject, resolve }) => ({
      attachments,
      createdAt: new Date(),
      id: createRuntimeCommandId(),
      input,
      mode: "prompt",
      options,
      priority: "next",
      reject,
      resolve,
      traceContext: options?.traceContext ?? this.rootTraceContext,
    }),
  });
}

export async function executeTurnCommand(
  this: AgentRuntimeInternal,
  input: string,
  attachments?: TurnState["attachments"],
  options?: ExecuteTurnOptions,
  startReservation?: ActiveTurnStartReservation,
): Promise<TurnResult> {
// Ordinary Turn used to read the Session Selection/output style after the asynchronous initialization was completed.
// The die cutting that occurs during initialization will cross the admission boundary, and the error will affect the Turn that has already started.
// Here the facts for this round are frozen before any await; subsequent configuration changes only apply to the next round.
  const admittedModelSelection = options?.intent?.modelSelection ?? this.getSessionModelSelection();
  const admittedOutputStyle = this.config.outputStyle;
  const compactInstructions = parseCompactCommand(input);
  const rewindCommand = parseRewindCommand(input);
  const turnId = startReservation?.turnId ?? createTurnId();
  const queryId = options?.queryId ?? (options?.inputId as QueryId | undefined) ?? createQueryId();
  const displayInput = options?.displayInput ?? input;
  const turnTraceContext =
    startReservation?.traceContext ??
    createChildTraceContext(options?.traceContext ?? this.rootTraceContext, {
      queryId,
      sessionId: this.sessionId,
      turnId,
      attributes: {
        turnNumber: this.turnNumber,
      },
    });
  const traceId = turnTraceContext.traceId;
  const turnStartedAtMs = Date.now();
  const targetRunInputID = options?.inputId ?? String(turnId);
  const events: SessionEvent[] = [];
  let turnMachine = TurnMachineImpl.create(this.sessionId, this.turnNumber, input, traceId, turnId);
  this.currentTurnFileChanges = new Map();
  if (!startReservation) this.reserveTurnStart(turnId, turnTraceContext, "regular");

  const turnAbortScope = createTurnAbortScope(options?.abortSignal);
  const turnAbortSignal = turnAbortScope.signal;
  let activeTurn: ActiveTurnSteeringState | undefined;
  let startedTarget: SessionGoal | null = null;
  let targetRunHeartbeat: ReturnType<typeof setInterval> | undefined;
  let userMessageId: MessageId | undefined;
  let loopState: RegularTurnLoopState | undefined;
  let shouldRetryTitleGenerationAfterTurn = false;
// The root cause candidate for online "has been working for N seconds" but has no final state is: await before inner Turn try/catch
//Refuse to pass through directly. Record the current stage and distinguish whether it has been processed by the inner layer to facilitate production log restoration stuck points.
  let turnPhase = "queued";
  let turnFailureHandled = false;
  let finishPreparation: () => void = () => {};
  const preparationStages: Record<string, LocalTtftDetail["stage"]> = {
    context_initialization: "context",
    session_start_hooks: "hooks",
    user_prompt_hooks: "hooks",
    session_persistence: "persistence",
    turn_started_event: "persistence",
    target_accounting: "persistence",
  };
  const startTurnPhase = (phase: string): number => {
    const stage = preparationStages[phase];
    finishPreparation =
      stage && stage !== "attempt" && stage !== "retry_wait" && stage !== "user_confirmation"
        ? beginLocalTurnPreparation(turnTraceContext, stage)
        : () => {};
    turnPhase = phase;
    const startedAt = Date.now();
    this.logger?.info("Turn phase started", {
      ...traceContextToLogContext(turnTraceContext),
      event: "turn.phase.started",
      module: "core.runtime",
      phase,
      status: "started",
    });
    return startedAt;
  };
  const completeTurnPhase = (phase: string, startedAt: number): void => {
    finishPreparation();
    this.logger?.info("Turn phase completed", {
      ...traceContextToLogContext(turnTraceContext),
      durationMs: Date.now() - startedAt,
      event: "turn.phase.completed",
      module: "core.runtime",
      phase,
      status: "completed",
    });
  };
  const turnTelemetry = this.agentTelemetry.turn({
    inputSource: options?.inputSource,
    traceContext: turnTraceContext,
    turnNumber: this.turnNumber,
  });

  const execute = () =>
    runWithContextAsync(turnTraceContext, async () => {
      const executionStartedAt = performance.timeOrigin + performance.now();
      beginLocalTurnPreparation(turnTraceContext, "execution")();
      throwIfTurnAborted(turnAbortSignal);
      let admittedModel;
      try {
        admittedModel =
          rewindCommand === null
            ? createTurnModel(this, {
                requestDependencies: options?.modelExecution?.requestDependencies,
                selection: admittedModelSelection,
              })
            : undefined;
      } catch (error) {
// Synchronization lag/model failure can create failures before the inner Turn try. Writing only logs will allow input to be accepted
// There is no final state, and no errors can be seen on the desktop or mobile phone; reuse outcome without waiting for synchronization or changing the original selection.
        turnFailureHandled = true;
        const coreError = createTurnFailureError(error, turnAbortSignal, "Model creation failed");
        await appendTurnOutcomeEvent(this, {
          coreError,
          events,
          durationMs: Date.now() - turnStartedAtMs,
          turnPhase: "model_creation",
          inputId: options?.inputId,
          traceContext: turnTraceContext,
          fallbackMessage: "Model creation failed",
          logEvent: "turn.failed",
          logLabel: "Turn",
        });
        throw coreError;
      }
      let phaseStartedAt = startTurnPhase("context_initialization");
      if (this.contextInitialized) {
// Each subsequent model step reprojects the Context according to the Model actually held by the step;
// Session Selection only determines which Model will be created in the future and cannot serve as an execution fact.
        rebuildContextPrefix(this, { model: admittedModel });
      } else {
// The first round of initialization has used admitted Model to construct and install the complete Context, and then rebuild
// The same Prefix will be constructed twice in a row. The uninitialized and initialized branches are mutually exclusive and are only constructed once per model step.
        await this.ensureContextInitialized(turnTraceContext, admittedModel);
      }
      completeTurnPhase("context_initialization", phaseStartedAt);
      throwIfTurnAborted(turnAbortSignal);
      phaseStartedAt = startTurnPhase("session_start_hooks");
      const sessionStartHookResult = await this.runSessionStartHooks(
        "startup",
        turnTraceContext,
        turnAbortSignal,
        admittedModel,
      );
      completeTurnPhase("session_start_hooks", phaseStartedAt);
      this.injectHookAdditionalContextIntoMessageHistory(
        HookEventName.SessionStart,
        sessionStartHookResult.additionalContexts,
      );

      if (compactInstructions !== null) {
        const compactModel = await applySubmissionExecutionState(
          this,
          options?.intent,
          turnTraceContext,
          options?.modelExecution,
          admittedModel,
        );
        return this.executeManualCompact(
          input,
          compactInstructions,
          turnId,
          turnTraceContext,
          turnAbortSignal,
          options?.inputId,
          compactModel,
        );
      }
      if (rewindCommand !== null) {
        return this.executeRewindCommand(
          input,
          rewindCommand,
          turnId,
          turnTraceContext,
          turnAbortSignal,
          options?.inputId,
        );
      }
      activeTurn = this.beginActiveTurn(turnId, turnTraceContext, "regular", true, {
        ...(options?.inputId === undefined ? {} : { inputId: options.inputId }),
      });
      this.logger?.info("Turn started", {
        ...traceContextToLogContext(turnTraceContext),
        event: "turn.started",
        inputLength: input.length,
        module: "core.runtime",
        status: "started",
      });

      turnMachine = new TurnMachineImpl(turnMachine.start());
      phaseStartedAt = startTurnPhase("session_persistence");
      await this.ensureSessionPersisted(displayInput, turnTraceContext);
// Execution-scoped temporary Provider (such as idle tasks) has its own model for this round and does not rewrite it
// Session Selection; ordinary Submission only applies its atomic selection when it actually starts running.
      const submissionModel = await applySubmissionExecutionState(
        this,
        options?.intent,
        turnTraceContext,
        options?.modelExecution,
        admittedModel,
      );
      startedTarget = await this.readSessionTargetForContext(turnTraceContext);
      completeTurnPhase("target_read", phaseStartedAt);
      if (startedTarget?.status !== "active") {
        startedTarget = null;
      }
      userMessageId =
        options?.skipInputRecord === true
          ? (options.recordedInputMessageId ?? createMessageId())
          : createMessageId();
// Attachment display meta-information is delivered with TurnStarted (v4 projection → userInput row.attachments).
// The workspace checkpoint is hung on the user messageId; first generate the id and then send TurnStarted.
// Only v4 projection can use turn rowId to retrieve the file checkpoint of this round, so as to avoid empty checking when the summary has a count.
      const attachmentMetas = summarizeTurnAttachmentsForEvent(attachments);
      const turnStartedEvent = this.createEvent(
        SessionEventType.TurnStarted,
        {
          executionStartedAt,
          turnNumber: this.turnNumber,
          input: displayInput,
          messageId: userMessageId,
          inputId: options?.inputId,
          ...(options?.automationId
            ? { automationId: options.automationId }
            : options?.offPeakTaskId
              ? {
                  offPeakTaskId: options.offPeakTaskId,
                  ...(options.offPeakRunType ? { offPeakRunType: options.offPeakRunType } : {}),
                }
              : {}),
          foregroundExecutionId: this.activeForegroundExecution?.foregroundExecutionId,
          queryId,
          inputSource: options?.inputSource,
          inputVisibility: options?.inputVisibility,
          originMeta: options?.originMeta,
          ...(options?.epilogueStart === undefined ? {} : { epilogueStart: options.epilogueStart }),
          ...(options?.backgroundSource ? { backgroundSource: options.backgroundSource } : {}),
          targetId: options?.targetId,
          ...(options?.intent ? { intent: options.intent } : {}),
          ...(attachmentMetas ? { attachments: attachmentMetas } : {}),
        },
        turnTraceContext,
      );
      phaseStartedAt = startTurnPhase("turn_started_event");
      await this.appendEvent(turnStartedEvent, turnTraceContext);
      completeTurnPhase("turn_started_event", phaseStartedAt);
      events.push(turnStartedEvent);
      phaseStartedAt = startTurnPhase("target_accounting");
      startedTarget = await this.startTargetTurnAccounting({
        inputID: targetRunInputID,
        startedAtMs: turnStartedAtMs,
        startedTarget,
        traceContext: turnTraceContext,
      });
      completeTurnPhase("target_accounting", phaseStartedAt);
      if (startedTarget && this.sessionStore?.heartbeatTargetRun) {
        targetRunHeartbeat = setInterval(() => {
          void this.trackResidencyBlockingWork(
            this.heartbeatTargetTurnAccounting({
              inputID: targetRunInputID,
              seenAtMs: Date.now(),
              startedTarget,
              traceContext: turnTraceContext,
            }),
          );
        }, TARGET_RUN_HEARTBEAT_MS);
        if (typeof targetRunHeartbeat === "object" && "unref" in targetRunHeartbeat) {
          targetRunHeartbeat.unref();
        }
      }

      try {
        phaseStartedAt = startTurnPhase("user_prompt_hooks");
        const userPromptHookResult: HookRunResult = options?.skipUserPromptSubmitHooks
          ? { additionalContexts: [] }
          : await this.runUserPromptSubmitHooks(
              input,
              attachments,
              turnTraceContext,
              turnAbortSignal,
            );
        completeTurnPhase("user_prompt_hooks", phaseStartedAt);
        if (userPromptHookResult.preventContinuation) {
          const response =
            userPromptHookResult.stopReason ?? "Prompt blocked by UserPromptSubmit hook.";
          if (activeTurn) activeTurn.steerable = false;
          turnMachine = new TurnMachineImpl(turnMachine.complete(response, "success"));
          const turnUsage = createModelUsageSummaryFromEvents(events);
          const completeEvent = this.createEvent(
            SessionEventType.TurnComplete,
            {
              response,
              tokenCount: 0,
              usage: turnUsage,
              toolCallCount: 0,
              duration: Date.now() - turnMachine.state.startedAt.getTime(),
              resultType: "success",
              cacheStats: this.messageHistory.getCacheStats(),
              inputId: options?.inputId,
            },
            turnTraceContext,
          );
          await this.appendEvent(completeEvent, turnTraceContext);
          events.push(completeEvent);
          await recordTurnUsageFact(this, {
            completedAt: Date.now(),
            events,
            startedAt: turnStartedAtMs,
            status: "completed",
            traceContext: turnTraceContext,
            turnId,
          });
          this.turnNumber++;
          const projection = await this.rebuildProjection();
          await this.accountTargetTurnCompletion({
            inputID: targetRunInputID,
            startedAtMs: turnStartedAtMs,
            startedTarget,
            traceContext: turnTraceContext,
            usage: turnUsage,
          });
          return {
            response,
            turnId,
            traceId,
            usage: turnUsage,
            events,
            projection,
          };
        }
        this.injectHookAdditionalContextIntoMessageHistory(
          HookEventName.UserPromptSubmit,
          userPromptHookResult.additionalContexts,
        );
        injectReferencedSessionContextReminderIntoMessageHistory.call(this, input, options);
        injectDateChangeReminderIntoMessageHistory.call(this);
        const resolvedAttachments = await resolveTurnAttachments(attachments, {
          abortSignal: turnAbortSignal,
          artifactStore: this.artifactStore,
          fileSystemPort: this.fileSystemPort,
          imageProcessorPort: this.imageProcessorPort,
          sessionId: this.sessionId,
          traceContext: turnTraceContext,
          turnId,
          workingDirectory: this.workingDirectory,
        });
        logResolvedTurnAttachments(this.logger, turnTraceContext, resolvedAttachments);
        const sharedContextRefs = options?.sharedContextRefs ?? options?.intent?.sharedContextRefs;
        if (sharedContextRefs && sharedContextRefs.length > 0) {
          const [reference] = sharedContextRefs;
          if (!reference || reference.kind !== "shared_context_import") {
            throw new Error("invalid shared context reference");
          }
          if (!this.sessionStore) throw new Error("shared context import storage is unavailable");
          const alreadyHydrated = this.messageHistory
            .borrowReadOnlyRuntimeEntries()
            .some(
              (entry) => entry.kind !== "attachment" && entry.metadata?.source === "shared_context",
            );
          if (!alreadyHydrated) {
            const importedMessages = await this.sessionStore.messages({
              sessionID: this.sessionId,
            });
            const contextMessage = importedMessages.find(
              (message) =>
                message.info.role === "user" &&
                message.info.source === "shared_context" &&
                message.info.metadata &&
                typeof message.info.metadata === "object" &&
                (message.info.metadata as Record<string, unknown>).contextId ===
                  reference.context_id,
            );
            const contextText = contextMessage?.parts
              .filter(
                (part): part is Extract<MessagePart, { type: "text" }> => part.type === "text",
              )
              .map((part) => part.text)
              .join("\n")
              .trim();
            if (!contextText) throw new Error("shared context content is unavailable");
            this.messageHistory.addUser(
              contextText,
              runtimeMetadataForSyntheticUserMessageSource("shared_context"),
            );
          }
        }
        await this.persistPendingModelChangeTimeline(turnTraceContext);
        if (options?.skipInputRecord !== true && options?.inputVisibility === "model-only") {
          const inputSource = options.inputSource ?? "goal-continuation";
          const userContent = buildUserContentFromTurn(input, resolvedAttachments);
          this.messageHistory.addUser(
            userContent,
            runtimeInputMetadata(options.inputPresentation) ??
              runtimeMetadataForSyntheticUserMessageSource(inputSource),
          );
// /Goal automatic continuation is the internal user-role input injected into the model by the runtime.
// It is not a new message sent by the user in the chat. During persistence, raw input is retained for recovery/troubleshooting.
// But use model-only semantics to prevent UI-facing snapshot from rendering it as a user bubble.
          await this.persistSyntheticUserNoticeForSession({
            messageID: userMessageId,
            metadata: {
              ...(options.targetId ? { targetId: options.targetId } : {}),
              ...(options.inputPresentation
                ? { inputPresentation: options.inputPresentation }
                : {}),
              visibility: "model-only",
            },
            sessionId: this.sessionId,
            source: inputSource,
            text: input,
            traceContext: turnTraceContext,
            visibility: "model-only",
          });
        } else if (options?.skipInputRecord !== true) {
          this.messageHistory.addEntries(
            buildRuntimeUserEntriesFromTurn(input, resolvedAttachments, {
              browserAmbientContext: options?.browserAmbientContext,
            }).map((entry) => {
              const metadata = runtimeInputMetadata(options?.inputPresentation);
              return entry.kind !== "attachment" && metadata ? { ...entry, metadata } : entry;
            }),
          );
// /init and custom slash command will expand the model input into longer internal
//prompt. The visible history of the model must use the expanded input, but the UI display, session title and
// Restoring the snapshot can only display the original query actually submitted by the user.
          await this.persistUserPrompt(
            userMessageId,
            displayInput,
            resolvedAttachments,
            turnTraceContext,
            {
              intent: options?.intent,
              inputPresentation: options?.inputPresentation,
              sessionInputId: options?.intent?.queueItemId,
              sourceCommandId: options?.inputId,
              ...(options?.epilogueStart === undefined
                ? {}
                : { epilogueStart: options.epilogueStart }),
            },
          );
// Before title generation, wait for the main turn to succeed before starting, and the user stops/cancels the first round of requests.
// generated title never gets a chance to be initiated. The first query can be generated asynchronously after being persisted to avoid being delayed by the main link cancellation.
          const titleGenerationStarted = maybeStartSessionTitleGeneration.call(
            this,
            displayInput,
            userMessageId,
            turnTraceContext,
            {
              deferIfProviderRuntimeHeadersRefresh: true,
            },
          );
          shouldRetryTitleGenerationAfterTurn = !titleGenerationStarted;
        }
// Plugin reminder must be appended after the corresponding user message is written to the history and session store:
// The provider form is therefore stable as user → system, and cold hydration is restored in the same causal order.
// Root cause: input may have been expanded by a custom command, and parsing it will make plugin:// in the command template
// Obtain "user reference" semantics out of thin air; only the real persistent canonical displayInput is parsed here.
// The model-only continuation inside the runtime does not represent new user intentions and is not parsed repeatedly.
        if (options?.inputVisibility !== "model-only") {
          await this.injectPluginReferenceReminderFromTurn(
            displayInput,
            turnTraceContext,
            options?.toolDisallowlist,
          );
        }

        this.messageHistory.setCacheMiss();
        const loopModel = submissionModel ?? admittedModel;
        if (!loopModel) {
          throw new Error("Turn model was not created before execution");
        }
        loopState = {
          activeTurn,
          ...(options?.automationId ? { automationId: options.automationId } : {}),
// When idle, the identity of the dispatch wheel enters the loop state for tool execution boundary deny OffPeakCreate.
          ...(options?.offPeakTaskId ? { offPeakTaskId: options.offPeakTaskId } : {}),
          anomalyWarningsInjected: 0,
          backgroundSubagentResultConsumed: options?.backgroundSubagentResultConsumed === true,
          workflowResultConsumed: options?.workflowResultConsumed === true,
          currentUserMessageId: userMessageId,
          events,
          input,
          modelResponse: "",
          model: loopModel,
          ...(options?.modelExecution?.selectionScope === "execution"
            ? { modelSelectionScope: "execution" as const }
            : {}),
          ...(options?.modelExecution?.subagents && options.intent?.modelSelection
            ? {
                subagentModelOverride: {
                  selection: options.intent.modelSelection,
                  requestDependencies: options.modelExecution.requestDependencies,
                  background: options.modelExecution.subagents.background,
                },
              }
            : {}),
          modelStepCount: 0,
          historyRoundCount: 0,
          reactiveCompactAttemptedInCurrentModelStep: false,
          repeatedToolCallSignature: undefined,
          repeatedToolCallStreakCount: 0,
          stopHookContinuationCount: 0,
          streamRecoveryRetryCount: 0,
          tokenCount: 0,
          toolCallCount: 0,
          turnRequestState: {
// Turn only borrows the canonical member collection once, and then advances it by explicit commit; the entry itself
// Follow the immutable convention of MessageHistory.
            entries: [...this.messageHistory.borrowReadOnlyRuntimeEntries()],
            outputTokenContinuationCount: 0,
          },
          toolDisallowlist: options?.toolDisallowlist,
          traceId,
          turnAbortSignal,
          turnId,
          turnMachine,
          turnOutputStyle: admittedOutputStyle,
          turnTraceContext,
          userMessageId,
        };

        openGoalStateChangeReminderDeferral(activeTurn);
        phaseStartedAt = startTurnPhase("regular_turn_loop");
        try {
          await runRegularTurnLoop.call(this, loopState);
          completeTurnPhase("regular_turn_loop", phaseStartedAt);
        } finally {
          finishOutputTokenRecovery(loopState.turnRequestState);
          await closeGoalStateChangeReminderDeferral.call(this, activeTurn, turnTraceContext);
        }
        turnMachine = loopState.turnMachine;

        const turnUsage = createModelUsageSummaryFromEvents(events);
// goal usage/active-run is settled first, and then the exact goal/verifier boundary is fixed; only both
// Already persisted, TurnComplete can allow projection/UI to open the final assistant fork.
        await this.accountTargetTurnCompletion({
          inputID: targetRunInputID,
          startedAtMs: turnStartedAtMs,
          startedTarget,
          traceContext: turnTraceContext,
          usage: turnUsage,
        });
        if (loopState.stableProductStartMessageId && loopState.stableBoundaryAssistantMessageId) {
          await persistStableForkCompletionBoundary(this, {
            boundaryMessageId: loopState.stableBoundaryAssistantMessageId,
            startMessageId: loopState.stableProductStartMessageId,
            historyRoundCount: loopState.historyRoundCount,
            traceContext: turnTraceContext,
          });
        }
        if (loopState.stableBoundaryAssistantMessageId) {
          await appendBrowserTurnScreenshot(
            this,
            loopState,
            loopState.stableBoundaryAssistantMessageId,
          );
        }
        const completeEvent = this.createEvent(
          SessionEventType.TurnComplete,
          {
            response: loopState.modelResponse,
            tokenCount: loopState.tokenCount,
            usage: turnUsage,
            toolCallCount: loopState.toolCallCount,
            historyRoundCount: loopState.historyRoundCount,
            duration: Date.now() - turnMachine.state.startedAt.getTime(),
            resultType: "success",
            ...(loopState.backgroundSubagentResultConsumed
              ? { backgroundSubagentResultConsumed: true }
              : {}),
            ...(loopState.workflowResultConsumed ? { workflowResultConsumed: true } : {}),
            cacheStats: this.messageHistory.getCacheStats(),
            inputId: options?.inputId,
          },
          turnTraceContext,
        );
        await this.appendEvent(completeEvent, turnTraceContext);
        events.push(completeEvent);
        await recordTurnUsageFact(this, {
          completedAt: Date.now(),
          events,
          startedAt: turnStartedAtMs,
          status: "completed",
          traceContext: turnTraceContext,
          turnId,
          userMessageId,
        });
        if (shouldRetryTitleGenerationAfterTurn && userMessageId) {
// Model that needs to refresh provider runtime headers before requesting
// If the title is generated before the main turn, the authentication refresh window will be occupied first, causing the real user message to fail.
          maybeStartDeferredSessionTitleGeneration.call(
            this,
            displayInput,
            userMessageId,
            turnTraceContext,
          );
        }
        this.turnNumber++;

        const projection = await this.rebuildProjection();
        this.logger?.info("Turn completed", {
          ...traceContextToLogContext(turnTraceContext),
          durationMs: Date.now() - turnMachine.state.startedAt.getTime(),
          event: "turn.completed",
          module: "core.runtime",
          status: "completed",
          toolCallCount: loopState.toolCallCount,
        });
// The single-round execution strategy only suppresses the background extraction of this successful Turn and does not modify the Session Memory configuration.
        if (options?.modelExecution?.memoryExtraction !== "skip") {
          scheduleProjectMemoryExtraction(this, {
            model: loopState.model,
            traceContext: turnTraceContext,
          });
        }

        const result: TurnResult = {
          response: loopState.modelResponse,
          turnId,
          traceId,
          usage: turnUsage,
          events,
          projection,
        };
        return result;
      } catch (error) {
        turnFailureHandled = true;
        const coreError = createTurnFailureError(error, turnAbortSignal, "Turn execution failed");
        const preserveQueueAutoDrainOnCancel =
          coreError.type === CoreErrorType.TurnCancelled &&
          this.activeForegroundExecution?.preserveQueueAutoDrainOnCancel === true;
        const finishedTarget = await this.finishTargetTurnAccounting({
          endedAtMs: Date.now(),
          inputID: targetRunInputID,
          startedTarget,
          status: coreError.type === CoreErrorType.TurnCancelled ? "paused" : undefined,
          traceContext: turnTraceContext,
        });
        if (finishedTarget?.targetID === startedTarget?.targetID) {
          startedTarget = finishedTarget;
        }
        if (coreError.type === CoreErrorType.TurnCancelled) {
          await this.pauseActiveTargetForCancellation(turnTraceContext);
          if (activeTurn) {
            await this.fallbackPendingGuidesToQueue({
              activeTurn,
              events,
              reasonCode: "guide.turnInterrupted",
              traceContext: turnTraceContext,
            });
          }
        }
// Ordinary TurnError only ends the current turn and does not cancel the accepted future input.
// V4 TurnError projection cuts the queue into error-paused, and the runtime synchronously closes the inline drain.
// Retain queued input, waiting for the user to explicitly continue.
        if (activeTurn && coreError.type !== CoreErrorType.TurnCancelled) {
          const pendingInputs = (await this.rebuildProjection()).pendingSteerInputs;
          if (pendingInputs.length > 0) {
            this.queueAutoDrain = false;
            this.queueExternalDrainActive = false;
          }
        } else if (
          activeTurn &&
          coreError.type === CoreErrorType.TurnCancelled &&
          !preserveQueueAutoDrainOnCancel &&
          activeTurn.pendingInputs.length > 0
        ) {
// The runtime authorization bit is synchronized with the projection: the projection is set when TurnComplete(cancelled)+queue>0
// queue.autoDrain is set to false (held), and the drain gate of the runtime must also flip synchronously.
// Otherwise, a new turn during the held period will drain out the subsequent queued items, diverging from the projection semantics.
          this.queueAutoDrain = false;
          this.queueExternalDrainActive = false;
        }

// background wake may be canceled before loopState is initialized; at this time, the dequeue result fact must still be retained.
        const backgroundSubagentResultConsumed =
          options?.backgroundSubagentResultConsumed === true ||
          loopState?.backgroundSubagentResultConsumed === true;
        const workflowResultConsumed =
          options?.workflowResultConsumed === true || loopState?.workflowResultConsumed === true;
        await appendTurnOutcomeEvent(this, {
          coreError,
          events,
          durationMs: Date.now() - turnMachine.state.startedAt.getTime(),
          turnPhase: turnMachine.state.phase,
          inputId: options?.inputId,
          traceContext: turnTraceContext,
          fallbackMessage: "Turn execution failed",
          logEvent: "turn.failed",
          logLabel: "Turn",
          preserveQueueAutoDrainOnCancel,
          backgroundSubagentResultConsumed,
          workflowResultConsumed,
          historyRoundCount: loopState?.historyRoundCount,
        });
        await recordTurnUsageFact(this, {
          completedAt: Date.now(),
          error: coreError,
          events,
          startedAt: turnStartedAtMs,
          status: coreError.type === CoreErrorType.TurnCancelled ? "cancelled" : "error",
          traceContext: turnTraceContext,
          turnId,
          userMessageId,
        });
        throw coreError;
      }
    }).then(
      (result) => {
        turnTelemetry.finishCompleted("assistant_message");
        return result;
      },
      (error: unknown) => {
        if (!turnFailureHandled) {
          this.logger?.warn("Turn execution escaped lifecycle handler", {
            ...traceContextToLogContext(turnTraceContext),
            durationMs: Date.now() - turnStartedAtMs,
            errorMessage: error instanceof Error ? error.message : String(error),
            event: "turn.lifecycle.unhandled_rejection",
            module: "core.runtime",
            phase: turnPhase,
            status: "failed",
          });
        }
        if (isTurnCancellationError(error, turnAbortSignal)) {
          turnTelemetry.finishCancelled("abort_signal");
        } else {
          turnTelemetry.finishFailed("unhandled", "unknown", error);
        }
        throw error;
      },
    );

  return turnTelemetry.run(execute).finally(async () => {
    if (targetRunHeartbeat) {
      clearInterval(targetRunHeartbeat);
    }
    this.releaseTurnStart(turnId);
    clearBrowserTurnState(this.sessionId, turnId);
    this.finishActiveTurn(activeTurn);
    turnAbortScope.dispose();
    try {
      await this.browserControlPort?.turnEnded?.({
        sessionId: this.sessionId,
        turnId: String(turnId),
        traceContext: turnTraceContext,
      });
    } catch (error) {
// Life cycle cleanup failure cannot overwrite the completed/failed main turn; the backend will be released completely after session close.
      this.logger?.warn("Browser turn cleanup failed", {
        error: error instanceof Error ? error.message : String(error),
        event: "browser.turn_cleanup.failed",
        turnId: String(turnId),
      });
    }
  });
}

function injectDateChangeReminderIntoMessageHistory(this: AgentRuntimeInternal): void {
  const currentDate = formatLocalIsoDate(this.now());
  const previousDate = this.lastEmittedLocalDate;
  this.lastEmittedLocalDate = currentDate;

  if (!previousDate || previousDate === currentDate) {
    return;
  }

  this.messageHistory.addAttachment(
    "date_change",
    buildDateChangeReminderBody(previousDate, currentDate),
  );
}

function injectReferencedSessionContextReminderIntoMessageHistory(
  this: AgentRuntimeInternal,
  input: string,
  options?: ExecuteTurnOptions,
): void {
  if (options?.inputVisibility === "model-only") return;
  const reminderBody = buildReferencedSessionContextReminderBody(input);
  if (!reminderBody) return;
  this.messageHistory.addAttachment("referenced_session_context", reminderBody);
}
