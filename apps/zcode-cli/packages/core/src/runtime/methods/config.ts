import type {
  CollaborationMode,
  ExecutionShellSelection,
  Model,
  ModelSelection,
  ModelToolContract,
  PermissionBrokerRequest,
  ProjectId,
  SessionEvent,
  SessionEventSink,
  SessionEventStorePort,
  SessionId,
  SessionProjection,
  TraceContext,
  ToolExecutor,
  ToolRegistry,
  ContextBuilder,
} from "../deps.js";
import { isInspectablePermissionBroker, projectIdFromDirectory } from "../helpers/index.js";
import {
  deriveChildClientPorts,
  type ChildClientPortsContext,
  type ClientFacingPorts,
} from "../helpers/child-client-ports.js";
import type { AgentRuntimeConfig, ActiveTurnInfo } from "../types.js";
import type { AgentRuntimeInternal } from "../internal.js";
import { cloneModelSelection } from "../model-selection.js";
import { applyRuntimeExecutionState } from "../execution-state.js";

import { orderProviderVisibleToolContracts } from "../../tool/provider-visible-order.js";
import { projectToolModelContract } from "../../tool/model-contract.js";
import { rebuildContextPrefix } from "./context-refresh.js";
import { filterEmbeddedSearchRuntimeVisibleTools } from "./embedded-search-branch.js";
import {
  getSessionShellSelection as readSessionShellSelection,
  initializeSessionShellEnvironmentIfNeeded as initializeSessionShellEnvironment,
  type SessionShellEnvironmentCandidate,
} from "./session-shell-environment.js";

export async function setExecutionState(
  this: AgentRuntimeInternal,
  input: { mode?: string; planEnabled?: boolean },
  traceContext?: TraceContext,
): Promise<void> {
  await applyRuntimeExecutionState(this, input, { source: "command", traceContext });
}

export function updateConfig(
  this: AgentRuntimeInternal,
  patch: Pick<AgentRuntimeConfig, "mode" | "planEnabled" | "language" | "outputStyle">,
): void {
  if (patch.mode !== undefined || patch.planEnabled !== undefined) {
    const previous = resolveExecutionState(this.config);
    const next = resolveExecutionState(patch, previous);
    Object.assign(this.config, next);
    if (previous.planEnabled !== next.planEnabled)
      this.needsPlanModeExitReminder = !next.planEnabled;
  }
  if (patch.language !== undefined) {
    this.config.language = patch.language;
    if (!this.activeTurn) {
      rebuildContextPrefix(this);
    }
  }
  if ("outputStyle" in patch) {
    this.config.outputStyle = patch.outputStyle;
    if (!this.activeTurn) {
      rebuildContextPrefix(this);
    }
  }
}

export function initializeSessionShellEnvironmentIfNeeded(
  this: AgentRuntimeInternal,
  selection: SessionShellEnvironmentCandidate,
): boolean {
  return initializeSessionShellEnvironment(this, selection);
}

export function getSessionShellSelection(
  this: AgentRuntimeInternal,
): ExecutionShellSelection | undefined {
  return readSessionShellSelection(this);
}

export function getMode(this: AgentRuntimeInternal): CollaborationMode {
  return this.config.mode ?? "build";
}

export function getPlanEnabled(this: AgentRuntimeInternal): boolean {
  return resolveExecutionState(this.config).planEnabled;
}

export function getSessionModelSelection(this: AgentRuntimeInternal): ModelSelection | undefined {
  return this.sessionModelSelection && cloneModelSelection(this.sessionModelSelection);
}

export function setSessionModelSelection(
  this: AgentRuntimeInternal,
  selection: ModelSelection | undefined,
): void {
// Restoration/configuration refresh can clear invalid selections; unbound should not borrow the default model, nor affect the executing Active Model.
  this.sessionModelSelection = selection && cloneModelSelection(selection);
}

export function getProjectId(this: AgentRuntimeInternal): ProjectId {
// Bash cd will change the execution cwd, but the project identity cannot drift with the cwd in the tool.
  return projectIdFromDirectory(this.workspaceRoot);
}

export function setWorkingDirectory(this: AgentRuntimeInternal, cwd: string): void {
// Bash cwd persistence should only affect the current runtime session, not the workspace identity.
  this.workingDirectory = cwd;
}

export async function ensureSessionPersistedForExternalActivity(
  this: AgentRuntimeInternal,
  input: string,
  options?: { traceContext?: TraceContext },
): Promise<void> {
  await this.ensureSessionPersisted(input, options?.traceContext ?? this.rootTraceContext);
}

export function getActiveTurnInfo(this: AgentRuntimeInternal): ActiveTurnInfo | undefined {
  const activeTurn = this.activeTurn;
  if (!activeTurn) return undefined;
  return {
    kind: activeTurn.kind,
    ...(activeTurn.inputId === undefined ? {} : { inputId: activeTurn.inputId }),
    queueLength: activeTurn.pendingInputs.length,
    steerable: activeTurn.steerable,
    turnId: activeTurn.turnId,
  };
}

export function getTools(this: AgentRuntimeInternal, model?: Model): ModelToolContract[] {
  if (this.cachedTools === null) {
    this.cachedTools = filterRuntimeVisibleTools.call(this, this.registry.toContracts());
  }
  return this.cachedTools
    .filter((tool) => tool.name !== "WebSearch" || shouldExposeWebSearch.call(this, model))
    .map((tool) =>
      projectToolModelContract(tool, this.registry.get(tool.name), {
        model,
      }),
    );
}

export function invalidateToolCache(this: AgentRuntimeInternal): void {
  this.cachedTools = null;
}

