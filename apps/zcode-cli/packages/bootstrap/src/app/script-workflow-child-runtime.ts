import { join } from "node:path";
import { createNodeContextSourceAdapter } from "@zcode/adapters/context";
import { createNodeExecutionAdapter } from "@zcode/adapters/exec";
import { createNodeFileSystemAdapter } from "@zcode/adapters/fs";
import { createNodeWebFetchHttpClientAdapter } from "@zcode/adapters/http";
import { createNodeSkillAdapter } from "@zcode/adapters/skills";
import type { ConfigResult } from "@zcode/adapters/config";
import {
  AgentRuntime,
  type AgentRuntimeConfig,
  type AgentRuntimeDeps,
  type ChildClientPortsContext,
  type PermissionService,
} from "@zcode/core";
import {
  type AgentExecutionTelemetryPort,
  type ContextSourcePort,
  type FileSystemPort,
  type HttpClientPort,
  type ImageProcessorPort,
  type JsonSchema,
  type PdfDocumentPort,
  type Logger,
  type McpPort,
  type ModelRequestAdmission,
  type SessionId,
  type SessionStorePort,
  type ToolArtifactStorePort,
  type TraceContext,
  type WorkflowAgentCallInput,
  type WorkflowEscalatePort,
  type WorkflowSubmitPort,
} from "@zcode/contracts";
import { collectDisabledPaths } from "../skill-command-overrides.js";
import { parseProviderQualifiedModelSelection } from "./provider-registry-selection.js";
import type { ZCodeAppOptions } from "./types.js";

export interface ScriptWorkflowAgentRuntimeDeps {
  agentTelemetry: AgentExecutionTelemetryPort;
  appOptions: ZCodeAppOptions;
  appVersion: string;
  artifactStore?: ToolArtifactStorePort;
  configResult: ConfigResult;
  contextSourcePort?: ContextSourcePort;
  fileSystemPort: FileSystemPort;
  httpClientPort?: HttpClientPort;
  imageProcessorPort: ImageProcessorPort;
  pdfDocumentPort?: PdfDocumentPort;
  logger: Logger;
  mcpPort?: McpPort;
/** The model factory of the parent session: child and main turn build models from the same Registry view and are not frozen separately. */
  modelFactory: NonNullable<AgentRuntimeDeps["modelFactory"]>;
  permissionService: PermissionService;
  runtime: AgentRuntime;
  runtimeConfig: AgentRuntimeConfig;
  sessionId: SessionId;
  sessionStore: SessionStorePort;
  storageRoot: string;
  workingDirectory: string;
}

export function createScriptWorkflowAgentRuntime(input: {
  childSessionId: SessionId;
  deps: ScriptWorkflowAgentRuntimeDeps;
  request: WorkflowAgentCallInput;
  traceContext: TraceContext;
  /**
* Override the configuration slice of the child runtime. dwf actor uses it to drop the tool surface: `workflowActorToolPolicy` is given
* `toolDisallowlist` (subtraction of the full set), while `request.opts.tools` can only express allowlist - neither
* The same degree of freedom.
   */
  configOverrides?: Partial<AgentRuntimeConfig>;
  /**
* Session-level submit port. Injection is to register the submit_result tool for the session (the core registration gate is subject to the existence of the port),
* This is the termination channel for dwf typed ask.
   */
  workflowSubmitPort?: WorkflowSubmitPort;
  /**
* typed `submit_result` declaration for mono subagent:
* When present core registers `{ result: <schema> }` instead of arbitrary JSON. Only meaningful when used with workflowSubmitPort.
   */
  workflowSubmitSchema?: JsonSchema;
  /**
* Session level upgrade port. Injection registers the `escalate` tool for the session
* (The registration gate of the core is also subject to the existence of the port). This is the only channel for help when the actor encounters true blocking.
   */
  workflowEscalatePort?: WorkflowEscalatePort;
  /**
* Model request access port. dwf actor by driver
* Given (each model request attempt passes through the process-level gate first); the sub-agent of the legacy `Workflow` tool does not pass - not subject to the gate,
*Do not feed signals. Enter runtime deps in the same way as both tool ports.
   */
  modelRequestAdmission?: ModelRequestAdmission;
}): AgentRuntime {
// The dwf actor takes the overlay path of the builder via configOverrides.workflowActor. At this time, systemPrompt must be
// Absent (the builder throws an error for both) - the custom system prompt that comes with the parent session must not be leaked to the child agent, so
// Strip it from the inherited configuration instead of overriding it later.
  const { systemPrompt: inheritedSystemPrompt, ...inheritedConfig } = input.deps.runtimeConfig;
  const systemPrompt =
    input.configOverrides?.workflowActor === undefined
      ? (input.request.opts?.systemPrompt ?? inheritedSystemPrompt)
      : undefined;
  const parentSelection = input.deps.runtime.getSessionModelSelection();
  const requestedSelection = input.request.opts?.model
    ? parseProviderQualifiedModelSelection(input.request.opts.model)
    : undefined;
  if (input.request.opts?.model && !requestedSelection) {
    throw new Error(`Workflow child model must be provider-qualified: ${input.request.opts.model}`);
  }
  const modelSelection = requestedSelection ?? parentSelection;
  return new AgentRuntime(
    input.childSessionId,
    {
      ...inheritedConfig,
      ...(systemPrompt === undefined ? {} : { systemPrompt }),
      agentName: input.request.opts?.agentType ?? "zcode-workflow",
      maxTurns: input.request.opts?.maxTurns ?? input.deps.runtimeConfig.maxTurns,
      mode: "yolo",
      modelSelection,
      parentSessionId: input.deps.sessionId,
      subagents: { enabled: false },
      taskType: "workflow_child",
      toolAllowlist: input.request.opts?.tools,
      workingDirectory: input.deps.workingDirectory,
      ...input.configOverrides,
    },
    {
      ...createRuntimeDeps(input.deps, input.traceContext, input.childSessionId, {
// External interaction ports can only be cast by the parent runtime: the child session is not an identity recognized by the client.
// In the past, `appOptions.providerRuntimeHeadersPort` was used directly here /
// `deps.permissionBroker`, so the actor takes `sess_dwf-...` to ask the desktop, the desktop
// `requireSession` throws an error, the response is never sent, and the subagent hangs before the first model request.
        agentId: input.childSessionId,
        agentType: input.request.opts?.agentType ?? "zcode-workflow",
        childSessionId: input.childSessionId,
        description: input.request.opts?.label ?? input.request.opts?.agentType ?? "workflow agent",
        ...(input.traceContext.turnId === undefined
          ? {}
          : { parentTurnId: input.traceContext.turnId }),
      }),
      ...(input.workflowSubmitPort ? { workflowSubmitPort: input.workflowSubmitPort } : {}),
      ...(input.workflowSubmitPort && input.workflowSubmitSchema
        ? { workflowSubmitSchema: input.workflowSubmitSchema }
        : {}),
      ...(input.workflowEscalatePort
        ? { workflowEscalatePort: input.workflowEscalatePort }
        : {}),
      ...(input.modelRequestAdmission
        ? { modelRequestAdmission: input.modelRequestAdmission }
        : {}),
    },
  );
}

