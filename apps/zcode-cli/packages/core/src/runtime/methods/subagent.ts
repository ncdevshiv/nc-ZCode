/* eslint-disable max-lines -- subagent runtime wiring centrally connects child runtime, tool pool, permissions, MCP and activity watchdog. Splitting requires separate migration. */
import { RESPOND_TO_COORDINATOR_TOOL_NAME } from "@zcode/contracts";
import type { SubagentRunOptions } from "@zcode/contracts";
import {
  defaultScheduler,
  PermissionService,
  defaultPermissionConfig,
  buildExploreAllowedTools,
  buildExploreAgentPrompt,
  createExploreSubagentPort,
  createCoreError,
  CoreErrorType,
} from "../deps.js";
import type {
  ExploreSubagentRuntimeRequest,
  McpConnectionSnapshot,
  McpPort,
  Model,
  ModelSelection,
  SkillContent,
  SkillLoadOutcome,
  SkillOperationOptions,
  SkillPort,
  SubagentPort,
} from "../deps.js";
import { AgentRuntime } from "../agent-runtime.js";
import type { AgentRuntimeInternal } from "../internal.js";
import { cloneModelSelection } from "../model-selection.js";
import { resolveSubagentSelection } from "../helpers/subagent-selection.js";
import type { AgentRuntimeDeps } from "../types.js";
import { toMcpToolName } from "../../mcp/index.js";
import { createBorrowedSubagentMcpAccess } from "../../subagent/borrowed-mcp-port.js";
import { createSubagentMessageSink } from "../../subagent/message-steering.js";
import {
  extractRequiredMcpServerNames,
  matchesRequiredMcpServer,
} from "../../subagent/mcp-config.js";
import { mirrorSubagentToolEvent } from "../../subagent/tool-event-mirror.js";
import { isBuiltInExploreAgentProfile } from "../../subagent/profile.js";
import {
  buildSubagentChildDisallowRules,
  filterSubagentChildToolNames,
} from "../../subagent/tool-policy.js";
import { isSubagentDispatchToolName } from "../../tool/compat.js";
import { resolveEmbeddedSearchBranchCapability } from "../../embedded-search/capability.js";
import { getSessionShellEnvironment } from "./session-shell-environment.js";
import { deriveChildClientPorts } from "../helpers/child-client-ports.js";
import { createCoordinatorResponsePort } from "../../subagent/coordinator-response.js";
import { isStaleBranchRuntimeTaskEvent } from "./runtime-command-generation.js";
import { loadPersistentAgentMemory } from "../../subagent/persistent-memory.js";
import {
  createOfficialCuaPolicy,
  SUBAGENT_COMPUTER_USE_UNAVAILABLE_CODE,
  SUBAGENT_COMPUTER_USE_UNAVAILABLE_MESSAGE,
  type OfficialCuaPolicy,
} from "../../subagent/computer-use-policy.js";
import { computeOfficialCuaServerNames } from "./mcp.js";

