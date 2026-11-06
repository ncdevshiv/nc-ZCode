/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import { isAbsolute, join, resolve } from "node:path";
import {
  createInMemorySessionEventStore,
  createNodeToolArtifactStore,
} from "@zcode/adapters/storage";
import { createNodeLoggerFactory } from "@zcode/adapters/logging";
import { createConfig, resolvePath } from "@zcode/adapters/config";
import {
  createNodeExecutionAdapter,
  resolveEffectiveBashShellSelection,
} from "@zcode/adapters/exec";
import { createNodeFileSystemAdapter } from "@zcode/adapters/fs";
import { createNodeWebFetchHttpClientAdapter } from "@zcode/adapters/http";
import { createJimpImageProcessorAdapter } from "@zcode/adapters/image";
import { createPopplerPdfDocumentAdapter } from "@zcode/adapters/pdf";
import { createNodeSessionMailboxAdapter } from "@zcode/adapters/mailbox";
import { createNodeContextSourceAdapter } from "@zcode/adapters/context";
import { createNodeSkillAdapter } from "@zcode/adapters/skills";
import { createMcpAdapter } from "@zcode/adapters/mcp";
import {
  AgentRuntime,
  PermissionService,
  buildPluginReferenceCatalog,
  type AmendWorkflowRunSettingsInput,
  type ResumeSessionResult,
} from "@zcode/core";
import { createModelTelemetry } from "@zcode/telemetry";
import {
  createRootTraceContext,
  traceContextToLogContext,
  type TraceContext,
  createSessionId,
  createSessionEvent,
  type ExecutionShellSelection,
  type MessageId,
} from "@zcode/contracts";
import { isRemoteWorkspaceIdentity, resolveZCodeRuntimeEnv } from "@zcode/shared";
import {
  ZCODE_ATTACHMENT_FAULT_CODES,
  ZCodeAttachmentFaultError,
} from "@zcode/shared/zcode-protocol-v4";

import { createModelAdapter } from "../model-factory.js";
import { StartupTimer, startupNow } from "../startup-logging.js";
import { scheduleStartupLogRetentionCleanup } from "../log-retention.js";
import type {
  PrepareUserExecutionBoundary,
  ResumeOptions,
  ZCodeApp,
  ZCodeAppOptions,
} from "./types.js";
import {
  createConfigCliOverrides,
  isMessageEnabled,
  resolveEffectiveLocale,
  resolveEffectiveConfigResult,
} from "./app-config-options.js";
import { getCliStorageRoot, getModelIoDir, projectIdFromDirectory } from "./paths.js";
import {
  asInputHistoryStore,
  asLocalSettingStore,
  openStartupSessionStore,
  readProjectPermissionMode,
  readSessionModelSelection,
} from "./session-store.js";
import { createWorkflowFacade } from "./workflow-facade.js";
import { createInputFacade } from "./input-facade.js";
import { createPluginFacadeForApp } from "./plugin-facade.js";
import { resolvePluginRuntimeFeatures } from "./plugin-runtime-features.js";
import { createSessionFacade } from "./session-facade.js";
import { resolveAppRuntimeConfig, runtimeConfigLogContext } from "./runtime-config.js";
import {
  collectDynamicWorkflowDisabledSkillPaths,
  DYNAMIC_WORKFLOW_GATED_COMMAND_NAMES,
} from "./dynamic-workflow-gate.js";
import { createWorkspaceHookRuntimeSecurity } from "./workspace-hook-trust.js";
import { createScriptWorkflowBridge } from "./script-workflow-methods.js";
import {
  createDynamicWorkflowRunService,
  isDynamicWorkflowTaskLinkStore,
  resolveDynamicWorkflowJournalStore,
} from "./dynamic-workflow-run-service.js";
import { getWorkflowConcurrencyGovernor } from "./workflow-concurrency-governor.js";
import { createDynamicWorkflowSnippetService } from "./dynamic-workflow-snippet-service.js";
import { createModelCatalogPort } from "./model-catalog-port.js";
import { createDynamicWorkflowRunProgressSink } from "./dynamic-workflow-run-progress-sink.js";
import { createScriptWorkflowAgentRuntime } from "./script-workflow-child-runtime.js";
import { workflowActorModelPolicy } from "./workflow-actor-model.js";
import { workflowActorToolPolicy } from "./workflow-actor-tools.js";
import {
  createNodeReplBrowserBroker,
  injectNodeReplBrowserBroker,
  type NodeReplBrowserBroker,
} from "./node-repl-browser-broker.js";
import { resolveBuiltInNodeReplMcpServers } from "./built-in-node-repl.js";
import { resolveZCodeCustomCommandPrompt } from "../custom-command-prompt.js";
import { resolveZCodeBuiltinPromptCommand } from "../builtin-prompt-command.js";
import { collectDisabledPaths } from "../skill-command-overrides.js";
import { loadPluginAgentProfiles, loadZCodeAgentProfiles } from "../subagents.js";
import { createRuntimeAiSdkModelExecutionConfig } from "../model-config.js";
import { ApiProviderModelRuntime } from "./provider-registry-model-runtime.js";
import {
  completeAppStartup,
  debugRuntimeConfigResolved,
  markConfigurationLoaded,
  markMcpAdapterInitialized,
  markRuntimeConstructed,
  markStorageAdaptersInitialized,
  resolveStartupPlugins,
  startAppStartup,
} from "./startup-marks.js";

function decodePromptAttachmentDataUrl(
  content: string,
  fallbackMime: string,
  maxBytes: number,
): { bytes: Uint8Array; mediaType: string } {
  const commaIndex = content.indexOf(",");
  const headerParts =
    content.slice(0, "data:".length).toLowerCase() === "data:" && commaIndex >= 0
      ? content.slice("data:".length, commaIndex).split(";")
      : [];
  const mediaType = (headerParts.shift()?.trim() || fallbackMime).split(";", 1)[0]!.toLowerCase();
  const payload = commaIndex >= 0 ? content.slice(commaIndex + 1) : "";
  if (
    headerParts.at(-1)?.trim().toLowerCase() !== "base64" ||
    !/^[A-Za-z0-9+/]*={0,2}$/u.test(payload) ||
    payload.length % 4 !== 0
  ) {
    throw new Error("fault.attachment.previewArtifactInvalid");
  }
  if (
    !mediaType.startsWith("image/") &&
    !mediaType.startsWith("video/") &&
    mediaType !== "application/pdf"
  ) {
    throw new Error("fault.attachment.previewNotMedia");
  }
  const bytes = Buffer.from(payload, "base64");
  if (bytes.byteLength > maxBytes) {
    throw new Error("fault.attachment.previewTooLarge");
  }
  return { bytes, mediaType };
}

