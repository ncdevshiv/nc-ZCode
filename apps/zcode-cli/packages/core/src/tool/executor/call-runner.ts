/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import {
  type AgentTelemetryErrorCategory,
  CoreErrorType,
  createChildTraceContext,
  createCoreError,
  createRootTraceContext,
  getCurrentTraceContext,
  traceContextToLogContext,
  type ToolExecutionSpanWriter,
  type SessionEvent,
} from "@zcode/contracts";
import {
  OFFICIAL_CUA_FRAME_MODEL_CONTENT_PROTECTION,
  attestOfficialCuaFrameContent,
} from "@zcode/zcode-cua/frame-contract";
import {
  normalizeToolExecutionInput,
  prepareInitialToolExecutionInput,
} from "../input-normalization.js";
import { hasOfficialCuaFrameAuthority } from "../../mcp/image-normalization.js";
import type { SkillTelemetryMetadata } from "@zcode/contracts";
import type { ToolExecutionContext, ToolExecutionResult } from "../types.js";
import type { ToolEntry } from "../types.js";
import type { BackgroundTaskTracker } from "./background-tasks.js";
import {
  createErrorResult,
  createPermissionErrorResult,
  createToolHandlerFailureError,
  isToolHandlerFailure,
  isToolHandlerFailureError,
} from "./errors.js";
import { emitToolCallError, emitToolCallResult, emitToolCallStarted } from "./events.js";
import {
  formatHookAdditionalContexts,
  runPostToolUseFailureHooks,
  runPostToolUseHooks,
  runPreToolUseHooks,
} from "./hook-flow.js";
import { resolveToolCallCapabilityFlags } from "./permission-capability.js";
import { resolveToolPermission } from "./permission-flow.js";
import { createMcpToolDisplay, createToolResultDisplay } from "./result-display.js";
import { appendHookAdditionalContexts, serializeOutput } from "./result-serialization.js";
import {
  ToolDeadline,
  executeWithTimeout,
  linkAbortSignal,
  observeToolAdmissionClock,
  resolveTimeoutMs,
} from "./timeout.js";
import { createToolModelStatusSink, withDefaultToolModelStatusSink } from "./model-status-sink.js";
import { runToolCallWithTelemetry } from "./telemetry.js";
import {
  withAutomationCreateLimitTurnStop,
  withPlanExitDeniedTurnStop,
  withTerminalToolTurnStop,
  withWorkflowRefineDeniedFollowUp,
} from "./turn-control.js";
import { mergeToolExecutionTelemetry, readToolExecutionTelemetry } from "../handlers/tool-perf.js";
import type { ToolExecuteOptions, ToolExecutorDeps } from "./types.js";
import { validateInitialModelToolInput, validateInput, validateOutput } from "./validation.js";
import type { ExecutableToolCall } from "../types.js";
import { resolveEmbeddedSearchBranchCapability } from "../../embedded-search/capability.js";
import { resolveToolEntryModelContract } from "../model-contract.js";

export async function executeToolCall(
  deps: ToolExecutorDeps,
  backgroundTasks: BackgroundTaskTracker,
  toolCall: ExecutableToolCall,
  options?: ToolExecuteOptions,
): Promise<ToolExecutionResult> {
  const totalStartedAt = Date.now();
  const entry = isEmptyToolName(toolCall.name) ? undefined : deps.registry.get(toolCall.name);
  const canonicalToolCall =
    entry && toolCall.name !== entry.metadata.name
      ? { ...toolCall, name: entry.metadata.name }
      : toolCall;
// Privacy and cardinality bounds: Unregistered tool names come from model output and cannot be assumed to be controlled enumerations.
// Business errors still retain their real names for model self-repair, and remote traces fall into the fixed unknown bucket.
  const telemetryToolCall = entry ? canonicalToolCall : { ...toolCall, name: "unknown" };
  return runToolCallWithTelemetry(deps, telemetryToolCall, options, (telemetry) =>
    executeToolCallImpl(deps, backgroundTasks, toolCall, totalStartedAt, options, telemetry),
  );
}

