import {
  createConfiguredHookRunner,
  createInMemoryHookRunner,
  createSessionMailboxHookRegistrations,
  createToolExecutor,
  getCurrentTraceContext,
  registerBuiltInTools,
  traceContextToLogContext,
} from "../deps.js";
import type { HookRunner, SessionId, ToolExecutor, TraceContext } from "../deps.js";
import type { AgentRuntimeInternal } from "../internal.js";
import type { AgentRuntimeDeps } from "../types.js";
import { resolveRuntimeEmbeddedSearchEnabled } from "../methods/embedded-search-branch.js";
import { getSessionShellSelectionFromConfig } from "../methods/session-shell-environment.js";
import { createRuntimeSessionModePort } from "../session-mode-port.js";
import { shouldSuppressSealedSubagentBashNotification } from "../../runtime-task/notification-policy.js";
import {
  resolveBuiltInToolAllowlist,
  resolveRuntimeDisallowedTools,
  resolveRuntimeDynamicWorkflowToolsIncluded,
} from "./tool-allowlist.js";
import { isStaleBranchRuntimeTaskEvent } from "../methods/runtime-command-generation.js";
import { resolveEnabledProjectMemoryRoot } from "./project-memory.js";

const DEFAULT_SUBAGENT_BACKGROUND_BASH_MAX_MS = 3_600_000;
const EMPTY_RUNTIME_HOOK_CONFIG = {
  enabled: false,
  events: {},
  maxOutputBytes: 32_768,
  timeoutMs: 60_000,
} as const;

export function initializeRuntimeTooling(
  runtime: AgentRuntimeInternal,
  deps: AgentRuntimeDeps,
  sessionId: SessionId,
): { executor: ToolExecutor; hookRunner?: HookRunner } {
  registerRuntimeBuiltInTools(runtime, deps);
  const hookRunner = createRuntimeHookRunner(runtime, deps, sessionId);
  return {
    executor: deps.toolExecutor ?? createRuntimeToolExecutor(runtime, deps, hookRunner),
    hookRunner,
  };
}

function registerRuntimeBuiltInTools(runtime: AgentRuntimeInternal, deps: AgentRuntimeDeps): void {
  const nodeReplEnabled = runtime.config.runtimeFeatures?.nodeRepl === true;
  const browserUseEnabled = resolveRuntimeBrowserUseEnabled(runtime, deps);
  registerBuiltInTools(runtime.registry, {
    bashTimeoutPolicy: runtime.config.bashTimeoutPolicy,
    includeSkill: Boolean(runtime.skillPort),
    includeAgent: Boolean(runtime.subagentPort),
    includeSendMessage: runtime.subagentPort?.sendMessage !== undefined,
    includeRespondToCoordinator:
      runtime.config.taskType === "subagent_child" && Boolean(deps.coordinatorResponsePort),
// submit_result is only registered in the workflow actor session where workflowSubmitPort is injected. Taking port existence as a gate,
// Independent of taskType: the workflow actor is workflow_child whose runtimeScope is currently "main".
    includeSubmitResult: Boolean(deps.workflowSubmitPort),
// mono subagent: typed declaration. Doors are still ports.
    ...(deps.workflowSubmitSchema === undefined
      ? {}
      : { submitResultSchema: deps.workflowSubmitSchema }),
// escalate is the same as submit_result for the same reason: the port is registered when it is present (no opt-in: the actor most likely to hit the wall is the one that the author has not marked).
    includeEscalate: Boolean(deps.workflowEscalatePort),
    includeWorkflow: Boolean(deps.workflowPort),
    includeAutomation: Boolean(deps.automationPort) && runtime.config.taskType !== "subagent_child",
// offPeakPort is only injected when the host issues offPeakToolEnabled (grayscale/remote gate is on the host side),
// The existence of the port means that the exposure is allowed; the subagent sub-session has the same rules as automation and is not exposed.
    includeOffPeak: Boolean(deps.offPeakPort) && runtime.config.taskType !== "subagent_child",
//Dynamic workflow grayscale gate: opposite of off-peak,
// Port presence cannot be used as a criterion here - the ports of the ten tools are fully equipped in any CLI, and the grayscale is determined by the Host.
//The value is stored in tool-allowlist.ts and shares the same derivation as the branch refresh entry.
    includeDynamicWorkflow: resolveRuntimeDynamicWorkflowToolsIncluded(runtime.config),
// browserControlPort is only a host capability and should not implicitly expose high-privilege node_repl.
// node_repl/browser-use is controlled by the runtimeFeatures derived from the start and stop of ZCode's official browser-use plug-in.
    includeNodeRepl: nodeReplEnabled,
    includeBrowserUse: browserUseEnabled,
    embeddedSearchEnabled: resolveRuntimeEmbeddedSearchEnabled(runtime),
    agentProfiles: runtime.config.subagents?.profiles,
    allowedTools: resolveBuiltInToolAllowlist(runtime.config),
// Structural disabling of workflow_child (CreateWorkflow/SaveWorkflow is invisible suspended due to alwaysAsk;
// ResumeWorkflowRun has been exempted from confirmation but is still listed because "child cannot be re-arranged") in the helper
// Merged with the turn-level list, see the root comment of tool-allowlist.ts.
    disallowedTools: resolveRuntimeDisallowedTools(runtime.config),
  });
}

