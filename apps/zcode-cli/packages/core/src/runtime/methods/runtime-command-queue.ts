/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import { traceContextToLogContext } from "../deps.js";
import type { RuntimeCommand, TaskNotificationRuntimeCommand } from "../command-queue.js";
import { uuidv7 } from "@zcode/shared";
import type { AgentRuntimeInternal } from "../internal.js";
import {
  persistBackgroundTaskNotificationBatch,
  shouldSuppressTaskNotificationRuntimeCommand,
} from "./background-notifications.js";
import { persistSubagentMessageCommand } from "./subagent-messages.js";
import { runControlOnlyTurnCommand } from "./control-only-turn.js";
import { createTurnCancelledError } from "../helpers/index.js";
import { executeTargetContinuationCommand } from "./target.js";
import { runActiveTargetContinuationLoop } from "./target-continuation-loop.js";
import { isStaleBranchRuntimeCommand } from "./runtime-command-generation.js";
import type {
  AcquireForegroundPromotionLeaseResult,
  ActiveForegroundExecutionState,
  ForegroundPromotionLeaseMode,
  StopActiveForegroundExecutionOptions,
  StopActiveForegroundExecutionResult,
} from "../types.js";

export function enqueueRuntimeCommand(this: AgentRuntimeInternal, command: RuntimeCommand): void {
  this.runtimeCommandQueue.enqueue(command);
  if (command.mode === "task-notification") {
    this.logger?.info?.("Background task notification enqueued into runtime command queue", {
      ...traceContextToLogContext(command.traceContext),
      commandId: command.id,
      event: "background_task.notification.runtime_enqueued",
      module: "core.runtime",
      queueSize: this.runtimeCommandQueue.size(),
    });
  }
  void this.drainRuntimeCommandQueue();
}

export async function drainRuntimeCommandQueue(this: AgentRuntimeInternal): Promise<void> {
  if (this.runtimeCommandDrainActive) return;

  this.runtimeCommandDrainActive = true;
  try {
    let commands: readonly RuntimeCommand[];
// Merge the same batch of background notifications into one model wheel to avoid initiating separate requests for each notification.
    while ((commands = dequeueNextRunnableBatch.call(this)).length > 0) {
      const firstCommand = commands[0];
      if (!firstCommand) continue;
      if (firstCommand.mode === "task-notification") {
        const notificationCommands = commands.filter(
          (command): command is TaskNotificationRuntimeCommand =>
            command.mode === "task-notification",
        );
        if (notificationCommands.length !== commands.length) {
          throw new Error("Runtime command queue returned a mixed task-notification batch");
        }
        await runTaskNotificationBatch.call(this, notificationCommands);
        continue;
      }
      if (commands.length !== 1) {
        throw new Error(`Runtime command queue returned an unsupported ${firstCommand.mode} batch`);
      }
      await runRuntimeCommand.call(this, firstCommand);
    }
  } finally {
    this.runtimeCommandDrainActive = false;
  }

  if (this.runtimeCommandQueue.hasPending() && this.foregroundPromotionLease === undefined) {
    await this.drainRuntimeCommandQueue();
  }
}

function dequeueNextRunnableBatch(this: AgentRuntimeInternal): readonly RuntimeCommand[] {
  const lease = this.foregroundPromotionLease;
  if (!lease) return this.runtimeCommandQueue.dequeueNextBatch();

  const promotedCommand = this.runtimeCommandQueue
    .snapshot()
    .find((command) => runtimeCommandInputId(command) === lease.promotedInputId);
  if (!promotedCommand) return Object.freeze([]);
  const removedPromotedCommand = this.runtimeCommandQueue.removeById(promotedCommand.id);
  if (!removedPromotedCommand) return Object.freeze([]);

// sendQueuedNow In the past, there was no Core between Stop A and promoted command enqueuing
// Scheduling ownership, notification B will be dequeued first. Matching command dequeue and lease consumption must be synchronized at the same time.
  this.foregroundPromotionLease = undefined;
  return Object.freeze([removedPromotedCommand]);
}

function runtimeCommandInputId(command: RuntimeCommand): string | undefined {
  if (
    command.mode === "prompt" ||
    command.mode === "target-continuation" ||
    command.mode === "target-continuation-loop"
  ) {
    return command.options?.inputId;
  }
  return undefined;
}