export function getToolRegistry(this: AgentRuntimeInternal): ToolRegistry {
  return this.registry;
}

export function getToolExecutor(this: AgentRuntimeInternal): ToolExecutor {
  return this.executor;
}

export function subscribeEvents(this: AgentRuntimeInternal, sink: SessionEventSink): () => void {
  this.eventSinks.add(sink);
  return () => {
    this.eventSinks.delete(sink);
  };
}

/**
* Seam of external sub-runtime (1): Hand over the session event store of this runtime.
 *
* The child runtime constructed outside the class (bootstrap's dwf actor/legacy script workflow) must be consistent with the parent
* The runtime shares the same store, otherwise sub-session events only fall in a private store that no one can read, v4
* `loadPersistedEvents(childSessionId)` is always empty - the transcript is always blank. Child events are still held in their own
* The sessionId is stored in the library, and the two sessions in the same store do not cover each other (`subagent.ts:280`
* `eventStore: this.eventStore` is the same contract).
 */
export function getSessionEventStore(this: AgentRuntimeInternal): SessionEventStorePort {
  return this.eventStore;
}

/**
* Seam of external sub-runtime (2): Fan out the original events of the sub-session to the external sink set of this runtime.
 *
* The semantics are the same as `notifyEventSinks(event, {...trace, sessionId: childSessionId})` of `subagent.ts:338`
* Exactly the same: retain the sub-sessionId (the protocol layer routes it to the detached live session), only notify, not append.
 *
* The child runtime must install this call as its own `deps.eventSink` during **construction**:
* `ensureSessionPersistedForExternalActivity` writes SessionTitleUpdated as sequenceNumber 1,
* The v4 gateway only drains consecutive seqs - subscriptions that are hung after construction, starting from seq 2, will wait forever for a seq 1 that will never come again.
 */
export async function notifyExternalChildSessionEvent(
  this: AgentRuntimeInternal,
  input: { childSessionId: SessionId; event: SessionEvent; traceContext?: TraceContext },
): Promise<void> {
// Only notify, never append: the child runtime has already dropped this event according to its own sessionId.
// Using the append link of the parent runtime will cause two copies of the same event to appear in the shared store.
  await this.notifyEventSinks(input.event, {
    ...(input.traceContext ?? this.rootTraceContext),
    sessionId: input.childSessionId,
  });
}

/**
* Seam of external sub-runtime (3): Cast the **external interaction** port of the sub-runtime.
 *
* The ledger identity (child sessionId) of the sub-runtime is not an identity that the protocol client can respond to. dwf actor and legacy
* workflow child used to take `providerRuntimeHeadersPort` / `permissionBroker` directly from `appOptions`,
* So I took `sess_dwf-...` to ask the desktop; the `requireSession` on the desktop return path threw an error and the response was never sent.
* Subagent hangs permanently before first model request (8 subagents, 80 minutes without any events). Core's built-in subagent relied on
* Two private wrappers are bypassed, and three assemblies, two of which are wrong and one of which is wrong - indicate that rules will definitely drift if they are scattered at the call point.
 *
* Fix: derived convergence to `deriveChildClientPorts`, and can only be called by **parent runtime** - `parentSessionId`
* Filled in by the parent himself, the caller cannot give wrong value. Any assembly that constructs a subruntime outside of class (dwf actor, legacy
* workflow child) must get the port here.
 */
export function createChildClientPorts(
  this: AgentRuntimeInternal,
  context: ChildClientPortsContext,
): ClientFacingPorts {
  return deriveChildClientPorts(
    {
      ...(this.permissionBroker === undefined ? {} : { permissionBroker: this.permissionBroker }),
      ...(this.providerRuntimeHeadersPort === undefined
        ? {}
        : { providerRuntimeHeadersPort: this.providerRuntimeHeadersPort }),
    },
    { ...context, parentSessionId: this.sessionId },
  );
}

export function getContextBuilder(this: AgentRuntimeInternal): ContextBuilder {
  if (!this.contextBuilder) {
    this.contextBuilder = this.createContextBuilderFromSnapshot(
      this.createConfigOnlyContextSnapshot(this.workingDirectory),
      undefined,
      { persistEnvInfo: false },
    );
  }
// This getter can only provide synchronous preview builder and cannot initialize messageHistory.
// Otherwise, the first round of executeTurn will skip the asynchronous context source parsing, causing the real workspace context to be lost.
  return this.contextBuilder;
}

export function getPendingPermissionRequests(
  this: AgentRuntimeInternal,
): PermissionBrokerRequest[] {
  if (!isInspectablePermissionBroker(this.permissionBroker)) return [];
  return this.permissionBroker.listPendingRequests();
}

export async function getProjection(this: AgentRuntimeInternal): Promise<SessionProjection> {
  return this.rebuildProjection();
}

export function getSessionId(this: AgentRuntimeInternal): SessionId {
  return this.sessionId;
}

function filterRuntimeVisibleTools(
  this: AgentRuntimeInternal,
  tools: ModelToolContract[],
): ModelToolContract[] {
  const visibleTools = filterEmbeddedSearchRuntimeVisibleTools(this, tools);
// provider-visible tool order belongs to the final output boundary; toolset only determines the set of visible tools.
  return orderProviderVisibleToolContracts(visibleTools);
}

function shouldExposeWebSearch(this: AgentRuntimeInternal, model?: Model): boolean {
// Model-less calls only enumerate the complete registry for persistence and UI metadata; real execution is always passed in
// The current Active Model and only reads its frozen full capability facts.
  if (!model) return true;
  return model.properties.supportsNativeWebSearch;
}
import { resolveExecutionState } from "@zcode/shared";