function createRuntimeDeps(
  deps: ScriptWorkflowAgentRuntimeDeps,
  traceContext: TraceContext,
  childSessionId: SessionId,
  clientPortsContext: ChildClientPortsContext,
): ConstructorParameters<typeof AgentRuntime>[2] {
  return {
    agentTelemetry: deps.agentTelemetry,
    agentTelemetryCausation: deps.agentTelemetry.captureCausation(),
// Script workflow child has an independent life cycle; use Link to retain the initiation relationship.
    agentTelemetryCausationMode: "linked_root",
    appVersion: deps.appVersion,
    artifactStore: deps.artifactStore,
    contextSourcePort:
      deps.contextSourcePort ??
      deps.appOptions.contextSourcePort ??
      createNodeContextSourceAdapter({ env: deps.appOptions.env }),
// Live channel (same as subagent): The original sub-session event only notifies the external sink set of the parent runtime.
// Keep the child sessionId and let the protocol layer route according to the detached live session; the parent side will no longer append.
//So there is exactly one copy of each event in the shared store.
    //
// Must be installed during **construction period**: `ensureSessionPersistedForExternalActivity` to put SessionTitleUpdated
//Written as sequenceNumber 1, and the v4 gateway only drains consecutive seqs - subscriptions that are suspended after construction start from seq 2,
// Will wait forever for a seq 1 that never comes (this is why the old subscribeEvents channel never broadcast successfully).
    eventSink: {
      onSessionEvent: async (event) => {
        await deps.runtime.notifyExternalChildSessionEvent({
          childSessionId,
          event,
          traceContext,
        });
      },
    },
// Share the event store with the parent runtime: child events fall in the same store according to the child's own sessionId.
// v4's `loadPersistedEvents(childSessionId)` therefore hits. When you create a private memory store, it can never be read.
// transcript is permanently blank (according to `eventStore: this.eventStore` of subagent.ts).
    eventStore: deps.runtime.getSessionEventStore(),
    executionPort:
      deps.appOptions.executionPort ??
      createNodeExecutionAdapter({
        onToolExecResource: deps.appOptions.onToolExecResource,
        network: {
          httpProxy: deps.configResult.config.network.httpProxy,
          noProxy: deps.configResult.config.network.noProxy,
          caCertFile: deps.configResult.config.network.caCertFile,
        },
        outputRootDir: join(deps.storageRoot, "cli", "exec"),
        processEnv: deps.appOptions.env ?? process.env,
      }),
    fileSystemPort: deps.appOptions.fileSystemPort ?? createNodeFileSystemAdapter(),
    httpClientPort:
      deps.httpClientPort ??
      deps.appOptions.httpClientPort ??
      createNodeWebFetchHttpClientAdapter({
        env: deps.appOptions.env ?? process.env,
        proxyUrl: deps.configResult.config.network.httpProxy,
        noProxy: deps.configResult.config.network.noProxy,
        caCertFile: deps.configResult.config.network.caCertFile,
        timeoutMs: deps.configResult.config.network.timeout,
      }),
    imageProcessorPort: deps.imageProcessorPort,
    pdfDocumentPort: deps.pdfDocumentPort,
    logger: deps.logger,
    mcpPort: deps.mcpPort,
    modelFactory: deps.modelFactory,
    resolveEffectiveModelSelection: deps.appOptions.resolveEffectiveModelSelection,
// permissionBroker + providerRuntimeHeadersPort are all in here: parent runtime derived, routing identity has been rewritten to parent session.
    ...deps.runtime.createChildClientPorts(clientPortsContext),
    permissionService: deps.permissionService,
    sessionStore: deps.sessionStore,
    skillPort:
      deps.configResult.config.features.skill && deps.configResult.config.skills.enabled
        ? (deps.appOptions.skillPort ??
          createNodeSkillAdapter({
            extraRoots: deps.configResult.config.skills.roots,
// Script workflow child runtime cannot bypass user-disabled SKILL.md paths.
            disabledPaths: collectDisabledPaths(deps.configResult.config.skillOverrides),
          }))
        : undefined,
    traceContext,
  };
}