export function createDefaultSubagentPort(
  this: AgentRuntimeInternal,
  deps: AgentRuntimeDeps,
): SubagentPort | undefined {
  if (this.config.subagents?.enabled === false) {
    return undefined;
  }

  return createExploreSubagentPort({
    logger: this.logger,
    inactivityTimeoutMs: this.config.subagents?.inactivityTimeoutMs,
    autoBackgroundMs: this.config.subagents?.autoBackgroundMs,
    outputRootDir: this.config.subagents?.outputRootDir,
    profiles: this.config.subagents?.profiles,
    builtInModelSelectionOverrides: this.config.subagents?.builtInModelSelectionOverrides,
    runtimeTaskRegistry: this.runtimeTaskRegistry,
    emitParentEvent: async (event, traceContext) => {
      if (isStaleBranchRuntimeTaskEvent(this, event)) return;
      await this.appendEvent(event, traceContext);
    },
    enqueueParentTaskNotification: (notification) => {
      this.enqueueBackgroundTaskNotification({
        originMeta: notification.originMeta,
        taskId: notification.taskId,
        text: notification.text,
        traceContext: notification.traceContext,
      });
      return undefined;
    },
    getAllowedTools: () => {
      return buildExploreAllowedTools({
        embeddedSearchEnabled: resolveSubagentEmbeddedSearchEnabled(),
      });
    },
    runExploreAgent: async (request, options) => {
      request.reportActivity?.();
      const builtInExplore = isBuiltInExploreAgentProfile(request.profile);
      const agentsMdInstructions =
        request.profile.injectAgentsMd !== false
          ? this.contextSourceSnapshot?.userInstructions
          : undefined;
      const { selection: profileChildSelection, hasConcreteModel } = resolveSubagentSelection({
        profileSelection: request.profile.modelSelection,
        parentSelection: this.getSessionModelSelection(),
        overrideSelection: options?.modelOverride?.selection,
        resolveSelection: deps.resolveEffectiveModelSelection,
      });
      const modelOverride = options?.modelOverride;
      const inheritedModel = !modelOverride && !hasConcreteModel ? options?.model : undefined;
// Core Server override takes precedence over persistence profile and parent model inheritance, but is still just a standard Selection.
      const childSelection = inheritedModel
        ? modelSelectionFromActiveModel(inheritedModel)
        : profileChildSelection;
      const embeddedSearchEnabled = resolveSubagentEmbeddedSearchEnabled();
      const baseChildEnvInfo = this.contextSourceSnapshot?.envInfo ??
        this.config.envInfo ?? {
          cwd: request.workingDirectory,
          platform: "unknown",
          shell: "unknown",
          osVersion: "unknown",
          nodeVersion: "unknown",
        };
      const shellEnvironment = getSessionShellEnvironment(this);
      const bashShellSelection = shellEnvironment?.selection;
      const childEnvInfo = {
        ...baseChildEnvInfo,
        ...(shellEnvironment ? { shell: shellEnvironment.promptShell } : {}),
      };
      const baseAgentPrompt =
        builtInExplore && request.systemPrompt?.trim() === ""
          ? buildExploreAgentPrompt({ embeddedSearchEnabled })
          : request.systemPrompt?.trim();
      const persistentMemory = await loadPersistentAgentMemory({
        fileSystemPort: deps.fileSystemPort,
        logger: this.logger,
        memory: this.config.memory,
        profile: request.profile,
        traceContext: request.traceContext,
        workspaceRoot: request.workspaceRoot,
      });
// The empty agent prompt is not a semantic segment; spelling `\n\n` first will remove the boundary of the missing segment
// Leak to the beginning of persistent Memory. Only non-empty text is combined here, and the left border of the block is added uniformly by the builder.
      const agentPrompt = [baseAgentPrompt, persistentMemory?.prompt]
        .filter((part): part is string => typeof part === "string" && part.length > 0)
        .join("\n\n");
      const childRuntimeEnvInfo = {
        ...childEnvInfo,

        cwd: request.workingDirectory,
      };
      const officialCuaServerNames = computeOfficialCuaServerNames(
        this.config.mcp?.servers ?? {},
        new Set(this.config.mcp?.trustedOfficialCuaServerNames ?? []),
      );
      const preflightCuaPolicy = createOfficialCuaPolicy(
        officialCuaServerNames,
        [],
        this.config.pluginReferenceCatalog,
      );
      await validateSubagentComputerUseConfiguration(request, preflightCuaPolicy, this.skillPort);
      const childMcpAccess = await resolveSubagentMcpAccess.call(
        this,
        request,
        officialCuaServerNames,
      );
      const childToolAllowlist = resolveSubagentToolAllowlist.call(
        this,
        request,
        childMcpAccess.snapshot?.tools.map((descriptor) => toMcpToolName(descriptor)) ?? [],
      );
      validateSubagentMcpRequirements(request, childToolAllowlist, childMcpAccess);
      const childMode = resolveSubagentPermissionMode(
        this.getPlanEnabled() ? "plan" : this.config.mode,
        request.permissionMode,
        builtInExplore,
      );
      const childCuaPolicy = createOfficialCuaPolicy(
        officialCuaServerNames,
        childMcpAccess.parentSnapshot?.tools ?? childMcpAccess.snapshot?.tools ?? [],
        this.config.pluginReferenceCatalog,
      );
      await validateSubagentComputerUseConfiguration(request, childCuaPolicy, this.skillPort);
      const childSkillPort = resolveSubagentSkillPort(
        this.skillPort,
        request.profile.skills,
        childCuaPolicy,
      );
      const baseChildModelFactory = modelOverride
        ? createSubagentOverrideModelFactory(modelOverride, this.modelFactory)
        : inheritedModel
          ? createInheritedSubagentModelFactory(childSelection, inheritedModel, this.modelFactory)
          : this.modelFactory;
      if (!baseChildModelFactory) {
        throw createCoreError(
          CoreErrorType.ConfigurationError,
          `Subagent model factory cannot resolve ${childSelection.providerId}/${childSelection.modelId}`,
          { recoverable: true },
        );
      }
      const childModel = baseChildModelFactory({ selection: childSelection });
      const childModelFactory: NonNullable<AgentRuntimeDeps["modelFactory"]> = (target) =>
        target.selection.providerId === childSelection.providerId &&
        target.selection.modelId === childSelection.modelId &&
        target.selection.options?.reasoningLevel === childSelection.options?.reasoningLevel
          ? childModel
          : baseChildModelFactory(target);
      const parentToolCallId = traceStringAttribute(request.traceContext, "parentToolCallId");
// External interaction port (permission broker + provider runtime headers): with dwf actor, legacy
//Workflow children share the same derivation, and the routing identity falls uniformly to the session of this runtime.
      const childClientPorts = deriveChildClientPorts(
        {
          permissionBroker: this.permissionBroker,
          ...(this.providerRuntimeHeadersPort === undefined
            ? {}
            : { providerRuntimeHeadersPort: this.providerRuntimeHeadersPort }),
        },
        {
          agentId: request.agentId,
          agentType: request.agentType,
          childSessionId: request.sessionId,
          description: request.description,
          parentSessionId: this.sessionId,
          parentToolCallId,
          ...(request.traceContext.turnId === undefined
            ? {}
            : { parentTurnId: request.traceContext.turnId }),
        },
      );
      const mirroredToolNameByChildToolCallId = new Map<string, string>();
      let sessionReadyNotified = false;
      const notifySessionReady = async () => {
        if (sessionReadyNotified) return;
        await request.onSessionReady?.();
        sessionReadyNotified = true;
      };
      this.logger?.debug("Starting subagent child runtime", {
        parentSessionId: this.sessionId,
        childSessionId: request.sessionId,
        agentType: request.agentType,
      });
      const childRuntime = new AgentRuntime(
        request.sessionId,
        {
// The old plan enumeration does not contain basic permissions; it inherits the complete status after splitting to avoid being rolled back to build by the constructor.
          mode: childMode === "plan" ? this.config.mode : childMode,
          planEnabled: childMode === "plan",
// The impact of model selection is not only on the final request.model: MCS, built-in search and token/media budget will be
// The child runtime is pre-shaped according to the default model. Synchronize the child so the entire set of executions must be
// The configuration points to the parent turn snapshot; the runner prohibits it from going to the background, and the provider registry is controlled by the parent turn
// Finally clean up, the snapshot will not become a restorable session configuration.
          modelSelection: cloneModelSelection(childSelection),
          modelContextBudgetStrategy: this.config.modelContextBudgetStrategy,
          workingDirectory: request.workingDirectory,
// The execution model is only projected into the Context by the child Active Model; envInfo does not save the second model fact.
          envInfo: childRuntimeEnvInfo,
// The Explore sub-runtime did not inherit the streaming configuration of the main session before, although the Protocol desktop side has defaulted
// Turn on modelStreaming, the sub-request will still return generateText. Some OpenAI-compatible endpoints are in
// SSE `data:` frames are also returned in non-streaming requests, and generateText will parse and report them as ordinary JSON.
// Invalid JSON response; inheriting the parent configuration allows the child agent to use the same streamText semantics as the main link.
          modelStreaming: this.config.modelStreaming,
          bashTimeoutPolicy: this.config.bashTimeoutPolicy,
          midConversationSystem: this.config.midConversationSystem,
          bashShellSelection,
// The child only reuses the instructions snapshot that has been parsed by the parent runtime; the Project Context is still not inherited.
          currentDate: this.contextSourceSnapshot?.currentDate ?? this.config.currentDate,
          subagentContext: {
            agentPrompt: agentPrompt ?? "",
            ...(agentsMdInstructions ? { userInstructions: agentsMdInstructions } : {}),
          },
          agentName: `zcode-${request.agentType}`,
          maxTurns: request.maxTurns ?? this.config.subagents?.maxTurns ?? 4,
          parentSessionId: this.sessionId,
          taskType: "subagent_child",
//Dynamic workflow grayscale gate must be structurally inherited:
// When the parent session is closed and the child agent is open, the Agent tool becomes a backdoor to bypass grayscale. Default path (child inheritance
//The tool name visible to the parent registry) is enough, but **custom agent profile must be written explicitly
// `allowedTools: ["CreateWorkflow"]` will skip that intersection**, leaving only this one to block it.
          dynamicWorkflowEnabled: this.config.dynamicWorkflowEnabled,
//The default subagent has been adjusted from Explore to general-purpose.
// The toolset can no longer rely on DEFAULT_SUBAGENT_TYPE, otherwise the default general agent will be mistakenly downgraded to a read-only search tool surface.
          toolset: builtInExplore ? "explore" : "main",
          toolAllowlist: childToolAllowlist,
          toolDisallowlist: this.config.toolDisallowlist,
          embeddedSearchBackend: this.config.embeddedSearchBackend,
          nativeSearchEnhancementsEnabled: this.config.nativeSearchEnhancementsEnabled,
          subagents: {
            backgroundBashMaxMs: this.config.subagents?.backgroundBashMaxMs,
            enabled: false,
          },
          mcp: childMcpAccess.config,
        },
        {
          agentTelemetry: this.agentTelemetry.port,
          agentTelemetryCausation: this.agentTelemetry.captureCausation(),
// The life cycle of the foreground child is awaited by the parent Agent Tool, using the real parent-child Span; the background child
// It may end later than the parent Tool/Turn, and can only be used as an independent Trace with Link to retain the causal relationship.
          agentTelemetryCausationMode: request.background ? "linked_root" : "child",
          eventStore: this.eventStore,
          sessionStore: deps.sessionStore,
// The child runtime inherits the parent's model request admission port: the subagent's request provider is also visible.
// They should feed manager signals like the parent (if the parent is an observer, the child is also an observer).
          modelRequestAdmission: this.modelRequestAdmission,
          modelFactory: childModelFactory,
          resolveEffectiveModelSelection: deps.resolveEffectiveModelSelection,
// The sub-runtime itself still uses request.sessionId for event persistence and trace archiving; blocking external interactions
// (permission / AskUserQuestion / provider runtime headers) are always routed back to the parent session——
// The desktop UI only knows the sessionId of the parent task. The derivation converges at deriveChildClientPorts,
// dwf actor follows the same path as legacy workflow child.
          ...childClientPorts,
          coordinatorResponsePort: createCoordinatorResponsePort({
            agentId: request.agentId,
            agentType: request.agentType,
            childSessionId: request.sessionId,
            parentToolCallId,
            enqueue: (input) => this.enqueueSubagentMessage(input),
          }),
// Explore uses independent read-only permission configuration; general-purpose and custom agents inherit the parent permission service.
          permissionService: builtInExplore
            ? new PermissionService(defaultPermissionConfig)
            : this.permissionService,
          toolScheduler: deps.toolScheduler ?? defaultScheduler,
          executionPort: deps.executionPort,
          fileSystemPort: deps.fileSystemPort,
// The Explore sub-runtime will expose WebFetch, but it has not inherited the main runtime before.
// HTTP client port, causing the tool to throw a configuration error before actually sending the request, instead of failing the network request.
          httpClientPort: deps.httpClientPort,
          imageProcessorPort: deps.imageProcessorPort,
          pdfDocumentPort: deps.pdfDocumentPort,
          memoryRoot: persistentMemory?.rootDir,
          mcpPort: childMcpAccess.port,
          skillPort: childSkillPort,
          artifactStore: deps.artifactStore,
          appVersion: this.appVersion,
          eventSink: {
            onSessionEvent: async (event) => {
              request.reportActivity?.();
//The events of the child runtime have been dropped into the library according to childSessionId, but the old link only
// A small number of tool events are mirrored to the parent sink, causing the UI to only get the information when it is opened after subscribing to the child topic.
// hydration, subsequent streaming content will not be updated. raw child event only notifies the parent runtime
// External sinks will not be appended again, so they will not be persisted repeatedly; bootstrap presses event.sessionId again
// Route it to the child publisher.
              await this.notifyEventSinks(event, {
                ...request.traceContext,
                sessionId: request.sessionId,
              });
              const mirroredEvent = mirrorSubagentToolEvent(event, {
                agentId: request.agentId,
                agentType: request.agentType,
                background: request.background,
                childSessionId: request.sessionId,
                description: request.description,
                parentSessionId: this.sessionId,
                parentToolCallId,
                parentTurnId: request.traceContext.turnId,
                toolNameByChildToolCallId: mirroredToolNameByChildToolCallId,
              });
              if (!mirroredEvent) return;

// parent mirror retains original semantics: parent session only sees subagent summary/tool ​​activities, raw child
// The main text will not pollute the parent timeline.
              await this.notifyEventSinks(mirroredEvent, {
                ...request.traceContext,
                sessionId: this.sessionId,
              });
            },
          },
          logger: this.logger,
          traceContext: request.traceContext,
        },
      );

      const resumesExistingChild = request.resumeFromStore === true;
      if (resumesExistingChild) {
        await childRuntime.resumeFromStore({
          traceContext: request.traceContext,
        });
      } else {
// The parent session used to publish SubagentSpawned first, and then the child's first round of executeTurn was dropped.
// When forking concurrently, the directory query will read one less child in between. Here persistence is promoted as a pre-release gate.
        await childRuntime.ensureSessionPersistedForExternalActivity(request.prompt, {
          traceContext: request.traceContext,
        });
      }
      await notifySessionReady();
      if (!resumesExistingChild) {
// The new child's final model may come from inheritance, lite, or explicit override by profile. It's the first round
// The real-time projection fact is also the transcript boundary that must be preserved in cold recovery; resume does not write repeatedly.
        childRuntime.recordPendingModelChange({
          toModel: childSelection,
          toModelLabel: `${childSelection.providerId}/${childSelection.modelId}`,
        });
        await childRuntime.emitModelSelected({
          modelSelection: childSelection,
          effectiveReasoningLevel: childModel.options.reasoningLevel,
          previousModelSelection: null,
          traceContext: request.traceContext,
        });
      }
      request.registerMessageSink?.(createSubagentMessageSink(childRuntime, request));
      try {
        return await childRuntime.executeTurn(request.prompt, undefined, {
          abortSignal: options?.signal,
// The first round of input to the child Runtime comes from the parent Agent, rather than direct input from the real user; retain the source facts to avoid
// Subagent Turn is misclassified as user in Trace and Success Rate reports.
          inputSource: "subagent",
          inputPresentation: "coordinator_input",
          traceContext: request.traceContext,
        });
      } finally {
        const cancelled = options?.signal?.aborted === true;
        childRuntime.sealBackgroundTaskNotifications({
          reason: cancelled ? "subagent_cancelled" : "subagent_terminal",
          traceContext: request.traceContext,
        });
        if (cancelled) {
          await childRuntime.cancelRunningRuntimeBackgroundTasks({
            reason: "subagent_cancelled",
            traceContext: request.traceContext,
          });
        }
      }
    },
  });
}

