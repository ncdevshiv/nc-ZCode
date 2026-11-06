import {
  CoreErrorType,
  SessionEventType,
  TASK_OUTPUT_ALIASES,
  TASK_OUTPUT_PROVIDER_DESCRIPTION,
  TASK_OUTPUT_TOOL_NAME,
  TaskOutputInputSchema,
  TaskOutputResultSchema,
  TaskOutputResultJsonSchema,
  TaskOutputInputJsonSchema,
  createCoreError,
  type TaskOutputInput,
  type TaskOutputResult,
  type TaskOutputTask,
} from "@zcode/contracts";
import type { RuntimeTaskSnapshot } from "../../runtime-task/registry.js";
import { formatPersistedOutputEnvelope } from "../result-persistence-format.js";
import type {
  ToolEntry,
  ToolExecutionContext,
  ToolHandler,
  ToolHandlerFailure,
  ToolInputValidationContext,
  ToolInputValidationResult,
  ToolPersistedModelContentInput,
} from "../types.js";
import { formatCompactFileSize, projectTask, throwIfAborted } from "./task-output-projection.js";

const TASK_OUTPUT_DEFAULT_LENGTH = 32_000;
const TASK_OUTPUT_MAX_LENGTH = 160_000;
const TASK_OUTPUT_PERSIST_THRESHOLD_CHARS = 100_000;
const TASK_OUTPUT_RESULT_BUDGET_BYTES = 400_000;
const TASK_OUTPUT_PERSIST_PREVIEW_CHARS = 2_000;
const TASK_OUTPUT_POLL_INTERVAL_MS = 100;
const TASK_OUTPUT_ERROR_CODE = {
  TASK_ID_REQUIRED: 1,
  TASK_NOT_FOUND: 2,
} as const;

const taskOutputHandler: ToolHandler = async (input, context) => {
  const parsed = TaskOutputInputSchema.parse(input) as TaskOutputInput;
  const inputFailure = getTaskOutputInputFailure(parsed, context.runtimeTaskRegistry);
  if (inputFailure) return inputFailure;

  const registry = requireRuntimeTaskRegistry(context);

  const initialTask = registry.get(parsed.task_id);
  if (!initialTask) {
    return taskOutputFailure(
      TASK_OUTPUT_ERROR_CODE.TASK_NOT_FOUND,
      `No task found with ID: ${parsed.task_id}`,
    );
  }

  if (!parsed.block) {
    if (isTaskActive(initialTask.status)) {
      return taskOutputResult("not_ready", await projectTask(initialTask, context));
    }
    const projectedTask = await projectTask(initialTask, context);
// notified is a claim that the completion result has been successfully delivered; writing before projection will occur when the read fails or
// Swallow subsequent completion notification when abort. Asynchronous projection will not be forced by the outer layer cancellation race condition
// Stop, so after await returns, the signal must be checked again before submitting the claim.
    throwIfAborted(context.abortSignal);
    markTaskNotified(initialTask, context);
    return taskOutputResult("success", projectedTask);
  }

  await emitWaitingProgress(context);
  const task = await waitForTask(parsed.task_id, parsed.timeout, context);
  if (!task) {
    return taskOutputResult("timeout", null);
  }
  if (isTaskActive(task.status)) {
    return taskOutputResult("timeout", await projectTask(task, context));
  }
  const projectedTask = await projectTask(task, context);
  throwIfAborted(context.abortSignal);
  markTaskNotified(task, context);
  return taskOutputResult("success", projectedTask);
};

export const taskOutputToolEntry: ToolEntry = {
  aliases: TASK_OUTPUT_ALIASES,
  capability: "read output/logs from a background task",
  maxModelChars: TASK_OUTPUT_PERSIST_THRESHOLD_CHARS,
  metadata: {
    name: TASK_OUTPUT_TOOL_NAME,
    description: TASK_OUTPUT_PROVIDER_DESCRIPTION,
    readOnly: true,
    destructive: false,
    concurrentSafe: true,
    maxOutputBytes: TASK_OUTPUT_RESULT_BUDGET_BYTES,
    sideEffectScope: "none",
    riskLevel: "low",
    needsApproval: false,
  },
  handler: taskOutputHandler,
  validateInput: validateTaskOutputInput,
  formatModelContent: formatTaskOutputModelContent,
  formatPersistedModelContent: formatPersistedTaskOutputModelContent,
  inputSchema: TaskOutputInputJsonSchema,
  outputSchema: TaskOutputResultJsonSchema,
  runtimeInputSchema: TaskOutputInputSchema,
  runtimeOutputSchema: TaskOutputResultSchema,
  permission: {
    permission: "taskOutput",
    reason: "TaskOutput reads a background task from the current runtime",
    riskLevel: "low",
    sideEffectScope: "none",
    needsApproval: false,
    patternSources: ["toolName", "input"],
    alwaysAllowPatternSources: ["toolName"],
    denyPriority: "beforeAsk",
  },
  resultBudget: {
    maxInlineBytes: TASK_OUTPUT_RESULT_BUDGET_BYTES,
    maxModelBytes: TASK_OUTPUT_RESULT_BUDGET_BYTES,
    strategy: "artifact",
    preview: {
      maxBytes: TASK_OUTPUT_RESULT_BUDGET_BYTES,
      direction: "head",
    },
    artifact: {
      enabled: true,
      retention: "session",
    },
  },
  resultArtifactContentType: "text/plain",
  timeout: {
    kind: "none",
  },
  cancellation: {
    supported: true,
    cleanup: "none",
    userVisibleMessage: "TaskOutput was cancelled while waiting for the task",
  },
  trace: {
    required: true,
    propagateToAdapters: false,
    recordInput: "summary",
    recordOutput: "summary",
  },
};