async function executeToolCallImpl(
  deps: ToolExecutorDeps,
  backgroundTasks: BackgroundTaskTracker,
  toolCall: ExecutableToolCall,
  totalStartedAt: number,
  options?: ToolExecuteOptions,
  telemetry?: ToolExecutionSpanWriter,
): Promise<ToolExecutionResult> {
  const parentTraceContext =
    options?.traceContext ??
    getCurrentTraceContext() ??
    deps.traceContext ??
    createRootTraceContext({ sessionId: deps.sessionId, turnId: deps.turnId });
  const emptyToolName = isEmptyToolName(toolCall.name);
  const registeredEntry = emptyToolName ? undefined : deps.registry.get(toolCall.name);
  const model = options?.model ?? deps.model;
  const entry = registeredEntry
    ? resolveToolEntryModelContract(registeredEntry, {
        model,
      })
    : undefined;
  const canonicalToolCall =
    entry && toolCall.name !== entry.metadata.name
      ? { ...toolCall, name: entry.metadata.name }
      : toolCall;
  const traceContext = createChildTraceContext(parentTraceContext, {
    sessionId: deps.sessionId,
    turnId: deps.turnId,
    attributes: {
      toolCallId: canonicalToolCall.id,
      toolName: canonicalToolCall.name,
    },
  });
  const traceId = traceContext.traceId;
  const turnId = traceContext.turnId ?? deps.turnId;

  if (!entry) {
    const result = createErrorResult(
      toolCall,
      createCoreError(
        CoreErrorType.ToolNotFound,
        emptyToolName
          ? "Model returned an invalid tool call: tool name is empty."
          : `Tool not found: ${toolCall.name}`,
        {
          context: { toolCallId: toolCall.id, toolName: toolCall.name },
          recoverable: false,
        },
      ),
    );
    if (emptyToolName) {
// Stopping an empty name during the admission phase will cause the model to never receive a pairing result. Reuse
// registry-miss lifecycle, but provider content strictly retains the original blank name returned by the model.
      result.modelContent = `<tool_use_error>Error: No such tool available: ${toolCall.name}</tool_use_error>`;
    }
// registry miss occurs before handler/ToolCallStarted; the old code only
// Returned to the provider, no ToolCallError is issued, and the V4 tool row is permanently stopped at inputStreaming.
    await emitToolCallError(deps, toolCall.id, traceContext, turnId, result.error);
    deps.logger?.warn("Tool call rejected because the tool is not registered", {
      ...traceContextToLogContext(traceContext),
      event: "tool.call.not_found",
      module: "core.tool.executor",
      status: "failed",
      toolCallId: toolCall.id,
      toolName: toolCall.name,
    });
    telemetry?.finishFailed("lookup", "configuration", result.error);
    return result;
  }

  const mode = deps.getMode();

  if (options?.signal?.aborted) {
    const result = createErrorResult(
      canonicalToolCall,
      createCoreError(CoreErrorType.ToolCancelled, "Tool execution cancelled"),
    );
    telemetry?.finishCancelled("abort_signal");
    return result;
  }

  const preparedInitialInput = prepareInitialToolExecutionInput({
    entry,
    input: canonicalToolCall.input,
    logger: deps.logger,
  });
  let executionInput = preparedInitialInput.input;
  const initialInputValidation = validateInitialModelToolInput(
    executionInput,
    entry,
    preparedInitialInput.runtimeValidationIssues,
  );
  if (initialInputValidation) {
    const result = createErrorResult(canonicalToolCall, initialInputValidation);
// Schema failure and registry miss belong to the same handler/ToolCallStarted early exit before; old code
// Only the failure is injected back into the model, and ToolCallError is not issued. Therefore, the V4 tool row stops at
// inputStreaming (CreateWorkflow card continues to display "Writing Workflow"), and the model stacks another one after retrying.
    await emitToolCallError(deps, canonicalToolCall.id, traceContext, turnId, result.error);
    telemetry?.finishFailed("validation", "parse", result.error);
    return result;
  }

  const toolInputValidation = entry.validateInput?.(executionInput, {
    runtimeTaskRegistry: deps.runtimeTaskRegistry,
  });
  if (toolInputValidation && isToolHandlerFailure(toolInputValidation)) {
// Tool-specific semantic verification can only be placed in the handler originally, causing invalid calls to still be executed first.
// PreToolUse, permissions and failure hook; semantic verification must end before hook.
    const result = createErrorResult(
      canonicalToolCall,
      createToolHandlerFailureError(canonicalToolCall, toolInputValidation),
    );
    await emitToolCallError(deps, canonicalToolCall.id, traceContext, turnId, result.error);
// Tool-specific verification returns a normal failure result and does not use the try/catch failure closure executed by the handler.
// Issue a ToolCallError first to update the tool line, and then explicitly mark the telemetry failure to avoid being recorded as abandoned.
    telemetry?.finishFailed("validation", "parse", result.error);
    return result;
  }

// Normalization: Replace the input parameters sent by the model with "execution facts that will occur". The position is deliberately **before hook** - after
// Hook, permission rules, confirmation window load, prepareApproval and handler all read the same input, so
// "The strategy can see the real script", "Cross-version visibility", "Confirmation and execution of the same bytes" three things are done at once.
  if (entry.resolveInput) {
    const workingDirectory = deps.getWorkingDirectory?.();
    const resolution = await entry.resolveInput(executionInput, {
      ...(workingDirectory === undefined ? {} : { workingDirectory }),
      runtimeTaskRegistry: deps.runtimeTaskRegistry,
      ...(deps.dynamicWorkflowRunPort === undefined
        ? {}
        : { dynamicWorkflowRunPort: deps.dynamicWorkflowRunPort }),
      ...(deps.modelCatalogPort === undefined ? {} : { modelCatalogPort: deps.modelCatalogPort }),
      sessionId: deps.sessionId,
    });
    if (isToolHandlerFailure(resolution)) {
// The same life cycle exit as validateInput: If it cannot be parsed, it is something that the model should take back and repair immediately.
// You shouldn't pop up a confirmation window that's doomed to fail first.
      const result = createErrorResult(
        canonicalToolCall,
        createToolHandlerFailureError(canonicalToolCall, resolution),
      );
      await emitToolCallError(deps, canonicalToolCall.id, traceContext, turnId, result.error);
      telemetry?.finishFailed("validation", "parse", result.error);
      return result;
    }
    executionInput = resolution.input;
  }

  const preToolHookResult = await runPreToolUseHooks(
    deps,
    canonicalToolCall,
    executionInput,
    entry,
    mode,
    traceContext,
    options?.signal,
  );
  if (preToolHookResult.permissionBehavior === "deny" || preToolHookResult.preventContinuation) {
    const result = appendPreToolAdditionalContextsToErrorResult(
      withPlanExitDeniedTurnStop(
        createPermissionErrorResult(
          canonicalToolCall,
          preToolHookResult.hookPermissionDecisionReason ??
            preToolHookResult.stopReason ??
            "Blocked by PreToolUse hook",
          {
            decision: "deny",
            mode,
            reason: preToolHookResult.hookPermissionDecisionReason ?? preToolHookResult.stopReason,
            source: "hook.PreToolUse",
          },
        ),
        {
          mode,
          planEnabled: deps.sessionModePort?.isPlanEnabled?.(),
          toolName: canonicalToolCall.name,
        },
      ),
      preToolHookResult.additionalContexts,
    );
    telemetry?.setPermissionDecision("denied");
    telemetry?.finishDenied("policy_denied");
    return result;
  }
  if (preToolHookResult.updatedInput !== undefined) {
    executionInput = normalizeToolExecutionInput({
      entry,
      input: preToolHookResult.updatedInput,
      logger: deps.logger,
      source: "hook",
    });
    const hookInputValidation = validateInput(executionInput, entry);
    if (hookInputValidation) {
// If Hook's modified input verification fails, it will return directly in front of the handler, and the old branch will not be taken.
// The unified append logic of PreToolUse context causes the model to only see the schema error and not the Hook
// Generated diagnostic context; inconsistent with early failure contract of deny, permission-deny.
      const result = appendPreToolAdditionalContextsToErrorResult(
        createErrorResult(canonicalToolCall, hookInputValidation),
        preToolHookResult.additionalContexts,
      );
      telemetry?.finishFailed("validation", "parse", result.error);
      return result;
    }
  }

  const permissionResult = await resolveToolPermission(
    deps,
    canonicalToolCall,
    entry,
    executionInput,
    preToolHookResult,
    mode,
    traceContext,
    options?.signal,
    telemetry,
  );
  if (!permissionResult.allowed) {
    const result = appendPreToolAdditionalContextsToErrorResult(
      withWorkflowRefineDeniedFollowUp(
        withPlanExitDeniedTurnStop(permissionResult.result, {
          mode,
          planEnabled: deps.sessionModePort?.isPlanEnabled?.(),
          toolName: canonicalToolCall.name,
        }),
        { toolName: canonicalToolCall.name },
      ),
      preToolHookResult.additionalContexts,
    );
    if (result.error?.type === CoreErrorType.PermissionDenied) {
      telemetry?.finishDenied("user_denied");
    } else {
      telemetry?.finishFailed(
        "permission",
        errorCategoryForToolError(result.error?.type),
        result.error,
      );
    }
    return result;
  }
  executionInput = permissionResult.executionInput;
  const permissionWaitMs = permissionResult.permissionWaitMs;

  const startTime = Date.now();
// Press **Execute input parameters** to parse the side effect flag once (Bash's read-only command judgment is settled here), and issue it with ToolCallStarted:
// The event precedes the handler, so the subscriber (the dynamic-workflow driver's import cache is closed) knows before the first byte is written to disk.
  await emitToolCallStarted(
    deps,
    canonicalToolCall,
    traceContext,
    turnId,
    startTime,
    createMcpToolDisplay(entry.metadata.mcpPresentation),
    resolveToolCallCapabilityFlags(deps, entry, executionInput),
  );

  deps.logger?.info("Tool call started", {
    ...traceContextToLogContext(traceContext),
    event: "tool.call.started",
    module: "core.tool.executor",
    status: "started",
    toolCallId: canonicalToolCall.id,
    toolName: canonicalToolCall.name,
  });

  const timeoutMs = resolveTimeoutMs(entry, executionInput, deps.defaultTimeoutMs, {
    model,
  });
  const executionAbortController = new AbortController();
  const unlinkParentAbort = linkAbortSignal(options?.signal, executionAbortController);
// Pauseable deadline: The model request inside this call is suspended when queuing in front of the admission gate. Both ends of the queue
// The ModelNetworkStatus session event of this toolCallId arrives, so just block it at the event exit, and the handler has no idea.
  const deadline = new ToolDeadline(timeoutMs);
  const emitEvent =
    deps.emitEvent === undefined
      ? undefined
      : async (event: SessionEvent): Promise<void> => {
          observeToolAdmissionClock(event, canonicalToolCall.id, deadline);
          await deps.emitEvent(event);
        };
  let readFileStateMetadata: ToolExecutionResult["readFileStateMetadata"];
  let failureStage: "handler" | "serialize" | "post_hook" = "handler";
  let skillTelemetryMetadata: SkillTelemetryMetadata | undefined;

  try {
    const model = options?.model ?? deps.model;
    const bashShellSelection = deps.getBashShellSelection?.() ?? deps.bashShellSelection;
    const embeddedSearchDecision = resolveEmbeddedSearchBranchCapability({
      bashAvailable: deps.registry.has("Bash"),
    });
    const context: ToolExecutionContext = {
      toolCallId: canonicalToolCall.id,
      telemetry,
      automationTurn: options?.automationTurn,
      offPeakTurn: options?.offPeakTurn,
      traceContext,
      traceId,
      spanId: traceContext.spanId,
      parentSpanId: traceContext.parentSpanId,
      abortSignal: executionAbortController.signal,
      backgroundTaskControlPort: deps.backgroundTaskControlPort,
      emitEvent,
      executionPort: deps.executionPort,
      browserControlPort: deps.browserControlPort,
      browserDocumentationRoot: deps.browserDocumentationRoot,
      fileSystemPort: deps.fileSystemPort,
      httpClientPort: deps.httpClientPort,
      imageProcessorPort: deps.imageProcessorPort,
      pdfDocumentPort: deps.pdfDocumentPort,
// Model requests inside the tool send status events to the session by default: deadline pauses and driver phases all rely on this stream.
      model: withDefaultToolModelStatusSink(
        model,
        createToolModelStatusSink({ emitEvent, sessionId: deps.sessionId, turnId, traceId }),
      ),
      subagentModelOverride: options?.subagentModelOverride,
      embeddedSearch: {
        ...(deps.embeddedSearchBackend ? { backend: deps.embeddedSearchBackend } : {}),
        enabled: embeddedSearchDecision?.useEmbeddedSearchBranch ?? false,
        ...(deps.nativeSearchEnhancementsEnabled === false ? { findAndGrepEnabled: false } : {}),
      },
      skillPort: deps.skillPort,
      subagentPort: deps.subagentPort,
      coordinatorResponsePort: deps.coordinatorResponsePort,
      workflowSubmitPort: deps.workflowSubmitPort,
      workflowEscalatePort: deps.workflowEscalatePort,
      artifactStore: deps.artifactStore,
      automationPort: deps.automationPort,
      offPeakPort: deps.offPeakPort,
      sessionStore: deps.sessionStore,
      sessionModePort: deps.sessionModePort,
      workflowPort: deps.workflowPort,
      dynamicWorkflowRunPort: deps.dynamicWorkflowRunPort,
      dynamicWorkflowSnippetPort: deps.dynamicWorkflowSnippetPort,
      modelCatalogPort: deps.modelCatalogPort,
      runtimeTaskRegistry: deps.runtimeTaskRegistry,
      readFileState: deps.readFileState,
      recordReadFileStateMetadata: (metadata) => {
        readFileStateMetadata = metadata;
      },
      recordSkillTelemetryMetadata: (metadata) => {
        skillTelemetryMetadata = metadata;
      },
      bashShellSelection,
      setWorkingDirectory: deps.setWorkingDirectory,
      workingDirectory: deps.getWorkingDirectory(),
      workspaceRoot: deps.getWorkspaceRoot(),
      workspaceIdentity: deps.workspaceIdentity,
      remoteSessionId: deps.remoteSessionId,
      clientMode: deps.clientMode,
      deliveryKind: deps.deliveryKind,
      memoryRoot: deps.getMemoryRoot?.(),
      runtimeScope: deps.runtimeScope,
      providerVisibleToolNames: deps.registry
        .list()
        .filter((name) => deps.registry.getMetadata(name)?.providerVisible !== false),
      sessionId: deps.sessionId,
      turnId,
    };

    const output = await executeWithTimeout(
      entry.handler,
      executionInput,
      context,
      deadline,
      executionAbortController,
      entry,
    );
    const durationMs = Date.now() - startTime;
    if (isToolHandlerFailure(output)) {
//The handler uses the return value to express expected business failure; here it only switches to the existing exception control flow,
// Continue to reuse the original failure hook, events and logs without introducing a second set of execution life cycles.
      throw createToolHandlerFailureError(canonicalToolCall, output);
    }
    validateOutput(output, entry);
// node_repl carries both Browser Use and CUA, and cannot mark the entire server as official during registration.
// Only when the CUA SDK result contains producer integrity metadata, atomic frame protection is temporarily turned on for this serialization;
// Otherwise, the general resultBudget will truncate/rearrange image_ref, or the non-authority path will strip the reference.
    const modelOutputEntry = resolveModelOutputEntry(entry, output);
    failureStage = "serialize";
    let serialization = await serializeOutput(
      deps,
      output,
      modelOutputEntry,
      traceContext,
      canonicalToolCall.id,
      executionAbortController.signal,
    );
    failureStage = "post_hook";
    const postToolHookResult = await runPostToolUseHooks(
      deps,
      canonicalToolCall,
      executionInput,
      output,
      serialization.artifactPath,
      traceContext,
      options?.signal,
    );
    serialization = appendHookAdditionalContexts(
      serialization,
      [...preToolHookResult.additionalContexts, ...postToolHookResult.additionalContexts],
      modelOutputEntry,
    );
    const display = createToolResultDisplay(canonicalToolCall.name, output, {
      mcp: entry.metadata.mcpPresentation,
      officialCua: entry.modelContentProtection === OFFICIAL_CUA_FRAME_MODEL_CONTENT_PROTECTION,
    });
    const perf = mergeToolExecutionTelemetry(readToolExecutionTelemetry(output), {
      permissionWaitMs,
// totalMs is the user-perceived tool life cycle: registry lookup, verification, Hook, permission waiting,
// handler, serialization and PostToolUse. durationMs continues to only represent the main execution section of the handler.
      totalMs: Date.now() - totalStartedAt,
    });

    const finalModelContent = serialization.modelContent ?? serialization.content;
    const modelContentProtection = modelOutputEntry.modelContentProtection
      ? attestOfficialCuaFrameContent(finalModelContent, modelOutputEntry.modelContentProtection)
      : undefined;
    if (
      modelOutputEntry.modelContentProtection &&
      Array.isArray(finalModelContent) &&
      finalModelContent.some((block) => block.type === "image") &&
      !modelContentProtection
    ) {
      throw createCoreError(
        CoreErrorType.ToolExecutionFailed,
        "Official CUA frame failed final model-content attestation",
        { recoverable: true },
      );
    }

    const result: ToolExecutionResult = withTerminalToolTurnStop(
      {
        toolCallId: canonicalToolCall.id,
        toolName: canonicalToolCall.name,
        success: true,
        output,
        display,
        modelContent: finalModelContent,
        ...(readFileStateMetadata ? { readFileStateMetadata } : {}),
        performance: perf,
        serialization,
        durationMs,
        startedAt: new Date(startTime),
        completedAt: new Date(),
      },
      { entry },
    );

    await emitToolCallResult(
      deps,
      canonicalToolCall,
      traceContext,
      turnId,
      serialization,
      durationMs,
      display,
      perf,
      skillTelemetryMetadata,
    );

    await backgroundTasks.trackBackgroundTask(canonicalToolCall, output, traceContext, turnId);

    deps.logger?.info("Tool call completed", {
      ...traceContextToLogContext(traceContext),
      durationMs,
      event: "tool.call.completed",
      module: "core.tool.executor",
      status: "completed",
      toolCallId: canonicalToolCall.id,
      toolName: canonicalToolCall.name,
    });

    telemetry?.setOutputBytes(serialization.returnedBytes);
    telemetry?.setOutputTruncated(serialization.truncated);
    telemetry?.finishCompleted();
    return result;
  } catch (error) {
    const durationMs = Date.now() - startTime;
    const failureHookResult = await runPostToolUseFailureHooks(
      deps,
      canonicalToolCall,
      executionInput,
      error,
      traceContext,
      options?.signal,
    );
    let result = createErrorResult(
      canonicalToolCall,
      error instanceof Error ? error : new Error(String(error)),
      durationMs,
    );
    const baseModelContent = result.error
      ? isToolHandlerFailureError(error) && typeof result.modelContent === "string"
        ? result.modelContent
        : result.error.message
      : undefined;
    if (failureHookResult.additionalContexts.length > 0 && baseModelContent) {
      result.modelContent = [
        baseModelContent,
        formatHookAdditionalContexts([
          ...preToolHookResult.additionalContexts,
          ...failureHookResult.additionalContexts,
        ]),
      ].join("\n\n");
    } else if (preToolHookResult.additionalContexts.length > 0 && baseModelContent) {
      result.modelContent = [
        baseModelContent,
        formatHookAdditionalContexts(preToolHookResult.additionalContexts),
      ].join("\n\n");
    }
    result = withAutomationCreateLimitTurnStop(result, {
      error,
      toolName: canonicalToolCall.name,
    });

// After the Skill has been parsed successfully, serialize/post_hook may still fail; error events must also be retained
// resolved metadata, otherwise the failed Skill agent_step cannot be attributed to the specific skill.
    await emitToolCallError(
      deps,
      canonicalToolCall.id,
      traceContext,
      turnId,
      result.error,
      skillTelemetryMetadata,
    );

    deps.logger?.error(
      "Tool call failed",
      error instanceof Error ? error : new Error(String(error)),
      {
        ...traceContextToLogContext(traceContext),
        durationMs,
        event: "tool.call.failed",
        module: "core.tool.executor",
        status: "failed",
        toolCallId: canonicalToolCall.id,
        toolName: canonicalToolCall.name,
      },
    );

    if (options?.signal?.aborted || result.error?.type === CoreErrorType.ToolCancelled) {
      telemetry?.finishCancelled("abort_signal");
    } else {
      telemetry?.finishFailed(
        failureStage,
        errorCategoryForToolError(result.error?.type),
// The original exception is only handed over to Telemetry for controlled desensitization; result.error is an error repackaged for business protocols.
// The source message/type/code used in Trace to locate the root cause cannot be overridden.
        error,
      );
    }
    return result;
  } finally {
    unlinkParentAbort();
  }
}