function resolveSubagentEmbeddedSearchEnabled(): boolean {
  const embeddedSearchDecision = resolveEmbeddedSearchBranchCapability({
    bashAvailable: true,
  });

  return embeddedSearchDecision.useEmbeddedSearchBranch;
}

function createInheritedSubagentModelFactory(
  inheritedSelection: ModelSelection,
  inheritedModel: Model,
  fallbackFactory: AgentRuntimeDeps["modelFactory"],
): NonNullable<AgentRuntimeDeps["modelFactory"]> {
  return (target) => {
    if (
      target.selection.providerId === inheritedSelection.providerId &&
      target.selection.modelId === inheritedSelection.modelId &&
      target.selection.options?.reasoningLevel === inheritedSelection.options?.reasoningLevel
    ) {
// Selection maintains the sparse form of explicit intent; it cannot be compared with the full effectiveness of Active Model
// options comparison, otherwise the default value will inevitably cause reuse failure and let the child reinterpret the variable Registry.
      return inheritedModel;
    }
    return fallbackFactory(target);
  };
}

function modelSelectionFromActiveModel(model: Model): ModelSelection {
  const reasoningLevel = model.options.reasoningLevel;
  return {
    providerId: model.providerId,
    modelId: model.modelId,
    ...(reasoningLevel ? { options: { reasoningLevel } } : {}),
  };
}

