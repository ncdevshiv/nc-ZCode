import type { AgentRuntime, TurnAttachment, TurnResult } from "@zcode/core";
import {
  SessionEventType,
  traceContextToLogContext,
  type InputHistoryEntry,
  type InputHistoryKind,
  type InputHistoryStorePort,
  type Logger,
  type SessionId,
  type ToolArtifactStorePort,
  type TraceContext,
} from "@zcode/contracts";
import {
  externalizePromptAttachments,
  materializeInputHistoryEntry,
  normalizePromptInput,
  projectInputHistoryAttachments,
} from "./prompt-input.js";
import type { PrepareUserExecutionBoundary, SubmitPromptOptions, ZCodeApp } from "./types.js";

type InputFacade = Pick<
  ZCodeApp,
  | "continueActiveTarget"
  | "enqueueDeferredInput"
  | "recordInputHistory"
  | "editQueueItem"
  | "recallPreviousInputHistory"
  | "removeQueueItem"
  | "reserveQueueItem"
  | "markQueueItemPromoting"
  | "releaseQueueItemReservation"
  | "clearQueueItems"
  | "completeExternalQueueDrain"
  | "reorderQueueItem"
  | "setQueueAutoDrain"
  | "setFollowupMode"
  | "sendInput"
  | "steerTurn"
  | "submitPrompt"
>;

interface CreateInputFacadeDeps {
  artifactStore?: ToolArtifactStorePort;
  customCommandPromptResolver?: (
    text: string,
    options?: Pick<SubmitPromptOptions, "abortSignal" | "traceContext">,
  ) => Promise<string | undefined>;
  inputHistoryStore?: InputHistoryStorePort;
  logger: Logger;
  prepareUserExecutionBoundary: PrepareUserExecutionBoundary;
  runtime: AgentRuntime;
  sessionId: SessionId;
  traceContext: TraceContext;
}