function resolveModelOutputEntry(entry: ToolEntry, output: unknown): ToolEntry {
  const isSharedNodeRepl =
    entry.metadata.name === "mcp__node_repl__js" ||
    entry.metadata.mcpPresentation?.serverName === "node_repl";
  if (
    entry.modelContentProtection === OFFICIAL_CUA_FRAME_MODEL_CONTENT_PROTECTION ||
    !isSharedNodeRepl ||
    !hasOfficialCuaFrameAuthority(output)
  ) {
    return entry;
  }
  return {
    ...entry,
    modelContentProtection: OFFICIAL_CUA_FRAME_MODEL_CONTENT_PROTECTION,
    resultBudget: {
      ...entry.resultBudget,
      maxInlineBytes: Math.max(entry.resultBudget.maxInlineBytes, 256 * 1024),
      maxModelBytes: Math.max(entry.resultBudget.maxModelBytes, 256 * 1024),
      strategy: "truncate",
      preview: { direction: "head" },
    },
  };
}

function isEmptyToolName(toolName: string): boolean {
  return toolName.trim().length === 0;
}

function errorCategoryForToolError(type: string | undefined): AgentTelemetryErrorCategory {
  switch (type) {
    case CoreErrorType.ConfigurationError:
    case CoreErrorType.ToolNotFound:
      return "configuration";
    case CoreErrorType.PermissionDenied:
    case CoreErrorType.PermissionEscalation:
    case CoreErrorType.PermissionTimeout:
      return "permission";
    case CoreErrorType.InvalidInput:
      return "parse";
    case CoreErrorType.ToolCancelled:
      return "cancelled";
    case CoreErrorType.ToolTimeout:
      return "timeout";
    default:
      return "internal";
  }
}

function appendPreToolAdditionalContextsToErrorResult(
  result: ToolExecutionResult,
  additionalContexts: string[],
): ToolExecutionResult {
  if (result.success || !result.error || additionalContexts.length === 0) return result;

// PreToolUse deny and permission denial will return early before the handler. The old logic is only in the handler.
// The success/exception path adds context, causing Hook to return additionalContext, but the model cannot see it.
  const baseModelContent =
    typeof result.modelContent === "string" ? result.modelContent : result.error.message;
  return {
    ...result,
    modelContent: [baseModelContent, formatHookAdditionalContexts(additionalContexts)].join("\n\n"),
  };
}