function createSubagentOverrideModelFactory(
  override: NonNullable<SubagentRunOptions["modelOverride"]>,
  fallbackFactory: AgentRuntimeDeps["modelFactory"],
): AgentRuntimeDeps["modelFactory"] {
  return (target) => {
    return fallbackFactory({
      ...target,
      selection: override.selection,
      requestDependencies: override.requestDependencies,
    });
  };
}

function resolveSubagentPermissionMode(
  parentMode: AgentRuntimeInternal["config"]["mode"],
  permissionMode: ExploreSubagentRuntimeRequest["permissionMode"],
  builtInExplore: boolean,
): AgentRuntimeInternal["config"]["mode"] {
  switch (permissionMode) {
    case "auto":
      return "auto";
    case "plan":
      return "plan";
    case undefined:
      return builtInExplore ? "yolo" : parentMode;
    default:
      return parentMode;
  }
}

function resolveSubagentToolAllowlist(
  this: AgentRuntimeInternal,
  request: ExploreSubagentRuntimeRequest,
  visibleMcpToolNames: readonly string[],
): readonly string[] {
  const disallowedRules = buildSubagentChildDisallowRules([
    ...(this.config.toolDisallowlist ?? []),
    ...(request.disallowedTools ?? []),
  ]);
  const inheritsAvailableTools =
    request.allowedTools.length === 0 || request.allowedTools.includes("*");
  if (inheritsAvailableTools) {
    const parentAllowedMcpToolNames = filterMcpToolNamesByParentAllowlist(
      visibleMcpToolNames,
      this.config.toolAllowlist,
    );
    const availableToolNames = [
      ...this.getTools()
// The child MCP must use the same batch of descriptors as the parent startup snapshot.
// Cannot be re-derived from another registry view.
        .filter((tool) => tool.permission?.permission !== "mcp")
        .map((tool) => tool.name),
      ...parentAllowedMcpToolNames,
    ];
    return appendCoordinatorResponseTool(
      [...new Set(availableToolNames)]
        .filter((toolName) => !isSubagentDispatchToolName(toolName))
        .filter((toolName) => filterSubagentChildToolNames([toolName], disallowedRules).length > 0),
    );
  }
  if (request.allowedTools.length > 0) {
    return appendCoordinatorResponseTool(
      filterSubagentChildToolNames(request.allowedTools, disallowedRules),
    );
  }
  return appendCoordinatorResponseTool([]);
}