export function createInputFacade(deps: CreateInputFacadeDeps): InputFacade {
  const preparePromptBoundary = async (options?: SubmitPromptOptions): Promise<void> => {
    await deps.prepareUserExecutionBoundary(options);
  };

  const recordAcceptedInputHistory = async (
    text: string,
    kind: InputHistoryKind,
    recordTraceContext: TraceContext,
    attachments?: TurnAttachment[],
  ): Promise<InputHistoryEntry | null> => {
    if (!deps.inputHistoryStore) return null;
    try {
      return await deps.inputHistoryStore.recordInputHistory({
        attachments: projectInputHistoryAttachments(attachments),
        kind,
        projectID: deps.runtime.getProjectId(),
        sessionID: deps.sessionId,
        text,
      });
    } catch (error) {
      deps.logger.warn("Input history write failed", {
        ...traceContextToLogContext(recordTraceContext),
        error: error instanceof Error ? error.message : String(error),
        event: "input_history.record.failed",
        kind,
        module: "bootstrap",
        projectId: deps.runtime.getProjectId(),
        status: "failed",
      });
      return null;
    }
  };

  const recallPreviousInputHistory = async (skip?: number): Promise<InputHistoryEntry | null> => {
    if (!deps.inputHistoryStore) return null;
    const entry = await deps.inputHistoryStore.recallPreviousInputHistory({
      projectID: deps.runtime.getProjectId(),
      skip,
    });
    return await materializeInputHistoryEntry(entry, deps.artifactStore);
  };

  const runPromptTurn = async (
    promptInput: ReturnType<typeof normalizePromptInput>,
    options?: SubmitPromptOptions,
  ): Promise<TurnResult> => {
    const storedAttachments = await externalizePromptAttachments(promptInput.attachments, {
      artifactStore: deps.artifactStore,
      sessionId: deps.sessionId,
      traceContext: options?.traceContext ?? deps.traceContext,
    });
    await recordAcceptedInputHistory(
      promptInput.text,
      "prompt",
      options?.traceContext ?? deps.traceContext,
      storedAttachments,
    );
    const resolvedCommandPrompt = await deps.customCommandPromptResolver?.(promptInput.text, {
      abortSignal: options?.abortSignal,
      traceContext: options?.traceContext ?? deps.traceContext,
    });
    const runtimePromptText = resolvedCommandPrompt ?? promptInput.text;
    const turnAttribution = options?.automationId
      ? { automationId: options.automationId }
      : options?.offPeakTaskId
        ? {
            offPeakTaskId: options.offPeakTaskId,
            ...(options.offPeakRunType ? { offPeakRunType: options.offPeakRunType } : {}),
          }
        : {};
    return await deps.runtime.executeTurn(runtimePromptText, storedAttachments, {
      abortSignal: options?.abortSignal,
      browserAmbientContext: options?.browserAmbientContext,
      continueActiveTargetAfterTurn: true,
      ...(resolvedCommandPrompt !== undefined ? { displayInput: promptInput.text } : {}),
      inputId: options?.inputId,
      ...turnAttribution,
      intent: options?.intent,
      sharedContextRefs: options?.sharedContextRefs,
      queryId: options?.queryId,
      toolDisallowlist: options?.toolDisallowlist,
      traceContext: options?.traceContext ?? deps.traceContext,
      modelExecution: options?.modelExecution,
    });
  };

  const prepareRuntimePrompt = async (
    promptInput: ReturnType<typeof normalizePromptInput>,
    options?: SubmitPromptOptions,
  ): Promise<{ input: string; storedAttachments?: TurnAttachment[] }> => {
    const storedAttachments = await externalizePromptAttachments(promptInput.attachments, {
      artifactStore: deps.artifactStore,
      sessionId: deps.sessionId,
      traceContext: options?.traceContext ?? deps.traceContext,
    });
    const resolvedCommandPrompt = await deps.customCommandPromptResolver?.(promptInput.text, {
      abortSignal: options?.abortSignal,
      traceContext: options?.traceContext ?? deps.traceContext,
    });
    return {
      input: resolvedCommandPrompt ?? promptInput.text,
      ...(storedAttachments ? { storedAttachments } : {}),
    };
  };

  return {
    continueActiveTarget: async (options) => {
      const unsubscribe = options?.onEvent
        ? deps.runtime.subscribeEvents({ onSessionEvent: options.onEvent })
        : undefined;
      try {
// cold resume will trigger SessionStart Hook review within the prepare boundary. Must subscribe first,
// Otherwise ReviewRequested occurs before the subscription window, and Dual ACK will never get the release authority.
        await preparePromptBoundary(options);
        return await deps.runtime.continueActiveTargetLoop({
          abortSignal: options?.abortSignal,
          inputId: options?.inputId,
          intent: options?.intent,
          traceContext: options?.traceContext ?? deps.traceContext,
          trigger: "manual",
          verifyBeforeFirstContinue: false,
        });
      } finally {
        unsubscribe?.();
      }
    },
    recordInputHistory: async (input, kind = "slash_command") => {
      const promptInput = normalizePromptInput(input);
      const attachments = await externalizePromptAttachments(promptInput.attachments, {
        artifactStore: deps.artifactStore,
        sessionId: deps.sessionId,
        traceContext: deps.traceContext,
      });
      return recordAcceptedInputHistory(promptInput.text, kind, deps.traceContext, attachments);
    },
    recallPreviousInputHistory,
    sendInput: async (input, options) => {
      const promptInput = normalizePromptInput(input);
      const delivery = options?.delivery ?? "auto";
      const unsubscribe =
        options?.onEvent || options?.onTurnStartedObserved
          ? deps.runtime.subscribeEvents({
              onSessionEvent: async (event) => {
                await options.onEvent?.(event);
                if (
                  event.type === SessionEventType.TurnStarted &&
                  (event.payload as { inputId?: unknown }).inputId === options.inputId
                ) {
// TurnStarted is only used for subsequent associations; admission ACK does not wait for projection commit.
                  options.onTurnStartedObserved?.(event);
                }
              },
            })
          : undefined;
      try {
// resume review belongs to this prompt life cycle, and the subscription covers prepare boundary; Core admission
//Complete start/queue in the same session runtime, do not read activeTurn here for forking.
        await preparePromptBoundary(options);
        const prepared = await prepareRuntimePrompt(promptInput, options);
        const result = await deps.runtime.admitPrompt(prepared.input, prepared.storedAttachments, {
          ...options,
          delivery,
          traceContext: options?.traceContext ?? deps.traceContext,
          ...(prepared.input !== promptInput.text ? { displayInput: promptInput.text } : {}),
        });
        if (result.kind === "started") {
          await recordAcceptedInputHistory(
            promptInput.text,
            "prompt",
            options?.traceContext ?? deps.traceContext,
            prepared.storedAttachments,
          );
          if (unsubscribe) void result.completion.then(unsubscribe, unsubscribe);
          return {
            completion: result.completion,
            kind: "started_turn",
            turnId: result.turnId,
          };
        }
        unsubscribe?.();
        if (result.kind === "queued") {
          await recordAcceptedInputHistory(
            promptInput.text,
            "steered_input",
            options?.traceContext ?? deps.traceContext,
            prepared.storedAttachments,
          );
        }
        return result;
      } catch (error) {
        unsubscribe?.();
        throw error;
      }
    },
    enqueueDeferredInput: async (input, options) => {
      const result = await deps.runtime.enqueueDeferredInput({
        input,
        inputPresentation: "user_steer",
        ...(options?.commandKind ? { commandKind: options.commandKind } : {}),
        ...(options?.delivery ? { delivery: options.delivery } : {}),
        ...(options?.intent ? { intent: options.intent } : {}),
        ...(options?.attachments ? { attachments: options.attachments } : {}),
        ...(options?.toolDisallowlist ? { toolDisallowlist: options.toolDisallowlist } : {}),
        ...(options?.intent?.queueItemId ? { pendingInputId: options.intent.queueItemId } : {}),
        ...(options?.inputId ? { inputId: options.inputId } : {}),
        ...(options?.queryId ? { queryId: options.queryId } : {}),
        traceContext: options?.traceContext ?? deps.traceContext,
      });
      if (result.kind === "queued") {
        await recordAcceptedInputHistory(
          input,
          "steered_input",
          options?.traceContext ?? deps.traceContext,
        );
      }
      return result;
    },
    steerTurn: async (input, options) => {
      const unsubscribe = options?.onEvent
        ? deps.runtime.subscribeEvents({ onSessionEvent: options.onEvent })
        : undefined;
      try {
        const result = await deps.runtime.steerTurn({
          inputPresentation: "user_steer",
          commandKind: options?.commandKind,
          inputId: options?.inputId,
          queryId: options?.queryId,
          expectedTurnId: options?.expectedTurnId,
          delivery: options?.delivery,
          intent: options?.intent,
          attachments: options?.attachments,
          pendingInputId: options?.intent?.queueItemId,
          input,
          toolDisallowlist: options?.toolDisallowlist,
          traceContext: options?.traceContext ?? deps.traceContext,
        });
        if (result.kind === "queued") {
          await recordAcceptedInputHistory(
            input,
            "steered_input",
            options?.traceContext ?? deps.traceContext,
          );
        }
        return result;
      } finally {
        unsubscribe?.();
      }
    },
    removeQueueItem: async (pendingInputId, options) => {
// v4 queue single item deletion: bridge v4 command to runtime single item pending-input removal.
      return deps.runtime.removePendingInputById({
        pendingInputId,
        reason: options?.reason ?? "user_removed",
        reservationId: options?.reservationId,
        traceContext: options?.traceContext ?? deps.traceContext,
      });
    },
    reserveQueueItem: async (pendingInputId, reservationId, options) =>
      deps.runtime.reservePendingInputById({
        pendingInputId,
        reservationId,
        traceContext: options?.traceContext ?? deps.traceContext,
      }),
    markQueueItemPromoting: async (pendingInputId, reservationId, options) =>
      deps.runtime.markPendingInputPromoting({
        pendingInputId,
        reservationId,
        traceContext: options?.traceContext ?? deps.traceContext,
      }),
    releaseQueueItemReservation: async (pendingInputId, reservationId, options) =>
      deps.runtime.releasePendingInputReservation({
        pendingInputId,
        reservationId,
        traceContext: options?.traceContext ?? deps.traceContext,
      }),
    editQueueItem: async (pendingInputId, newText, options) => {
// v4 queue single item editing: replace the queued input text (reducer with the same id is updated in place, and the position is maintained).
      return deps.runtime.editPendingInputById({
        pendingInputId,
        newText,
        traceContext: options?.traceContext ?? deps.traceContext,
      });
    },
    reorderQueueItem: async (pendingInputId, beforePendingInputId, options) => {
// v4 queue rearrangement: move the queued item to the front of the anchor point (null = end of the queue).
      return deps.runtime.reorderPendingInput({
        pendingInputId,
        beforePendingInputId,
        traceContext: options?.traceContext ?? deps.traceContext,
      });
    },
    clearQueueItems: async (options) => {
// v4 clearQueueAndSend: Clear the active turn memory item + held projection residue.
      return deps.runtime.clearAllPendingInputs(options?.traceContext ?? deps.traceContext);
    },
    setQueueAutoDrain: async (autoDrain, options) => {
// v4 setAutoDrain: Toggle queue autoDrain authorization bit (session-level configuration event).
      await deps.runtime.setQueueAutoDrain({
        autoDrain,
        traceContext: options?.traceContext ?? deps.traceContext,
      });
    },
    completeExternalQueueDrain: () => {
      deps.runtime.completeExternalQueueDrain();
    },
    setFollowupMode: async (mode, options) => {
// v4 setFollowupMode: Flip followup routing mode (queue/guide).
      await deps.runtime.setFollowupMode({
        mode,
        traceContext: options?.traceContext ?? deps.traceContext,
      });
    },
    submitPrompt: async (prompt, options) => {
      const promptInput = normalizePromptInput(prompt);
      const unsubscribe = options?.onEvent
        ? deps.runtime.subscribeEvents({ onSessionEvent: options.onEvent })
        : undefined;
      try {
        await preparePromptBoundary(options);
        return await runPromptTurn(promptInput, options);
      } finally {
        unsubscribe?.();
      }
    },
  };
}