export function hasActiveOrQueuedTurnWork(this: AgentRuntimeInternal): boolean {
  return (
    this.foregroundPromotionLease !== undefined ||
    this.activeForegroundExecution !== undefined ||
    this.runtimeCommandDrainActive ||
    this.runtimeCommandQueue.hasPending() ||
    this.activeTurn !== undefined ||
    this.activeTurnStartReservation !== undefined
  );
}

export function acquireForegroundPromotionLease(
  this: AgentRuntimeInternal,
  options: {
    leaseId: string;
    mode: ForegroundPromotionLeaseMode;
    promotedInputId: string;
  },
): AcquireForegroundPromotionLeaseResult {
  const existing = this.foregroundPromotionLease;
  if (existing) {
    return existing.leaseId === options.leaseId
      ? { kind: "acquired", leaseId: existing.leaseId }
      : { kind: "conflict", leaseId: existing.leaseId };
  }
  if (
    options.mode === "idle-only" &&
    (this.activeForegroundExecution !== undefined ||
      this.runtimeCommandDrainActive ||
      this.runtimeCommandQueue.hasPending() ||
      this.activeTurn !== undefined ||
      this.activeTurnStartReservation !== undefined)
  ) {
    return { kind: "busy" };
  }
  this.foregroundPromotionLease = {
    leaseId: options.leaseId,
    promotedInputId: options.promotedInputId,
  };
  return { kind: "acquired", leaseId: options.leaseId };
}

export function releaseForegroundPromotionLease(
  this: AgentRuntimeInternal,
  leaseId: string,
): boolean {
  if (this.foregroundPromotionLease?.leaseId !== leaseId) return false;
  this.foregroundPromotionLease = undefined;
// When it fails before starting, B may have been waiting after the lease; the release must actively resume drain and cannot wait for the next enqueue.
  void this.drainRuntimeCommandQueue();
  return true;
}

async function runPostCommandActiveTargetLoop(
  this: AgentRuntimeInternal,
  command: RuntimeCommand,
  abortSignal: AbortSignal,
): Promise<Awaited<ReturnType<AgentRuntimeInternal["continueActiveTargetLoop"]>> | null> {
  if (command.mode === "prompt" && command.options?.continueActiveTargetAfterTurn === true) {
    return await runActiveTargetContinuationLoop.call(this, {
      abortSignal,
      inputId: command.options.inputId,
      traceContext: command.options.traceContext ?? command.traceContext,
      trigger: "user-prompt",
      verifyBeforeFirstContinue: true,
    });
  }
  if (command.mode === "task-notification") {
    try {
      return await runActiveTargetContinuationLoop.call(this, {
        abortSignal,
        traceContext: command.traceContext,
        trigger: "task-notification",
        verifyBeforeFirstContinue: true,
      });
    } catch (error) {
      this.logger?.warn("Post-command goal continuation failed", {
        ...traceContextToLogContext(command.traceContext),
        errorMessage: error instanceof Error ? error.message : String(error),
        event: "target.continuation.after_command_failed",
        module: "core.runtime",
      });
    }
  }
  return null;
}