function filterMcpToolNamesByParentAllowlist(
  toolNames: readonly string[],
  parentAllowlist: readonly string[] | undefined,
): readonly string[] {
  if (parentAllowlist === undefined) return toolNames;
  const allowed = new Set(parentAllowlist);
  return toolNames.filter((toolName) => allowed.has(toolName));
}

function isModelVisibleMcpToolName(toolName: string): boolean {
  return toolName.startsWith("mcp__");
}

function appendCoordinatorResponseTool(toolNames: readonly string[]): readonly string[] {
  if (toolNames.includes(RESPOND_TO_COORDINATOR_TOOL_NAME)) {
    return toolNames;
  }

// The child control channel is not constrained by the profile tool list; the global toolDisallowlist still takes effect at the runtime registration boundary.
  return [...toolNames, RESPOND_TO_COORDINATOR_TOOL_NAME];
}

interface ResolvedSubagentMcpAccess {
  config: { enabled?: boolean } | undefined;
  port: McpPort | undefined;
  parentSnapshot: McpConnectionSnapshot | undefined;
  snapshot: McpConnectionSnapshot | undefined;
}

async function resolveSubagentMcpAccess(
  this: AgentRuntimeInternal,
  request: ExploreSubagentRuntimeRequest,
  officialCuaServerNames: ReadonlySet<string>,
): Promise<ResolvedSubagentMcpAccess> {
  if (!shouldBorrowParentMcp(request)) {
    return { config: undefined, parentSnapshot: undefined, port: undefined, snapshot: undefined };
  }

  const scopedServerNames = request.profile.mcpServers?.length
    ? request.profile.mcpServers
    : undefined;
  if (!this.mcpPort || this.config.mcp?.enabled === false) {
    if (scopedServerNames) {
      throw createSubagentMcpUnavailableError(request);
    }
    return {
      config: this.config.mcp?.enabled === false ? { enabled: false } : undefined,
      parentSnapshot: undefined,
      port: undefined,
      snapshot: undefined,
    };
  }

// The child does not own the connection life cycle and can only reuse the startup snapshot created by the parent constructor.
  const parentStartupSnapshot = await this.mcpStartupPromise;
  if (!parentStartupSnapshot) {
    if (scopedServerNames) {
      throw createSubagentMcpUnavailableError(request);
    }
    return { config: undefined, parentSnapshot: undefined, port: undefined, snapshot: undefined };
  }

  const unavailableScopedServerNames = (scopedServerNames ?? []).filter(
    (serverName) => parentStartupSnapshot.statuses[serverName]?.status !== "connected",
  );
  if (unavailableScopedServerNames.length > 0) {
    throw createCoreError(
      CoreErrorType.ConfigurationError,
      `Required MCP server is not connected: ${unavailableScopedServerNames.join(", ")}`,
      {
        context: {
          agentType: request.agentType,
          missingMcpServers: unavailableScopedServerNames,
        },
        recoverable: true,
      },
    );
  }

  const borrowed = createBorrowedSubagentMcpAccess(
    this.mcpPort,
    parentStartupSnapshot,
    scopedServerNames,
    officialCuaServerNames,
  );
  return {
    config: { enabled: true },
    parentSnapshot: parentStartupSnapshot,
    port: borrowed.port,
    snapshot: borrowed.snapshot,
  };
}