export async function createZCodeApp(options: ZCodeAppOptions): Promise<ZCodeApp> {
  if (!options?.providerRegistry) {
    throw new Error("createZCodeApp requires a Provider Registry");
  }
  const startupStartedAt = startupNow();
  const appVersion = options.version ?? "0.0.0";
  const sessionId = options.sessionId ?? createSessionId();
  const traceContext = options.traceContext ?? createRootTraceContext({ sessionId });
  const workingDirectory = resolve(options.runtimeConfig?.workingDirectory ?? process.cwd());
  const configResult = resolveEffectiveConfigResult(
    createConfig({
      env: options.env,
      projectConfigPath: options.projectConfigPath,
      workingDirectory,
      workspaceIdentity: options.runtimeConfig?.memory?.workspaceIdentity,
      skipUserConfig: options.skipUserConfig,
      userConfigPath: options.userConfigPath,
      cliOverrides: createConfigCliOverrides(options),
    }),
    options,
  );
  const loggerFactory = options.loggerFactory ?? createNodeLoggerFactory({ env: options.env });
  const logger = loggerFactory.createLogger("zcode").child({
    ...traceContextToLogContext(traceContext),
    module: "bootstrap",
  });
  const startupTimer = new StartupTimer(
    logger,
    {
      ...traceContextToLogContext(traceContext),
      module: "bootstrap",
      startupKind: "zcode_app",
    },
    startupStartedAt,
  );
  startAppStartup({
    hasInjectedModelAdapter: options.modelAdapter !== undefined,
    resume: options.resume === true,
    startupTimer,
  });
  markConfigurationLoaded({
    configResult,
    startupTimer,
  });
  const modelLogger = loggerFactory.createLogger("zcode").child({
    ...traceContextToLogContext(traceContext),
    module: "adapters.model",
  });
  const modelTelemetry = createModelTelemetry({
    owner: options.telemetryOwner,
    sessionId,
  });
  let nodeReplBrowserBroker: NodeReplBrowserBroker | undefined;
  let ownedNodeReplBrowserBroker: NodeReplBrowserBroker | undefined;
  let providerModelRuntime: ApiProviderModelRuntime | undefined;
  try {
    const storageRoot = resolvePath(configResult.config.storage.dir);
    const cliStorageRoot = getCliStorageRoot(storageRoot);
    const modelIoDir = getModelIoDir(
      cliStorageRoot,
      resolveZCodeRuntimeEnv(options.env ?? process.env) === "development",
    );
    const zcodeSubagentProfileOutcome = await loadZCodeAgentProfiles({
      logger,
      storageRoot,
      workingDirectory,
    });
    const zcodeSubagentProfiles = zcodeSubagentProfileOutcome.profiles;
    const pluginOutcome = resolveStartupPlugins({
      cliStorageRoot,
      configResult,
      env: options.env,
      logger,
      options,
      startupTimer,
      workingDirectory,
    });
    const pluginSubagentProfiles = loadPluginAgentProfiles({
      logger,
      plugins: pluginOutcome.plugins,
      reservedProfileNames: zcodeSubagentProfiles.map((profile) => profile.name),
      modelSelectionOverrides: zcodeSubagentProfileOutcome.pluginAgentModelSelectionOverrides,
    }).profiles;
    const pluginRuntimeFeatures = resolvePluginRuntimeFeatures(pluginOutcome);
    const builtInMcpServers = resolveBuiltInNodeReplMcpServers({
      pluginOutcome,
      workingDirectory,
    });
// The user directory has been migrated in place before the loader; memory-compatible bypass cannot be added to the old identity of the project/plug-in.
    const subagentProfiles = [...zcodeSubagentProfiles, ...pluginSubagentProfiles];
    const ownsSessionStore = options.sessionStore === undefined;
    const sessionStore =
      options.sessionStore ?? (await openStartupSessionStore(configResult, startupTimer));
    const localSettingStore = asLocalSettingStore(sessionStore);
    const projectID = projectIdFromDirectory(workingDirectory);
    const persistedMode = options.runtimeConfig?.mode
      ? undefined
      : readProjectPermissionMode(localSettingStore, projectID);
    let { configuredMcpServers, runtimeConfig, untrustedProjectMcpServers } =
      resolveAppRuntimeConfig({
        cliStorageRoot,
        configResult,
        options,
        persistedMode,
        pluginHooks: pluginOutcome.hooks,
        pluginMcpServers: pluginOutcome.mcpServers,
        builtInMcpServers,
        pluginRuntimeFeatures,
        builtInSubagentModelSelectionOverrides:
          zcodeSubagentProfileOutcome.builtInModelSelectionOverrides,
        subagentOutputRootDir: join(cliStorageRoot, "agents"),
        subagentProfiles,
        storageRoot,
        workingDirectory,
        workspaceIdentity: options.runtimeConfig?.memory?.workspaceIdentity,
      });
    const browserControlPort = options.browserControlPort;
    if (
      browserControlPort &&
      pluginRuntimeFeatures.browserUse === true &&
      runtimeConfig.mcp?.servers?.node_repl?.type === "stdio"
    ) {
      nodeReplBrowserBroker =
        options.nodeReplBrowserBroker ??
        (ownedNodeReplBrowserBroker = createNodeReplBrowserBroker({
          browserControlPort,
          logger,
          platform: options.platform,
        }));
      configuredMcpServers = injectNodeReplBrowserBroker(
        configuredMcpServers,
        nodeReplBrowserBroker,
      );
      runtimeConfig.mcp = {
        ...runtimeConfig.mcp,
        servers: injectNodeReplBrowserBroker(
          runtimeConfig.mcp.servers ?? {},
          nodeReplBrowserBroker,
        ),
      };
    }
    startupTimer.mark("ZCode runtime configuration resolved", {
      context: runtimeConfigLogContext(runtimeConfig, workingDirectory),
      event: "bootstrap.app.startup.runtime_config.completed",
      stage: "resolve_runtime_config",
    });
// Plugin dialogue reference: identity catalog in App (Session runtime)
// Freeze once when created. Cold recovery will rebuild the App and naturally obtain the new catalog; existing Sessions will not hot-load new Plugins.
    const pluginReferenceCatalog = buildPluginReferenceCatalog(pluginOutcome.plugins);
    runtimeConfig.pluginReferenceCatalog = pluginReferenceCatalog;
    let runtime: AgentRuntime | undefined;
    const workspaceHookRuntimeSecurity = createWorkspaceHookRuntimeSecurity({
      appVersion,
      logger,
      projectConfigPath: options.projectConfigPath,
      policy: options.workspaceHookPolicy,
      policyProvider: options.workspaceHookPolicyProvider,
      reviewHost: options.workspaceHookReviewHost,
      workspaceHookTrustEnabled: options.workspaceHookTrustEnabled,
      runtimeRoot: configResult.sources.project.workspaceHookRuntimeRoot ?? {
// Fallback only takes effect when config-factory is not exported (theoretically it will not happen).
// Here the runtimeRoot was originally rebuilt unconditionally according to the single-layer runtimeConfig.hooks, and
// config-factory traverses default/user/project/env/cli and the derivation of all layers is inconsistent.
// Causes the review snapshot to be different from the bundleDigest reconstructed by toggle,
// "Under review toggle" was misreported as workspace_hooks_snapshot_mismatch.
        enabled: runtimeConfig.hooks?.enabled === true,
        timeoutMs: runtimeConfig.hooks?.timeoutMs ?? 60_000,
        maxOutputBytes: runtimeConfig.hooks?.maxOutputBytes ?? 32_768,
      },
      sessionId,
      snapshot: configResult.sources.project.workspaceHookSnapshot,
      userConfigPath: configResult.sources.user.path,
      workingDirectory,
      ...(options.workspaceHookReviewHost
        ? {
            emitReviewEvent: async (event) => {
              if (!runtime) throw new Error("ZCode runtime is not initialized yet.");
              await runtime.appendEvent(
                createSessionEvent(event.type, sessionId, event.payload, {
                  traceId: traceContext.traceId,
                }),
                traceContext,
              );
            },
            emitAdmissionEvent: async (event) => {
              if (!runtime) throw new Error("ZCode runtime is not initialized yet.");
              await runtime.appendEvent(
                createSessionEvent(event.type, sessionId, event.payload, {
                  traceId: traceContext.traceId,
                }),
                traceContext,
              );
            },
          }
        : {}),
    });
    const permissionService = new PermissionService({
      allowedTools: new Set(configResult.config.permission.allowedTools),
      autoApproveHighRisk: configResult.config.permission.autoApproveHighRisk,
      disallowedTools: new Set(configResult.config.permission.disallowedTools),
      allowMediumRiskInAutoMode: configResult.config.permission.allowMediumRiskInAuto,
    });
    const inputHistoryStore = options.inputHistoryStore ?? asInputHistoryStore(sessionStore);
    const artifactStore =
      options.artifactStore ??
      createNodeToolArtifactStore({
        imageCacheRootDir: join(storageRoot, "cli", "image-cache"),
        pdfCacheRootDir: join(storageRoot, "cli", "pdf-cache"),
        rootDir: join(storageRoot, "cli", "artifacts"),
        videoCacheRootDir: join(storageRoot, "cli", "video-cache"),
      });
    const imageProcessorPort = options.imageProcessorPort ?? createJimpImageProcessorAdapter();
    const messageEnabled = isMessageEnabled(options.env ?? process.env);
    const sessionMailboxPort =
      options.sessionMailboxPort ??
      (messageEnabled
        ? createNodeSessionMailboxAdapter({
            rootDir: resolvePath(
              (options.env ?? process.env).ZCODE_MAILBOX_ROOT ?? "~/.zcode/mailbox",
            ),
          })
        : undefined);
    markStorageAdaptersInitialized({
      cliStorageRoot,
      hasInjectedArtifactStore: options.artifactStore !== undefined,
      hasInjectedSessionStore: options.sessionStore !== undefined,
      startupTimer,
      storageRoot,
    });
    const mcpPort =
      options.mcpPort ??
      (runtimeConfig.mcp?.enabled === false
        ? undefined
        : (options.mcpPortFactory?.({ workingDirectory }) ??
          createMcpAdapter({
            clientVersion: appVersion,
            env: options.env,
            logger,
            network: {
              httpProxy: configResult.config.network.httpProxy,
              noProxy: configResult.config.network.noProxy,
              caCertFile: configResult.config.network.caCertFile,
            },
            workingDirectory,
          })));
    const ownsMcpPort = options.mcpPort === undefined && mcpPort !== undefined;
    const executionPort =
      options.executionPort ??
      createNodeExecutionAdapter({
        onToolExecResource: options.onToolExecResource,
        network: {
          httpProxy: configResult.config.network.httpProxy,
          noProxy: configResult.config.network.noProxy,
          caCertFile: configResult.config.network.caCertFile,
        },
        outputRootDir: join(storageRoot, "cli", "exec"),
        processEnv: options.env ?? process.env,
      });
    const ownsExecutionPort = options.executionPort === undefined;
    const pdfDocumentPort =
      options.pdfDocumentPort ?? createPopplerPdfDocumentAdapter({ executionPort });
// browser-use control port: only available when injected by the host (desktop), no local fallback (pure CLI without browser dock).
    const fileSystemPort = options.fileSystemPort ?? createNodeFileSystemAdapter();
    const httpClientPort =
      options.httpClientPort ??
      createNodeWebFetchHttpClientAdapter({
        env: options.env ?? process.env,
        timeoutMs: configResult.config.network.timeout,
        proxyUrl: configResult.config.network.httpProxy,
        noProxy: configResult.config.network.noProxy,
        caCertFile: configResult.config.network.caCertFile,
      });
    markMcpAdapterInitialized({
      configuredMcpServers,
      hasInjectedMcpPort: options.mcpPort !== undefined,
      mcpEnabled: runtimeConfig.mcp?.enabled !== false,
      startupTimer,
      trustedMcpServerCount: Object.keys(runtimeConfig.mcp?.servers ?? {}).length,
    });
    debugRuntimeConfigResolved({
      configResult,
      logger,
      runtimeConfig,
    });
    const getRuntime = (): AgentRuntime => {
      if (!runtime) throw new Error("ZCode runtime is not initialized yet.");
      return runtime;
    };
    let resumePrepared = false;
    const resolveDefaultShellSelection = (): ExecutionShellSelection =>
      resolveEffectiveBashShellSelection({
        env: options.env ?? process.env,
        platform: options.platform ?? process.platform,
      }).selection;
    let initialShellSelectionPromise: Promise<ExecutionShellSelection> | undefined;
    const resolveInitialShellSelection = (): Promise<ExecutionShellSelection> => {
      initialShellSelectionPromise ??= (async () =>
        (await options.resolveInitialBashShellSelection?.()) ?? resolveDefaultShellSelection())();
      return initialShellSelectionPromise;
    };
    const initializeSessionShellEnvironment = async (): Promise<void> => {
      getRuntime().initializeSessionShellEnvironmentIfNeeded(await resolveInitialShellSelection());
    };

    const restorePersistedModelSelection = async (): Promise<
      ResumeSessionResult["modelSelection"]
    > => {
      const registry = options.providerRegistry;
      let selection: ResumeSessionResult["modelSelection"];
      try {
// Version migration has been completed after database startup; read-only new fields are restored and migration is not repeated by session.
        selection = await readSessionModelSelection(sessionStore, sessionId);
      } catch (error) {
// Select entry's read/parse failure cannot bring down independent history recovery; leaving real storage errors,
// Don't read old messages or default models to mask failures.
        logger.warn("Session model selection restore failed", {
          error: error instanceof Error ? error.message : String(error),
          event: "session.model_selection.restore_failed",
          sessionId,
        });
      }
      const validation = selection && registry.validateSelection(selection);
// Only checking whether the model exists will rebind the missing gear/deleted selection into the Runtime.
// Offsets unbound initialization. Historical recovery does not require an executable model, only the full selection can be bound.
      getRuntime().setSessionModelSelection(validation?.ok ? selection : undefined);
// The restoration result is to save the intent, not to execute the binding. In the past, I left blank/delete the file here, Host's
// The Selection View will no longer be able to obtain the original intent, and it cannot be re-parsed after account switching and configuration restoration.
      return selection;
    };

    const resumeFromStore = async (resumeOptions?: ResumeOptions): Promise<ResumeSessionResult> => {
      const runtime = getRuntime();
      const unsubscribe = resumeOptions?.onEvent
        ? runtime.subscribeEvents({ onSessionEvent: resumeOptions.onEvent })
        : undefined;

      try {
        const resumeTraceContext = resumeOptions?.traceContext ?? traceContext;
        const modelSelection = await restorePersistedModelSelection();
        await initializeSessionShellEnvironment();
        const result = await runtime.resumeFromStore({
          ...(resumeOptions?.abortSignal ? { abortSignal: resumeOptions.abortSignal } : {}),
//Only pass the caller's original mode; project/global default values ​​cannot be disguised as invocation override.
// Otherwise interactive resume will not be able to restore the truly persistent session mode.
          modeOverride: options.runtimeConfig?.mode,
          persistedMessages: resumeOptions?.persistedMessages,
          traceContext: resumeTraceContext,
        });
        await runtime.activatePausedTargetAfterResume(resumeTraceContext);
        resumePrepared = true;
        return { ...result, modelSelection };
      } finally {
        unsubscribe?.();
      }
    };

    const prepareResume = async (
      submitTraceContext?: TraceContext,
      abortSignal?: AbortSignal,
    ): Promise<void> => {
      if (!options.resume || resumePrepared) return;
      await resumeFromStore({
        ...(abortSignal ? { abortSignal } : {}),
        traceContext: submitTraceContext ?? traceContext,
      });
      resumePrepared = true;
    };

    const prepareUserExecutionBoundary: PrepareUserExecutionBoundary = async (boundaryOptions) => {
// Bash shell snapshots belong to the "first real user execution" boundary, not chat
// input unique state. Ordinary prompt, expert workflow, script workflow are all possible
// As the first model/sub-agent entry of the new session, it must be unified in resume/context
// Settling once before initialization to avoid Shell and Bash execution shell bifurcation seen by the model.
      await initializeSessionShellEnvironment();
      await prepareResume(boundaryOptions?.traceContext, boundaryOptions?.abortSignal);
    };

    const modelExecutionConfig = createRuntimeAiSdkModelExecutionConfig(options.env, {
      appVersion,
      network: configResult.config.network,
      sourceTitle: options.sourceTitle,
    });
    const modelAdapter =
      options.modelAdapter ??
      createModelAdapter({
        env: options.env,
        logger: modelLogger,
        modelIoDir,
        modelIoFullRetentionEnabled: options.modelIoFullRetentionEnabled,
        executionConfig: modelExecutionConfig,
        statusSink: modelTelemetry.statusSink,
        streamIdleTimeoutMs: configResult.config.modelStream.idleTimeoutMs,
      });
    if (options.modelAdapter && modelTelemetry.statusSink) {
      modelAdapter.addStatusSink(modelTelemetry.statusSink);
    }
// Process-level concurrency manager: run service uses its narrow port to
// driver (each actor runtime has a request-level admission port); the main runtime hangs its observer (deps below)——
// No queuing, no cooldown, but counted as flying and feeding signals. Process-level singleton - the quota is based on the account and is not divided by sessions.
// No longer feed signals through adapter level addStatusSink: the same event can only be fed once along the ticket.
    const workflowConcurrencyGovernor = getWorkflowConcurrencyGovernor();
    modelAdapter.setModelIoFullRetentionEnabled(options.modelIoFullRetentionEnabled ?? false);
    providerModelRuntime = new ApiProviderModelRuntime({
      registry: options.providerRegistry,
      modelAdapter,
    });
    providerModelRuntime.start();
// The model factory is constructed in advance before three workflow child assembly lines: script bridge, dwf actor workflow
// Both runtime and expert workflow facade **share** the parent session's factory - after the Registry view is updated
// Only the newly created Model can be seen, and the children do not freeze one copy each.
    const modelFactory = providerModelRuntime.modelFactory;
    const scriptWorkflowFacade = createScriptWorkflowBridge({
      agentTelemetry: modelTelemetry.agentExecution,
      appOptions: options,
      appVersion,
      artifactStore,
      configResult,
      fileSystemPort,
      httpClientPort,
      imageProcessorPort,
      pdfDocumentPort,
      logger,
      mcpPort,
      modelFactory,
      permissionService,
      prepareUserExecutionBoundary,
      getRuntime,
      runtimeConfig,
      sessionId,
      sessionStore,
      storageRoot,
      traceContext,
      workingDirectory,
    });

// workflow run service: the side that actually starts the engine after the confirmation window of CreateWorkflow Allow.
// When journal narrowing fails (store does not have a dwf_* table), the service is not constructed - the port remains undefined,
// CreateWorkflow therefore returns to the placeholder diagnostic path. This is a logged visible downgrade, not a
// The persistent running path will be silently lost (see the file header of dynamic-workflow-run-service.ts for details).
    const dynamicWorkflowJournal = resolveDynamicWorkflowJournalStore(sessionStore, logger);
    const dynamicWorkflowRunPort =
      dynamicWorkflowJournal === undefined
        ? undefined
        : createDynamicWorkflowRunService({
            concurrency: workflowConcurrencyGovernor,
            createActorRuntime: ({
              persona,
              pinnedModel,
              runSubagentModel,
              sessionId: actorSessionId,
              submitPort,
              submitProfile,
              escalatePort,
              modelRequestAdmission,
            }) =>
              createScriptWorkflowAgentRuntime({
                childSessionId: actorSessionId,
                configOverrides: {
// persona's identity (valid name + system) → context builder's workflow subagent path.
// Anonymous / When there is no system, the field is absent, so the builder omits the named clause and persona section.
                  workflowActor: {
                    ...(persona.name === undefined ? {} : { name: persona.name }),
                    ...(persona.system === undefined ? {} : { persona: persona.system }),
                  },
// The tool surface of the actor is subtraction (the complete set minus the interactive tools that will hang/override), which can only be passed through configOverrides
// expression (request.opts.tools only allowlist).
                  ...workflowActorToolPolicy(),
// Model surface: `runSubagentModel` is the run’s own choice (`subagent_model`). When present, the entire
// Override, ranked above pin - master agent is not affected by it. Without it and without pin, it will not be covered - child runtime
// The baseline is the current model of the parent session (factory baseline, see script-workflow-child-runtime.ts).
// The pin brought by resume pegs the last actually run model (persona freezes the persistence half of the invariant, see
// priority table of workflow-actor-model.ts); parentSelection takes the **current** selection of the parent session,
// Same as the factory baseline, so the pin comparison will not drift.
                  ...workflowActorModelPolicy(
                    {
                      parentSelection: getRuntime().getSessionModelSelection(),
                      ...(runSubagentModel === undefined ? {} : { runSelection: runSubagentModel }),
                    },
                    pinnedModel,
                  ).configOverrides,
                },
                deps: {
                  agentTelemetry: modelTelemetry.agentExecution,
                  appOptions: options,
                  appVersion,
                  artifactStore,
                  configResult,
                  fileSystemPort,
                  httpClientPort,
                  imageProcessorPort,
                  logger,
                  mcpPort,
// The model factory of the parent session: the actor and the main turn build the Model from the same Registry view.
// Do not freeze one copy each.
                  modelFactory,
                  permissionService,
                  runtime: getRuntime(),
                  runtimeConfig,
                  sessionId,
                  sessionStore,
                  storageRoot,
                  workingDirectory,
                },
// persona no longer replaces the system prompt of the subagent through request.opts.systemPrompt in its entirety, but instead
// workflowActor is superimposed on the base.
// request here is just a placeholder for the factory signature: if opts is empty, it means "do not overwrite anything".
                request: { opts: {} } as never,
                traceContext,
// submit profile → submit_result form:
// `untyped` does not inject ports (core's registration gate is port presence, so there is no such tool - a fully untyped sub-agent
// There is nowhere to submit); `mono` injects ports + typed declarations; `generic` only injects ports (generic declarations).
                ...(submitProfile.kind === "untyped" ? {} : { workflowSubmitPort: submitPort }),
// Expand: JsonSchema of dwf is an interface without index signature, and contracts is Record;
// Literal expansion gets the implicit index signature without having to create a conversion function between the two packages.
                ...(submitProfile.kind === "mono"
                  ? { workflowSubmitSchema: { ...submitProfile.schema } }
                  : {}),
// The upgrade port and the submit port come in and out at the same time: both are control channels of the actor session, and the port is present
// This is the registration gate on the core side. Constant transmission (the port is always constructed in run service), no opt-in——
// The actor most likely to hit an unforeseen wall is the one the author didn't mark.
                workflowEscalatePort: escalatePort,
// Request-level admission port: driver is given when the manager is present, and runner tries to pass the gate first every time.
                ...(modelRequestAdmission === undefined ? {} : { modelRequestAdmission }),
              }),
// Both boundary accounting and transcription truncation read and write actor session messages, and they must go through the same store.
            actorTranscriptStore: sessionStore,
//The byte placement point of the user plane product: with the main session and workflow sub
// The agents share the same tool-artifact store, so the products fall under the same directory tree as other large results.
            artifactStore,
            executionPort,
            fileSystemPort,
            journal: dynamicWorkflowJournal,
            logger,
// Seam of progress projection: an engine event → a session event of the parent session → v4's workflowRuns state key.
// It is necessary to take the runtime append link (instead of pushing eventSink directly): only it does persistence at the same time,
// Complement sequenceNumber and fanout, cold recovery and replayable reconnection are therefore free.
            //
// The three degradation paths of identity gate, runtime not ready and append failure are all in this sink (along with their single tests),
// See the header of dynamic-workflow-run-progress-sink.ts.
            onRunEvent: createDynamicWorkflowRunProgressSink({
// Lazy: the run service is built before the runtime is constructed (it is one of AgentRuntime's dependencies).
              getRuntime,
              logger,
              sessionId,
            }),
// The scope of orphan convergence: the session of this app. Leave **this session** in a non-final state in the journal during construction
// run (the relic of the dead process) converges to failed; the brother session's run is on the fly, so it will never be accidentally injured.
            parentSessionId: sessionId,
// The flying engine makes this session permanent.
// Lazy selection of runtime is the same as onRunEvent: run service is a dependency of AgentRuntime and is constructed earlier;
// And startup only comes from tool calls or v4 commands, at which time the runtime must be ready.
            registerResidencyBlockingWork: (work) => {
              void getRuntime().trackResidencyBlockingWork(work);
            },
// Initiate anchor point: CreateWorkflow is executed in the activity wheel of the parent session,
//The inputId of that round is the message to be hung by the sub-agent agent_step. trace.turnId does not match the active turn
// (Theoretically it shouldn't happen) Just hand over the submit side pocket bottom cast value, and never record the id of other rounds as anchor points.
            resolveLaunchInputId: (trace) => {
              const active = getRuntime().getActiveTurnInfo();
              return active !== undefined && active.turnId === trace.turnId
                ? active.inputId
                : undefined;
            },
            ...(isDynamicWorkflowTaskLinkStore(sessionStore)
              ? { taskLinkStore: sessionStore }
              : {}),
          });
// dwf snippet service: the execution surface of EvalWorkflowSnippet. Deliberately **not** dependent on dwf journal——
// snippet is completely transient (memory journal) and should not be affected by the durability conditions of run service;
// So even if the run port is not constructed due to the absence of journal, the experimental channel is still available.
    const dynamicWorkflowSnippetPort = createDynamicWorkflowSnippetService({
      executionPort,
      fileSystemPort,
      logger,
    });
// Model catalog: The tool layer parses the model name mentioned by the user into the sub-agent selection of workflow run (model-catalog-port.ts).
    const modelCatalogPort = createModelCatalogPort({
      registry: options.providerRegistry,
      currentSelection: () => getRuntime().getSessionModelSelection(),
    });
    runtime = new AgentRuntime(sessionId, runtimeConfig, {
      agentTelemetry: modelTelemetry.agentExecution,
// The main agent's model has requested the manager's observer: release it immediately, but let the manager see its 429 / success.
      modelRequestAdmission: workflowConcurrencyGovernor.observer(),
      eventStore: options.eventStore ?? createInMemorySessionEventStore(),
      sessionStore,
      sessionMailboxPort,
      logger,
      executionPort,
      workspaceHookAdmission: workspaceHookRuntimeSecurity?.admission,
      workspaceHookSnapshot: workspaceHookRuntimeSecurity?.snapshot,
      browserControlPort,
      fileSystemPort,
      httpClientPort,
      imageProcessorPort,
      pdfDocumentPort,
      artifactStore,
      contextSourcePort:
        options.contextSourcePort ?? createNodeContextSourceAdapter({ env: options.env }),
      skillPort:
        configResult.config.features.skill && configResult.config.skills.enabled
          ? (options.skillPort ??
            createNodeSkillAdapter({
              extraRoots: configResult.config.skills.roots,
              extraResolvedRoots: pluginOutcome.skillRoots,
              disabledPaths: [
                ...collectDisabledPaths(configResult.config.skillOverrides),
// The dynamic-workflows skill is not provided when the dynamic workflow grayscale is turned off:
// Ten tools are not present, and asking the model to read "How to write workflow scripts" will only induce it to adjust tools that do not exist.
                ...(runtimeConfig.dynamicWorkflowEnabled === false
                  ? collectDynamicWorkflowDisabledSkillPaths(pluginOutcome.skillRoots)
                  : []),
              ],
            }))
          : undefined,
      mcpPort,
      eventSink: options.eventSink,
      modelFactory,
      modelIoDir,
      providerRuntimeHeadersPort: options.providerRuntimeHeadersPort,
      resolveEffectiveModelSelection: options.resolveEffectiveModelSelection,
      isRemoteWorkspace: () =>
        isRemoteWorkspaceIdentity(runtimeConfig.memory?.workspaceIdentity ?? ""),
      permissionBroker: options.permissionBroker,
      permissionService,
      workflowPort: scriptWorkflowFacade.workflowPort,
      dynamicWorkflowRunPort,
      dynamicWorkflowSnippetPort,
      modelCatalogPort,
      automationPort: options.automationPort,
      offPeakPort: options.offPeakPort,
      appVersion,
      traceContext,
    });
    markRuntimeConstructed({
      hasInjectedModelAdapter: options.modelAdapter !== undefined,
      sessionId,
      startupTimer,
    });
    completeAppStartup({
      sessionId,
      startupTimer,
      workingDirectory,
    });
    scheduleStartupLogRetentionCleanup(loggerFactory, logger);
    const inputFacade = createInputFacade({
      artifactStore,
      customCommandPromptResolver: async (text, resolverOptions) => {
        const builtinPrompt = resolveZCodeBuiltinPromptCommand(text, {
          workingDirectory,
        });
        if (builtinPrompt !== undefined) {
          return builtinPrompt;
        }
        return await resolveZCodeCustomCommandPrompt(text, {
// When the dynamic workflow grayscale is turned off, `/workflow` must not be expanded into plug-in prompt words. The directory side has
// Remove it from the `/` panel, but the user can still type the command name by hand, and both paths must give the same conclusion.
// Absent (TUI, headless, workflow_child) does not have access control, see runtimeConfig field comments.
          ...(runtimeConfig.dynamicWorkflowEnabled === false
            ? { disabledCommandNames: DYNAMIC_WORKFLOW_GATED_COMMAND_NAMES }
            : {}),
          env: options.env,
          executionPort,
          logger,
          projectConfigPath: options.projectConfigPath,
          sessionId,
          signal: resolverOptions?.abortSignal,
          skipUserConfig: options.skipUserConfig,
          traceContext: resolverOptions?.traceContext ?? traceContext,
          userConfigPath: options.userConfigPath,
          workingDirectory,
        });
      },
      inputHistoryStore,
      logger,
      prepareUserExecutionBoundary,
      runtime,
      sessionId,
      traceContext,
    });
    const workflowFacade = createWorkflowFacade({
      agentTelemetry: modelTelemetry.agentExecution,
      appOptions: options,
      appVersion,
      artifactStore,
      cliStorageRoot,
      configResult,
      eventSink: options.eventSink,
      imageProcessorPort,
      pdfDocumentPort,
      logger,
      mcpPort,
      modelFactory,
      permissionService,
      prepareUserExecutionBoundary,
      runtime,
      runtimeConfig,
      sessionId,
      sessionStore,
      storageRoot,
      traceContext,
      workingDirectory,
    });
    const sessionFacade = createSessionFacade({
// Stop the dwf run owned by this session when the App is closed:
// The engine lives in the closure of this App. If you close the App without stopping it, the journal line will stop running and wait for the next orphan convergence.
      ...(dynamicWorkflowRunPort === undefined
        ? {}
        : { closeDynamicWorkflowRuns: () => dynamicWorkflowRunPort.close() }),
      configResult,
      configuredMcpServers,
      ...(options.configuredDefaultModelSelection
        ? {
            configuredDefaultModelSelection: options.configuredDefaultModelSelection,
          }
        : {}),
      executionPort,
      localSettingStore,
      logger,
      loggerFactory,
      mcpPort,
      ownsExecutionPort,
      ownsMcpPort,
      closeNodeReplBrowserBroker: async () => {
        await ownedNodeReplBrowserBroker?.close();
      },
      ownsSessionStore,
      prepareUserExecutionBoundary,
      prepareResume,
      projectID,
      providerRegistry: options.providerRegistry,
      resolveUiLocale: (locale) => resolveEffectiveLocale(locale, options),
      runtime,
      sessionId,
      sessionStore,
      traceContext,
      untrustedProjectMcpServers,
      workingDirectory,
    });

    const closeSession = sessionFacade.close;
    const resolvePromptAttachment = async (input: {
      ref: string;
      mime: string;
      messageId?: string;
      attachmentIndex?: number;
    }): Promise<{ ref: string; mediaType: string; artifactUri?: string }> => {
      let ref = input.ref;
      let mediaType = input.mime;
      let artifactUri: string | undefined;
      if (input.messageId && input.attachmentIndex !== undefined) {
// Previewing a single attachment has decoded the entire session through messages(); long sessions will be scanned synchronously
// All parts, regardless of bad lines, will also cause the target preview to fail. Just click session/message to read at a fixed point.
        const persistedMessage = await sessionStore.messageWithParts({
          sessionID: sessionId,
          messageID: input.messageId as MessageId,
        });
        const persistedAttachment = persistedMessage?.parts.filter((part) => part.type === "file")[
          input.attachmentIndex
        ];
        if (persistedAttachment?.type === "file") {
          mediaType = persistedAttachment.mime;
// The ref of the live row is still the original path; if read directly, the source file will be deleted or overwritten.
// The hot preview will be inconsistent with the cold recovery artifact. Immutable copies of the same message/index must be taken first.
          artifactUri =
            persistedAttachment.metadata?.artifactUri ??
            (persistedAttachment.url.startsWith("zcode-artifact://")
              ? persistedAttachment.url
              : undefined);
          ref =
            artifactUri ??
            (!persistedAttachment.url.startsWith("data:") ? persistedAttachment.url : input.ref);
        }
// The message row will be dropped into the library one by one before the subsequent FilePart; when the target part is not yet visible, it should still be
// Using input.ref already authorized by the current projection cannot create a short preview failure window.
      }
      return { ref, mediaType, ...(artifactUri ? { artifactUri } : {}) };
    };
    return {
      sessionId,
      traceId: traceContext.traceId,
      runtime,
      respondWorkspaceHookReview: (input) =>
        workspaceHookRuntimeSecurity?.respond(
          {
            sessionId: input.sessionId,
            taskId: input.taskId,
            runId: input.runId,
            ...(input.remoteSessionId ? { remoteSessionId: input.remoteSessionId } : {}),
            workspaceIdentity: input.workspaceIdentity,
            bundleDigest: input.bundleDigest,
            reviewFlowId: input.reviewFlowId,
            generation: input.generation,
            interactionId: input.interactionId,
          },
          input.decision,
        ) ??
        Promise.resolve({
          accepted: false as const,
          reasonCode: "workspace_hooks_require_trust_capable_host" as const,
        }),
      toggleWorkspaceHookReviewItem: (input) =>
        workspaceHookRuntimeSecurity?.toggle(
          {
            sessionId: input.sessionId,
            taskId: input.taskId,
            runId: input.runId,
            ...(input.remoteSessionId ? { remoteSessionId: input.remoteSessionId } : {}),
            workspaceIdentity: input.workspaceIdentity,
            bundleDigest: input.bundleDigest,
            reviewFlowId: input.reviewFlowId,
            generation: input.generation,
            interactionId: input.interactionId,
          },
          input.reviewItemId,
          input.enabled,
        ) ??
        Promise.resolve({
          accepted: false as const,
          reasonCode: "workspace_hooks_require_trust_capable_host" as const,
        }),
      revokeWorkspaceHookTrust: (input) =>
        ("hookDeclarationDigests" in input
          ? workspaceHookRuntimeSecurity?.revokeCurrent(input)
          : workspaceHookRuntimeSecurity?.revoke(
              {
                sessionId: input.sessionId,
                taskId: input.taskId,
                runId: input.runId,
                ...(input.remoteSessionId ? { remoteSessionId: input.remoteSessionId } : {}),
                workspaceIdentity: input.workspaceIdentity,
                bundleDigest: input.bundleDigest,
                reviewFlowId: input.reviewFlowId,
                generation: input.generation,
                interactionId: input.interactionId,
              },
              input.reviewItemIds,
            )) ??
        Promise.resolve({
          accepted: false as const,
          reasonCode: "workspace_hooks_require_trust_capable_host" as const,
        }),
      requestWorkspaceHookReview: (input) =>
        workspaceHookRuntimeSecurity?.requestReview({
          workspaceIdentity: input.workspaceIdentity,
          bundleDigest: input.bundleDigest,
        }) ??
        Promise.resolve({
          accepted: false as const,
          reasonCode: "workspace_hooks_require_trust_capable_host" as const,
        }),
// Settings pretrust is called by the server according to the workspace after writing to the disk: reload the Trust store to this
// The coordinator of the session and resends the admission status (see types.ts comments for details).
      reloadWorkspaceHookTrust: () =>
        workspaceHookRuntimeSecurity?.reloadTrust() ?? Promise.resolve(),
      setModelIoFullRetentionEnabled: (enabled) =>
        modelAdapter.setModelIoFullRetentionEnabled(enabled),
      readToolResultArtifact: (uri) =>
        artifactStore.readToolResultArtifact({ uri, trace: traceContext }),
// The whole process of wire/staging is decoded chunk; only after complete checksum commit
// Restore the existing data-URL artifact form in the CLI process and keep the provider read chain compatible.
      writePromptAttachment: async (input) => {
        const artifact = await artifactStore.writeToolResultArtifact({
          content: `data:${input.mime};base64,${Buffer.from(input.bytes).toString("base64")}`,
          contentType: "text/plain",
          retention: "session",
          sessionId,
          toolCallId: `prompt-attachment-upload-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`,
          toolName: "prompt-attachment:upload",
          trace: traceContext,
        });
        if (
          input.mime.startsWith("image/") ||
          input.mime.startsWith("video/") ||
          input.mime.split(";", 1)[0]?.trim().toLowerCase() === "application/pdf"
        ) {
// Derived media is just a rebuildable cache; real IO failures don't break durable data URLs, and final request projections ensure again.
          void artifactStore
            .primeMediaAttachmentPath?.({
              bytes: input.bytes,
              mediaType: input.mime,
              uri: artifact.uri,
            })
            .catch(() => undefined);
        }
        return { ref: artifact.uri };
      },
      readPromptAttachment: async (input) => {
        const { ref, mediaType } = await resolvePromptAttachment(input);
// Reading must stay within the session runtime: artifacts go to the session store, and paths go to the current
// FileSystemPort, SSH/WSL/Docker will hit the correct remote file system.
        if (ref.startsWith("zcode-artifact://")) {
          const artifact = await artifactStore.readToolResultArtifact({
            uri: ref,
            trace: traceContext,
          });
          return decodePromptAttachmentDataUrl(artifact.content, mediaType, input.maxBytes);
        }
        const read = await fileSystemPort.readBinaryFile({
          path: ref,
          maxBytes: input.maxBytes,
          trace: traceContext,
        });
        return { bytes: read.content, mediaType };
      },
      statPromptAttachment: async (input) => {
        const { ref, mediaType, artifactUri } = await resolvePromptAttachment(input);
        if (artifactUri) {
          if (!artifactStore.statToolResultArtifact) {
            throw new ZCodeAttachmentFaultError(ZCODE_ATTACHMENT_FAULT_CODES.statUnsupported);
          }
          const result = await artifactStore.statToolResultArtifact({
            uri: artifactUri,
            trace: traceContext,
          });
          return {
            totalBytes: result.bytes,
            mediaType: result.contentType || mediaType,
            ...(result.mtimeMs === undefined ? {} : { mtimeMs: result.mtimeMs }),
          };
        }
        const result = await fileSystemPort.stat({ path: ref, trace: traceContext });
        if (result.kind !== "file") {
// Directory/symbolic link/disappeared means "this attachment is no longer a shareable file", use the stable code to throw it up,
// Let the share preflight process according to the determined classification, rather than relying on wrong text guessing.
          throw new ZCodeAttachmentFaultError(ZCODE_ATTACHMENT_FAULT_CODES.statNotFile);
        }
        return {
          totalBytes: result.sizeBytes,
          mediaType,
          ...(result.mtimeMs === undefined ? {} : { mtimeMs: result.mtimeMs }),
        };
      },
      resolvePromptAttachmentPreviewSource: async (input) => {
        const resolved = await resolvePromptAttachment(input);
        if (!resolved.mediaType.startsWith("video/")) return { kind: "chunked" };
        if (resolved.artifactUri) {
          if (!artifactStore.ensureMediaAttachmentPath) return { kind: "chunked" };
          try {
            const materialized = await artifactStore.ensureMediaAttachmentPath({
              uri: resolved.artifactUri,
              mediaType: resolved.mediaType,
            });
            if (materialized.status === "ready" && materialized.path.trim()) {
              return {
                kind: "local_path",
                path: materialized.path,
                mediaType: resolved.mediaType,
              };
            }
          } catch {
// The artifact remains an immutable fact; failure to fork the file only allows the gateway to fall back on the artifact chunk.
          }
          return { kind: "chunked" };
        }
        if (isAbsolute(resolved.ref)) {
          return {
            kind: "local_path",
            path: resolved.ref,
            mediaType: resolved.mediaType,
          };
        }
        return { kind: "chunked" };
      },
      ...sessionFacade,
      close: async () => {
        try {
          await closeSession?.();
        } finally {
          try {
            providerModelRuntime?.dispose();
          } finally {
            await modelTelemetry.shutdown();
          }
        }
      },
      ...workflowFacade,
      ...scriptWorkflowFacade,
// Reading side of dwf event log. **Optional capability**: When the journal is unavailable, the run service will not be constructed at all.
// This method is absent, and the v4 gateway's ability to structure responses based on this does not support errors - "no event" and
// "This session does not have this capability" must be distinguishable by the renderer.
      ...(dynamicWorkflowRunPort === undefined
        ? {}
        : {
            listDynamicWorkflowRunEvents: async (input: {
              runId: string;
              afterSequence?: number;
              limit?: number;
            }) =>
              dynamicWorkflowRunPort.listEvents(input.runId, {
                ...(input.afterSequence === undefined
                  ? {}
                  : { afterSequence: input.afterSequence }),
                ...(input.limit === undefined ? {} : { limit: input.limit }),
              }),
          }),
//Read the **user interface product** of workflow run. Register all three together,
// Absent together: they are three slices of the same journal reading surface. Partial presence will only allow the UI to get a card.
//The side panel cannot be opened. The three port members are all optional (the stub port does not accompany the operation), so they are detected one by one.
// ⚠ Terminology: artifact = the output that a script publishes to the user, not the `output` (top-level return value) on the port.
      ...(dynamicWorkflowRunPort === undefined ||
      typeof dynamicWorkflowRunPort.listArtifacts !== "function" ||
      typeof dynamicWorkflowRunPort.listArtifactItems !== "function" ||
      typeof dynamicWorkflowRunPort.readArtifact !== "function"
        ? {}
        : {
            listDynamicWorkflowRunArtifacts: async (input: { runId: string }) =>
              dynamicWorkflowRunPort.listArtifacts!(input.runId),
            listDynamicWorkflowRunArtifactItems: async (input: {
              runId: string;
              artifactId: string;
              afterSequence?: number;
              limit: number;
            }) =>
              dynamicWorkflowRunPort.listArtifactItems!(input.runId, input.artifactId, {
                ...(input.afterSequence === undefined
                  ? {}
                  : { afterSequence: input.afterSequence }),
                limit: input.limit,
              }),
            readDynamicWorkflowRunArtifact: async (input: {
              runId: string;
              artifactId: string;
              version: number;
            }) =>
              dynamicWorkflowRunPort.readArtifact!(input.runId, input.artifactId, input.version),
          }),
// workspace of workflow run transcript: two words together
// Registration and absence at the same time, the three reasons are the same as the product.
      ...(dynamicWorkflowRunPort === undefined ||
      typeof dynamicWorkflowRunPort.listWorkspaceNodes !== "function" ||
      typeof dynamicWorkflowRunPort.readWorkspaceNodeResult !== "function"
        ? {}
        : {
            listDynamicWorkflowRunWorkspaceNodes: async (input: { runId: string }) =>
              dynamicWorkflowRunPort.listWorkspaceNodes!(input.runId),
            readDynamicWorkflowRunNodeResult: async (input: {
              runId: string;
              siteId: string;
              ordinal: number;
              maxBytes: number;
            }) =>
              dynamicWorkflowRunPort.readWorkspaceNodeResult!(
                input.runId,
                input.siteId,
                input.ordinal,
                { maxBytes: input.maxBytes },
              ),
          }),
// Session-level life cycle reading of workflow run (on-the-fly counting + billing subscription). The consumer is the host's provider registry
// Security boundary: The sub-agents share the live adapter of this session and cannot replace the registry during the flight run. The conditions for absence are the same as above.
      ...(dynamicWorkflowRunPort === undefined ? {} : {}),
// Enumeration side of workflow run (discovery query after restart). The ability absence condition is the same as above; the port's listRunsForSession
// It is an optional member, and this ability will not be registered when the method is absent.
      ...(dynamicWorkflowRunPort === undefined ||
      typeof dynamicWorkflowRunPort.listRunsForSession !== "function"
        ? {}
        : {
            listDynamicWorkflowRuns: async (input: { limit?: number }) =>
              dynamicWorkflowRunPort.listRunsForSession!(input.limit),
          }),
//Cold replay of workflow run. The conditions for ability absence are the same as above.
      ...(dynamicWorkflowRunPort === undefined ||
      typeof dynamicWorkflowRunPort.replayProgressForSession !== "function"
        ? {}
        : {
            replayDynamicWorkflowRuns: async (input: { excludeRunIds: ReadonlySet<string> }) =>
              dynamicWorkflowRunPort.replayProgressForSession!(input),
          }),
//Resume of dwf run. The conditions for absent ability are the same as above; in addition
// The resume of the port is an optional member (the stub port does not accompany the run), and this capability will not be registered when the method is absent——
// For renderer, "port absence" and "method absence" are the same business fact.
      ...(dynamicWorkflowRunPort === undefined ||
      typeof dynamicWorkflowRunPort.resume !== "function"
        ? {}
        : {
            resumeWorkflowRun: async (input: { workId: string; name?: string }) => {
              const result = await dynamicWorkflowRunPort.resume!(input.workId);
              if (result.ok) {
// The tracking arm must follow a successful resume: registry registration (recovery guardrail), backgroundWorks
// Entry (cancellable), final state notification. port.resume has replaced the registry entry first,
// waiter is therefore hung on the new settlement promise (run service file header invariant 5).
                await getRuntime().trackResumedDynamicWorkflowRun({
                  runId: result.runId,
                  ...(result.toolCallId === undefined ? {} : { toolCallId: result.toolCallId }),
                  ...(input.name === undefined ? {} : { name: input.name }),
                  traceContext,
                });
              }
              return result;
            },
          }),
// The hub directly launches a saved workflow. Ability absence condition and
// The resumeWorkflowRun family is consistent: the dwf port is not registered when the entire dwf port is absent (stub / single test host) - the GUI gets it accordingly
// Capability not supported errors are displayed as they are, instead of mistaking "direct startup not supported" as a failed startup.
// Isomorphic to the /goal control wheel: first go to the unified user execution boundary (otherwise the shell selection will be empty before the first persistence,
// Cold recovery returns to legacy fallback), and then the runtime parses + verifies + compiles + launches the wheel + submit.
      ...(dynamicWorkflowRunPort === undefined
        ? {}
        : {
            startSavedWorkflow: async (input: {
              name: string;
              scope?: "project" | "global";
              args?: Record<string, unknown>;
            }) => {
              await prepareUserExecutionBoundary({ traceContext });
              return await getRuntime().startSavedWorkflowRun({ ...input, traceContext });
            },
          }),
// GUI "Configuration". it
// Inherit the predecessor script, so the port must be able to both amend and read back the script; if one is missing, it will not be registered, and the GUI retrieval capability is not supported.
// The same user execution boundary as startSavedWorkflow: the cold-restored session first restores the Session boundary and then enters the setting wheel.
      ...(dynamicWorkflowRunPort === undefined ||
      typeof dynamicWorkflowRunPort.amend !== "function" ||
      typeof dynamicWorkflowRunPort.getScript !== "function"
        ? {}
        : {
            amendWorkflowRunSettings: async (
              input: Omit<AmendWorkflowRunSettingsInput, "traceContext">,
            ) => {
              await prepareUserExecutionBoundary({ traceContext });
              return await getRuntime().amendWorkflowRunSettings({ ...input, traceContext });
            },
          }),
      ...createPluginFacadeForApp({ configResult, options, workingDirectory }),
      getPluginReferenceCatalog: () => pluginReferenceCatalog,
      getSkillCatalog: async () => {
// The Skill directory belongs to the context initialization result. Cold recovery must first restore the Session boundary and then read
// The snapshot of the new runtime cannot be scanned independently using the old working directory after bypassing resume.
        await prepareUserExecutionBoundary({ traceContext });
        return await getRuntime().getSkillCatalog(traceContext);
      },
      resume: resumeFromStore,
      ...inputFacade,
    };
  } catch (error) {
    providerModelRuntime?.dispose();
    void modelTelemetry.shutdown().catch(() => undefined);
    void ownedNodeReplBrowserBroker?.close();
    startupTimer.fail("ZCode app startup failed", error, {
      context: { sessionId, workingDirectory },
      event: "bootstrap.app.startup.failed",
      stage: "total",
    });
    throw error;
  }
}