async function runRuntimeCommand(
  this: AgentRuntimeInternal,
  command: RuntimeCommand,
): Promise<void> {
  if (command.mode === "task-notification") {
    await runTaskNotificationBatch.call(this, [command]);
    return;
  }
  if (isStaleBranchRuntimeCommand(this, command)) return;
  const foregroundExecution = beginForegroundExecution.call(this, command);
  try {
    if (command.mode === "prompt") {
      if (this.runtimeCommandQueue.consumeCancelPending(command.id)) {
        if (command.startReservation) this.releaseTurnStart(command.startReservation.turnId);
        command.reject(createTurnCancelledError(command.options?.abortSignal?.reason));
        return;
      }
      try {
        const result = await this.executeTurnCommand(
          command.input,
          command.attachments,
          {
            ...command.options,
            abortSignal: foregroundExecution.controller.signal,
          },
          command.startReservation,
        );
        const continuationResult = await runPostCommandActiveTargetLoop.call(
          this,
          command,
          foregroundExecution.controller.signal,
        );
        command.resolve(continuationResult ?? result);
      } finally {
        this.runtimeCommandQueue.clearCancelPending(command.id);
      }
      return;
    }
    if (command.mode === "target-continuation") {
      if (this.runtimeCommandQueue.consumeCancelPending(command.id)) {
        command.reject(createTurnCancelledError(command.options.abortSignal?.reason));
        return;
      }
      try {
        const result = await executeTargetContinuationCommand.call(this, {
          ...command.options,
          abortSignal: foregroundExecution.controller.signal,
        });
        command.resolve(result);
      } finally {
        this.runtimeCommandQueue.clearCancelPending(command.id);
      }
      return;
    }
    if (command.mode === "target-continuation-loop") {
      if (this.runtimeCommandQueue.consumeCancelPending(command.id)) {
        command.reject(createTurnCancelledError(command.options.abortSignal?.reason));
        return;
      }
      try {
        const result = await runActiveTargetContinuationLoop.call(this, {
          ...command.options,
          abortSignal: foregroundExecution.controller.signal,
          yieldBeforeFirstContinue: false,
        });
        command.resolve(result);
      } finally {
        this.runtimeCommandQueue.clearCancelPending(command.id);
      }
      return;
    }
    if (command.mode === "subagent-message") {
      this.logger?.debug("Subagent response command started", {
        ...traceContextToLogContext(command.traceContext),
        agentId: command.agentId,
        commandId: command.id,
        event: "subagent.response.command_started",
        messageLength: command.messageLength,
        module: "core.runtime",
        queueSize: this.runtimeCommandQueue.size(),
        responseId: command.responseId,
        summary: command.summary.slice(0, 200),
      });
      const messageId = await persistSubagentMessageCommand.call(this, command);
      await this.executeTurnCommand(command.text, undefined, {
        abortSignal: foregroundExecution.controller.signal,
        inputSource: "subagent_message",
        inputVisibility: "model-only",
        recordedInputMessageId: messageId,
        skipInputRecord: true,
        skipUserPromptSubmitHooks: true,
        traceContext: command.traceContext,
      });
      this.logger?.debug("Subagent response command completed", {
        ...traceContextToLogContext(command.traceContext),
        agentId: command.agentId,
        commandId: command.id,
        event: "subagent.response.command_completed",
        messageId,
        module: "core.runtime",
        queueSize: this.runtimeCommandQueue.size(),
        responseId: command.responseId,
      });
      return;
    }
    if (command.mode === "control-only-turn") {
      await runControlOnlyTurnCommand.call(this, command);
      return;
    }
  } catch (error) {
    if (
      command.mode === "prompt" ||
      command.mode === "target-continuation" ||
      command.mode === "target-continuation-loop"
    ) {
      command.reject(error);
      return;
    }
    this.logger?.warn("Runtime command failed", {
      ...traceContextToLogContext(command.traceContext),
      commandMode: command.mode,
      errorMessage: error instanceof Error ? error.message : String(error),
      event: "runtime_command.failed",
      module: "core.runtime",
    });
  } finally {
    finishForegroundExecution.call(this, foregroundExecution);
  }
}

async function runTaskNotificationBatch(
  this: AgentRuntimeInternal,
  commands: readonly TaskNotificationRuntimeCommand[],
): Promise<void> {
  const eligibleCommands = commands.filter(
    (command) =>
      !isStaleBranchRuntimeCommand(this, command) &&
      !shouldSuppressTaskNotificationRuntimeCommand.call(this, command),
  );
  const firstCommand = eligibleCommands[0];
  if (!firstCommand) return;

  const foregroundExecution = beginForegroundExecution.call(this, firstCommand);
  const commandIds = eligibleCommands.map((command) => command.id);
  try {
    const persisted = await persistBackgroundTaskNotificationBatch.call(
      this,
      eligibleCommands as [TaskNotificationRuntimeCommand, ...TaskNotificationRuntimeCommand[]],
    );
    this.logger?.info?.("Background task notification batch started", {
      ...traceContextToLogContext(firstCommand.traceContext),
      batchSize: eligibleCommands.length,
      commandId: firstCommand.id,
      commandIds,
      event: "background_task.notification.batch_started",
      messageId: persisted.messageId,
      module: "core.runtime",
    });
    await this.executeTurnCommand(persisted.text, undefined, {
// wake lacks inputId, telemetry borrows persistence msg_*, and forks from the ordinary main turn.
// Each independent batch uses the same UUID v7 rule; persistent messages still use recordedInputMessageId.
      inputId: uuidv7(),
      abortSignal: foregroundExecution.controller.signal,
// Batch display metadata only retains representative tasks. The composition must check the entire batch and cannot be obscured by the first Bash task.
      backgroundSubagentResultConsumed: eligibleCommands.some(
        (command) => command.originMeta?.backgroundSource === "subagent",
      ),
//Workflow dimension of the same rule: run completion/question notification is in the batch.
      workflowResultConsumed: eligibleCommands.some(
        (command) => command.originMeta?.backgroundSource === "workflow",
      ),
      ...(persisted.backgroundSource ? { backgroundSource: persisted.backgroundSource } : {}),
      inputSource: "background_task",
      inputVisibility: "model-only",
      ...(persisted.originMeta ? { originMeta: persisted.originMeta } : {}),
      recordedInputMessageId: persisted.messageId,
      skipInputRecord: true,
      skipUserPromptSubmitHooks: true,
      traceContext: firstCommand.traceContext,
    });
    await runPostCommandActiveTargetLoop.call(
      this,
      firstCommand,
      foregroundExecution.controller.signal,
    );
    this.logger?.info?.("Background task notification batch completed", {
      ...traceContextToLogContext(firstCommand.traceContext),
      batchSize: eligibleCommands.length,
      commandId: firstCommand.id,
      commandIds,
      event: "background_task.notification.batch_completed",
      messageId: persisted.messageId,
      module: "core.runtime",
    });
  } catch (error) {
    this.logger?.warn("Background task notification batch failed", {
      ...traceContextToLogContext(firstCommand.traceContext),
      batchSize: eligibleCommands.length,
      commandId: firstCommand.id,
      commandIds,
      errorMessage: error instanceof Error ? error.message : String(error),
      event: "background_task.notification.batch_failed",
      module: "core.runtime",
    });
  } finally {
    finishForegroundExecution.call(this, foregroundExecution);
  }
}