function shouldBorrowParentMcp(request: ExploreSubagentRuntimeRequest): boolean {
  if ((request.profile.mcpServers?.length ?? 0) > 0) return true;
  if (request.allowedTools.length === 0 || request.allowedTools.includes("*")) return true;
  return request.allowedTools.some((toolName) => {
    const normalized = toolName.trim();
    return isConcreteMcpToolName(normalized) || isMcpServerSelector(normalized);
  });
}

function validateSubagentMcpRequirements(
  request: ExploreSubagentRuntimeRequest,
  effectiveAllowedTools: readonly string[],
  access: ResolvedSubagentMcpAccess,
): void {
  const inheritsAvailableTools =
    request.allowedTools.length === 0 || request.allowedTools.includes("*");
  if (inheritsAvailableTools) return;

  const normalizedAllowedTools = effectiveAllowedTools.map((toolName) => toolName.trim());
  const requiredToolNames = normalizedAllowedTools.filter(isConcreteMcpToolName);
  const requiredServerNames = extractRequiredMcpServerNames(
    normalizedAllowedTools.filter(isMcpServerSelector),
  );
  if (requiredToolNames.length === 0 && requiredServerNames.length === 0) return;
  if (!access.port || !access.snapshot) {
    throw createSubagentMcpUnavailableError(request, requiredToolNames);
  }
  const snapshot = access.snapshot;

  const unavailableServerNames = requiredServerNames.filter(
    (serverName) => !matchesRequiredMcpServer(serverName, snapshot.statuses),
  );
  if (unavailableServerNames.length > 0) {
    throw createCoreError(
      CoreErrorType.ConfigurationError,
      `Required MCP server is not connected: ${unavailableServerNames.join(", ")}`,
      {
        context: {
          agentType: request.agentType,
          missingMcpServers: unavailableServerNames,
        },
        recoverable: true,
      },
    );
  }

  const visibleToolNames = new Set(snapshot.tools.map((descriptor) => toMcpToolName(descriptor)));
  const missingToolNames = requiredToolNames.filter((toolName) => !visibleToolNames.has(toolName));
  if (missingToolNames.length === 0) return;

  throw createCoreError(
    CoreErrorType.ConfigurationError,
    `Required MCP tool is not available in the parent startup snapshot: ${missingToolNames.join(", ")}`,
    {
      context: {
        agentType: request.agentType,
        missingMcpTools: missingToolNames,
      },
      recoverable: true,
    },
  );
}