function createRuntimeHookRunner(
  runtime: AgentRuntimeInternal,
  deps: AgentRuntimeDeps,
  sessionId: SessionId,
): HookRunner | undefined {
  let hookRunner =
    deps.hookRunner ??
    ((runtime.config.hooks?.enabled || deps.workspaceHookSnapshot) && deps.executionPort
      ? createConfiguredHookRunner({
          config: runtime.config.hooks ?? EMPTY_RUNTIME_HOOK_CONFIG,
          emitEvent: async (event) => {
            await runtime.appendEvent(event, getCurrentTraceContext() ?? runtime.rootTraceContext);
          },
          executionPort: deps.executionPort,
          getWorkingDirectory: () => runtime.workingDirectory,
          logger: runtime.logger,
          workspaceHookAdmission: deps.workspaceHookAdmission,
          workspaceHookSnapshot: deps.workspaceHookSnapshot,
        })
      : undefined);

  if (!deps.sessionMailboxPort) {
    return hookRunner;
  }

  hookRunner ??= createInMemoryHookRunner({
    emitEvent: async (event) => {
      if (isStaleBranchRuntimeTaskEvent(runtime, event)) return;
      await runtime.appendEvent(event, getCurrentTraceContext() ?? runtime.rootTraceContext);
    },
    logger: runtime.logger,
  });
  for (const hook of createSessionMailboxHookRegistrations({
    enqueuePendingInput: async (input, traceContext) => {
      const result = await runtime.steerTurn({
        delivery: "guide",
        expectedTurnId: traceContext.turnId,
        input,
        traceContext,
      });
      if (result.kind === "rejected") {
        runtime.logger?.warn("Session mailbox input was not queued", {
          ...traceContextToLogContext(traceContext),
          event: "session.mailbox.queue_rejected",
          module: "core.runtime",
          reason: result.reason,
          status: "completed",
        });
      }
    },
    mailbox: deps.sessionMailboxPort,
    sessionId,
  })) {
    if ("register" in hookRunner && typeof hookRunner.register === "function") {
      hookRunner.register(hook);
    }
  }
  return hookRunner;
}