function beginForegroundExecution(
  this: AgentRuntimeInternal,
  command: RuntimeCommand,
): ActiveForegroundExecutionState {
  const controller = new AbortController();
  const parentAbortSignal = runtimeCommandAbortSignal(command);
  const abortFromParent = (): void => {
    controller.abort(parentAbortSignal?.reason);
  };
  if (parentAbortSignal?.aborted) {
    abortFromParent();
  } else {
    parentAbortSignal?.addEventListener("abort", abortFromParent, { once: true });
  }
  const state: ActiveForegroundExecutionState = {
    controller,
    disposeParentAbort: () => {
      parentAbortSignal?.removeEventListener("abort", abortFromParent);
    },
    foregroundExecutionId: String(command.id),
    preserveQueueAutoDrainOnCancel: false,
  };
// The old Stop only holds the bootstrap outer controller, and the goal verifier/continuation
// The normal turn life cycle has been exceeded. The cancellation domain must cover the entire runtime command in order to be able to
// The junction still hits the same foreground execution.
  this.activeForegroundExecution = state;
  return state;
}

function finishForegroundExecution(
  this: AgentRuntimeInternal,
  state: ActiveForegroundExecutionState,
): void {
  state.disposeParentAbort();
  if (this.activeForegroundExecution === state) {
    this.activeForegroundExecution = undefined;
  }
}

function runtimeCommandAbortSignal(command: RuntimeCommand): AbortSignal | undefined {
  if (
    command.mode === "prompt" ||
    command.mode === "target-continuation" ||
    command.mode === "target-continuation-loop"
  ) {
    return command.options?.abortSignal;
  }
  return undefined;
}

export function stopActiveForegroundExecution(
  this: AgentRuntimeInternal,
  options: StopActiveForegroundExecutionOptions = {},
): StopActiveForegroundExecutionResult {
  const active = this.activeForegroundExecution;
  if (!active || active.controller.signal.aborted) {
    return { kind: "idle" };
  }
  if (
    options.expectedForegroundExecutionId !== undefined &&
    options.expectedForegroundExecutionId !== active.foregroundExecutionId
  ) {
    return {
      kind: "mismatch",
      activeForegroundExecutionId: active.foregroundExecutionId,
    };
  }
// The internal preemption of sendQueuedNow used to share the same type of canceled as the user's manual Stop.
// turn catch thus turns queueAutoDrain off. Fix the calling intent to the current foreground
// On execution, it is guaranteed that late TurnComplete after timeout can still retain the original queue authorization.
  active.preserveQueueAutoDrainOnCancel = options.preserveQueueAutoDrainOnCancel === true;
  active.controller.abort(new Error(options.reason ?? "foreground execution stopped"));
  return {
    kind: "stopped",
    foregroundExecutionId: active.foregroundExecutionId,
  };
}

export function getActiveForegroundExecutionId(this: AgentRuntimeInternal): string | undefined {
  return this.activeForegroundExecution?.foregroundExecutionId;
}