function isConcreteMcpToolName(toolName: string): boolean {
  return isModelVisibleMcpToolName(toolName) && !isMcpServerSelector(toolName);
}

function isMcpServerSelector(toolName: string): boolean {
  return toolName === "mcp" || (toolName.startsWith("mcp__") && toolName.endsWith("__*"));
}

function createSubagentMcpUnavailableError(
  request: ExploreSubagentRuntimeRequest,
  requiredToolNames: readonly string[] = [],
) {
  return createCoreError(
    CoreErrorType.ConfigurationError,
    "Subagent MCP is unavailable because the parent startup snapshot is unavailable",
    {
      context: {
        agentType: request.agentType,
        requiredMcpTools: requiredToolNames,
      },
      recoverable: true,
    },
  );
}

function resolveSubagentSkillPort(
  parentSkillPort: SkillPort | undefined,
  skillNames: readonly string[] | undefined,
  cuaPolicy: OfficialCuaPolicy,
): SkillPort | undefined {
  if (!parentSkillPort) {
    return parentSkillPort;
  }
  return new FilteredSkillPort(
    parentSkillPort,
    skillNames && skillNames.length > 0 ? new Set(skillNames) : undefined,
    cuaPolicy,
  );
}

class FilteredSkillPort implements SkillPort {
  constructor(
    private readonly parent: SkillPort,
    private readonly allowedSkills: ReadonlySet<string> | undefined,
    private readonly cuaPolicy: OfficialCuaPolicy,
  ) {}

  async discoverSkills(
    request: Parameters<SkillPort["discoverSkills"]>[0],
    options?: SkillOperationOptions,
  ): Promise<SkillLoadOutcome> {
    const outcome = await this.parent.discoverSkills(request, options);
    const skills = outcome.skills.filter((skill) => this.isAllowedSkill(skill));
    return {
      ...outcome,
      skills,
      totalDiscovered: skills.length,
    };
  }

  async loadSkill(
    request: Parameters<SkillPort["loadSkill"]>[0],
    options?: SkillOperationOptions,
  ): Promise<SkillContent> {
    if (
      this.cuaPolicy.isOfficialSkillRequest(request.name) ||
      (await this.hasUniqueOfficialSkillMatch(request, options))
    ) {
      throw createSubagentComputerUseUnavailableError(request.name);
    }
    const resolvedName = await this.resolveAllowedSkillRequestName(request, options);
    if (!resolvedName) {
      throw createCoreError(
        CoreErrorType.ToolExecutionFailed,
        "Skill is not allowed for subagent",
        {
          context: {
            allowedSkills: this.allowedSkills ? [...this.allowedSkills] : [],
            skill: request.name,
            toolName: "Skill",
          },
          recoverable: true,
        },
      );
    }
    return this.parent.loadSkill({ ...request, name: resolvedName }, options);
  }

  private async hasUniqueOfficialSkillMatch(
    request: Parameters<SkillPort["loadSkill"]>[0],
    options?: SkillOperationOptions,
  ): Promise<boolean> {
    if (request.name.includes(":")) return false;
    const outcome = await this.parent.discoverSkills(
      {
        workingDirectory: request.workingDirectory,
        roots: request.roots,
        trace: request.trace,
      },
      options,
    );
    return isUniqueOfficialSkillRequest(outcome.skills, request.name, this.cuaPolicy);
  }