function createRuntimeToolExecutor(
  runtime: AgentRuntimeInternal,
  deps: AgentRuntimeDeps,
  hookRunner: HookRunner | undefined,
): ToolExecutor {
  const browserUseEnabled = resolveRuntimeBrowserUseEnabled(runtime, deps);
  return createToolExecutor({
    agentTelemetry: runtime.agentTelemetry.port,
    agentTelemetryActorKind: runtime.agentTelemetry.actorKind,
    registry: runtime.registry,
    permissionService: runtime.permissionService,
    permissionBroker: runtime.permissionBroker,
    emitEvent: async (event) => {
      await runtime.appendEvent(event, getCurrentTraceContext() ?? runtime.rootTraceContext);
    },
    enqueueBackgroundTaskNotification: (notification) => {
      runtime.enqueueBackgroundTaskNotification(notification);
    },
    shouldEnqueueBackgroundTaskNotification: (input) =>
      shouldEnqueueRuntimeBackgroundTaskNotification(runtime, input),
    logger: runtime.logger,
    backgroundTaskControlPort: {
      stopBackgroundTask: runtime.stopBackgroundTask.bind(runtime),
    },
    executionPort: deps.executionPort,
    browserControlPort: browserUseEnabled ? deps.browserControlPort : undefined,
    browserDocumentationRoot: browserUseEnabled
      ? runtime.config.runtimeFeatures?.browserDocumentationRoot
      : undefined,
    fileSystemPort: deps.fileSystemPort,
    httpClientPort: deps.httpClientPort,
    imageProcessorPort: deps.imageProcessorPort,
// This port was missed when merging and deleting old model connections; Read paging rendering and entire PDF page count check still rely on host injection.
    pdfDocumentPort: deps.pdfDocumentPort,
    embeddedSearchBackend: runtime.config.embeddedSearchBackend,
    nativeSearchEnhancementsEnabled: runtime.config.nativeSearchEnhancementsEnabled,
    skillPort: deps.skillPort,
    subagentPort: runtime.subagentPort,
    coordinatorResponsePort: deps.coordinatorResponsePort,
    workflowSubmitPort: deps.workflowSubmitPort,
    workflowEscalatePort: deps.workflowEscalatePort,
    artifactStore: deps.artifactStore,
    automationPort: deps.automationPort,
    offPeakPort: deps.offPeakPort,
    sessionStore: deps.sessionStore,
    sessionModePort: createRuntimeSessionModePort(runtime),
    workflowPort: deps.workflowPort,
    dynamicWorkflowRunPort: deps.dynamicWorkflowRunPort,
    dynamicWorkflowSnippetPort: deps.dynamicWorkflowSnippetPort,
    modelCatalogPort: deps.modelCatalogPort,
    runtimeTaskRegistry: runtime.runtimeTaskRegistry,
    readFileState: runtime.readFileState,
    subagentBackgroundBashMaxMs:
      runtime.config.taskType === "subagent_child"
        ? normalizeSubagentBackgroundBashMaxMs(runtime.config.subagents?.backgroundBashMaxMs)
        : undefined,
    getBashShellSelection: () => getSessionShellSelectionFromConfig(runtime.config),
    hookRunner,
    getWorkingDirectory: () => runtime.workingDirectory,
    setWorkingDirectory: runtime.setWorkingDirectory.bind(runtime),
    getWorkspaceRoot: () => runtime.workspaceRoot,
    workspaceIdentity: runtime.config.workspaceIdentity?.toString(),
    remoteSessionId: runtime.config.remoteSessionId,
    clientMode: runtime.config.clientMode,
    deliveryKind: runtime.config.deliveryKind,
    getMemoryRoot: () =>
      deps.memoryRoot ?? resolveEnabledProjectMemoryRoot(runtime.config, runtime.workspaceRoot),
    runtimeScope: runtime.config.taskType === "subagent_child" ? "subagent" : "main",
    permissionTimeoutMs: runtime.config.permissionTimeoutMs,
    sessionId: runtime.sessionId,
    traceContext: runtime.rootTraceContext,
    getMode: () => runtime.config.mode ?? "build",
    maxConcurrency: runtime.config.toolConcurrency?.maxConcurrency,
  });
}

function resolveRuntimeBrowserUseEnabled(
  runtime: AgentRuntimeInternal,
  deps: AgentRuntimeDeps,
): boolean {
  return (
    runtime.config.runtimeFeatures?.browserUse === true && deps.browserControlPort !== undefined
  );
}

function shouldEnqueueRuntimeBackgroundTaskNotification(
  runtime: AgentRuntimeInternal,
  input: {
    status: string;
    taskId: string;
    toolName: string;
    traceContext: TraceContext;
  },
): boolean {
  if (runtime.shuttingDown) {
    runtime.logger?.info?.("Suppressed background task notification during runtime shutdown", {
      ...traceContextToLogContext(input.traceContext),
      event: "runtime.background_task_notification.shutdown_suppressed",
      module: "core.runtime",
      taskId: input.taskId,
      taskStatus: input.status,
      toolName: input.toolName,
    });
    return false;
  }
  const registryTask = runtime.runtimeTaskRegistry.get(input.taskId);
  if (
    !shouldSuppressSealedSubagentBashNotification({
      isSubagentChildRuntime: runtime.config.taskType === "subagent_child",
      notificationSealed: runtime.backgroundTaskNotificationsSealed,
      registryTask,
      toolName: input.toolName,
    })
  ) {
    return true;
  }
  runtime.logger?.info?.("Suppressed sealed subagent background Bash notification", {
    ...traceContextToLogContext(input.traceContext),
    event: "runtime.background_task_notification.suppressed",
    module: "core.runtime",
    reason: runtime.backgroundTaskNotificationSealReason,
    taskId: input.taskId,
    taskStatus: input.status,
    toolName: input.toolName,
  });
  return false;
}

function normalizeSubagentBackgroundBashMaxMs(value: number | undefined): number {
  if (typeof value !== "number" || !Number.isFinite(value) || value <= 0) {
    return DEFAULT_SUBAGENT_BACKGROUND_BASH_MAX_MS;
  }
  return Math.trunc(value);
}