function taskOutputFailure(errorCode: number, message: string): ToolHandlerFailure {
  return { result: false, errorCode, message };
}

function validateTaskOutputInput(
  input: unknown,
  context: ToolInputValidationContext,
): ToolInputValidationResult {
  const parsed = TaskOutputInputSchema.parse(input) as TaskOutputInput;
  return getTaskOutputInputFailure(parsed, context.runtimeTaskRegistry) ?? { result: true };
}

function getTaskOutputInputFailure(
  input: TaskOutputInput,
  registry: ToolInputValidationContext["runtimeTaskRegistry"],
): ToolHandlerFailure | undefined {
  if (!input.task_id) {
    return taskOutputFailure(TASK_OUTPUT_ERROR_CODE.TASK_ID_REQUIRED, "Task ID is required");
  }
  if (registry && !registry.get(input.task_id)) {
    return taskOutputFailure(
      TASK_OUTPUT_ERROR_CODE.TASK_NOT_FOUND,
      `No task found with ID: ${input.task_id}`,
    );
  }
  return undefined;
}

function formatTaskOutputModelContent(output: unknown): string {
  const parsed = TaskOutputResultSchema.parse(output);
  const blocks = [`<retrieval_status>${parsed.retrieval_status}</retrieval_status>`];
  const task = parsed.task;
  if (task) {
    blocks.push(`<task_id>${task.task_id}</task_id>`);
    blocks.push(`<task_type>${task.task_type}</task_type>`);
    blocks.push(`<status>${task.status}</status>`);
    if (task.exitCode !== undefined && task.exitCode !== null) {
      blocks.push(`<exit_code>${task.exitCode}</exit_code>`);
    }
    if (task.output.trim()) {
// When there is no real complete file, task_id is not a readable path and cannot be disguised as
// "Full output"; at this time, the original text is retained and the outer large result artifact mechanism continues to process it.
      const content = (
        task.outputFile ? truncateTaskOutput(task.output, task.outputFile) : task.output
      ).trimEnd();
      blocks.push(`<output>\n${content}\n</output>`);
    }
    if (task.error) {
      blocks.push(`<error>${task.error}</error>`);
    }
  }
  return blocks.join("\n\n");
}

function truncateTaskOutput(
  output: string,
  outputPath: string,
  configuredValue = process.env.TASK_MAX_OUTPUT_LENGTH,
): string {
  const maxLength = resolveTaskOutputLength(configuredValue);
  if (output.length <= maxLength) return output;

  const prefix = `[Truncated. Full output: ${outputPath}]\n\n`;
  const tailLength = maxLength - prefix.length;
  return prefix + output.slice(-tailLength);
}

function resolveTaskOutputLength(configuredValue = process.env.TASK_MAX_OUTPUT_LENGTH): number {
  if (!configuredValue) return TASK_OUTPUT_DEFAULT_LENGTH;
  const parsed = Number.parseInt(configuredValue, 10);
  if (Number.isNaN(parsed) || parsed <= 0) return TASK_OUTPUT_DEFAULT_LENGTH;
  return Math.min(parsed, TASK_OUTPUT_MAX_LENGTH);
}

function formatPersistedTaskOutputModelContent(input: ToolPersistedModelContentInput): string {
  return formatPersistedOutputEnvelope({
    content: input.content,
    formatBytes: formatCompactFileSize,
    originalBytes: input.content.length,
    persistedPath: input.persistedPath,
    previewChars: TASK_OUTPUT_PERSIST_PREVIEW_CHARS,
  });
}

async function waitForTask(
  taskId: string,
  timeoutMs: number,
  context: ToolExecutionContext,
): Promise<RuntimeTaskSnapshot | undefined> {
  const startedAt = Date.now();
  while (Date.now() - startedAt < timeoutMs) {
    throwIfAborted(context.abortSignal);
    const task = context.runtimeTaskRegistry?.get(taskId);
    if (!task) return undefined;
    if (!isTaskActive(task.status)) return task;
    await delay(TASK_OUTPUT_POLL_INTERVAL_MS);
  }
  return context.runtimeTaskRegistry?.get(taskId);
}

function delay(milliseconds: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, milliseconds);
  });
}

const isTaskActive = (status: string): boolean => status === "running" || status === "pending";

function markTaskNotified(task: RuntimeTaskSnapshot, context: ToolExecutionContext): void {
  context.runtimeTaskRegistry?.update(task.taskId, (current) =>
    current.notified ? current : { ...current, notified: true },
  );
}

async function emitWaitingProgress(context: ToolExecutionContext): Promise<void> {
  if (!context.emitEvent) return;

  await context.emitEvent({
    id: crypto.randomUUID() as never,
    sessionId: context.sessionId,
    turnId: context.turnId,
    type: SessionEventType.ToolCallProgress,
    timestamp: new Date(),
    traceId: context.traceId,
    sequenceNumber: 0,
    payload: {
      toolCallId: context.toolCallId as never,
      toolName: TASK_OUTPUT_TOOL_NAME,
      elapsedMs: 0,
    },
  });
}

function taskOutputResult(
  retrievalStatus: TaskOutputResult["retrieval_status"],
  task: TaskOutputTask | null,
): TaskOutputResult {
  return {
    retrieval_status: retrievalStatus,
    task,
  };
}

function requireRuntimeTaskRegistry(context: ToolExecutionContext) {
  const registry = context.runtimeTaskRegistry;
  if (registry) return registry;

  throw createCoreError(
    CoreErrorType.ConfigurationError,
    "Runtime task registry is not configured for TaskOutput",
    {
      context: {
        toolCallId: context.toolCallId,
        toolName: TASK_OUTPUT_TOOL_NAME,
      },
      recoverable: false,
    },
  );
}