  private isAllowedSkill(skill: SkillContent["metadata"]): boolean {
    if (this.cuaPolicy.isOfficialSkill(skill)) return false;
    return (
      this.allowedSkills === undefined ||
      this.allowedSkills.has(skill.name) ||
      (skill.qualifiedName !== undefined && this.allowedSkills.has(skill.qualifiedName))
    );
  }

  private async resolveAllowedSkillRequestName(
    request: Parameters<SkillPort["loadSkill"]>[0],
    options?: SkillOperationOptions,
  ): Promise<string | undefined> {
    const outcome = await this.discoverSkills(
      {
        workingDirectory: request.workingDirectory,
        roots: request.roots,
        trace: request.trace,
      },
      options,
    );
    const matches = outcome.skills.filter((skill) => matchesSkillRequestName(skill, request.name));
    if (matches.length === 0) {
      return undefined;
    }
    if (matches.length > 1) {
      throw createCoreError(
        CoreErrorType.ToolExecutionFailed,
        "Skill name is ambiguous for subagent; use the fully qualified skill name",
        {
          context: {
            allowedSkills: this.allowedSkills ? [...this.allowedSkills] : [],
            matchingSkills: matches.map((skill) => skill.qualifiedName ?? skill.name),
            skill: request.name,
            toolName: "Skill",
          },
          recoverable: true,
        },
      );
    }
// The visible skills list will display the plugin skill as a qualified name and declare that bare alias can also be loaded.
// When the child agent is called with bare alias, the filtered metadata is first bound back to avoid the parent from accidentally loading the skill with the same global name.
    return matches[0]?.qualifiedName ?? matches[0]?.name;
  }
}

async function validateSubagentComputerUseConfiguration(
  request: ExploreSubagentRuntimeRequest,
  cuaPolicy: OfficialCuaPolicy,
  parentSkillPort: SkillPort | undefined,
): Promise<void> {
  const explicitServer = request.profile.mcpServers?.find((serverName) =>
    cuaPolicy.serverNames.has(serverName.trim()),
  );
  const explicitTool = request.allowedTools.find(
    (toolName) =>
      cuaPolicy.isOfficialToolRequest(toolName) || cuaPolicy.isOfficialServerSelector(toolName),
  );
  let explicitSkill = request.profile.skills?.find((skillName) =>
    cuaPolicy.isOfficialSkillRequest(skillName),
  );
  if (!explicitSkill && parentSkillPort && request.profile.skills?.length) {
    try {
      const outcome = await parentSkillPort.discoverSkills({
        workingDirectory: request.workingDirectory,
        trace: request.traceContext,
      });
      explicitSkill = request.profile.skills.find((skillName) =>
        isUniqueOfficialSkillRequest(outcome.skills, skillName, cuaPolicy),
      );
    } catch {
      // Skill discovery remains lazy; a later Skill load will surface its own error.
    }
  }
  if (!explicitServer && !explicitTool && !explicitSkill) return;

  throw createCoreError(
    CoreErrorType.ConfigurationError,
    SUBAGENT_COMPUTER_USE_UNAVAILABLE_MESSAGE,
    {
      context: {
        agentType: request.agentType,
        code: SUBAGENT_COMPUTER_USE_UNAVAILABLE_CODE,
        ...(explicitServer ? { mcpServer: explicitServer } : {}),
        ...(explicitTool ? { mcpTool: explicitTool } : {}),
        ...(explicitSkill ? { skill: explicitSkill } : {}),
      },
      recoverable: true,
    },
  );
}

function isUniqueOfficialSkillRequest(
  skills: readonly SkillContent["metadata"][],
  requestName: string,
  cuaPolicy: Pick<OfficialCuaPolicy, "isOfficialSkill">,
): boolean {
  if (requestName.includes(":")) return false;
  const matches = skills.filter((skill) => matchesSkillRequestName(skill, requestName));
  return matches.length === 1 && matches[0] !== undefined && cuaPolicy.isOfficialSkill(matches[0]);
}

function createSubagentComputerUseUnavailableError(skillName: string) {
  return createCoreError(
    CoreErrorType.ToolExecutionFailed,
    SUBAGENT_COMPUTER_USE_UNAVAILABLE_MESSAGE,
    {
      context: {
        code: SUBAGENT_COMPUTER_USE_UNAVAILABLE_CODE,
        skill: skillName,
        toolName: "Skill",
      },
      recoverable: true,
    },
  );
}

function matchesSkillRequestName(skill: SkillContent["metadata"], requestName: string): boolean {
  return skill.name === requestName || skill.qualifiedName === requestName;
}

function traceStringAttribute(
  traceContext: { attributes?: Record<string, string | number | boolean> },
  key: string,
): string | undefined {
  const value = traceContext.attributes?.[key];
  return typeof value === "string" ? value : undefined;
}
