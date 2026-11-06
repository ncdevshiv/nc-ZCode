// ============================================================
// Core package exports
// ============================================================

// Agent components
export * from "./agent/index.js";

// Context Builder
export * from "./context/index.js";

// Compact helpers
export * from "./compact/index.js";

// Memory paths
export { resolveProjectMemoryRoot } from "./memory/project-root.js";

// Tool components
export { ToolScheduler, defaultToolScheduler, READ_ONLY_TOOLS } from "./tool/scheduler.js";
export type { ToolSchedule, ToolScheduleItem, ToolDependency } from "./tool/scheduler.js";
export { createToolRegistry, ToolRegistry, ToolRegistryImpl } from "./tool/registry.js";
export { createToolExecutor, ToolExecutor, ToolExecutorImpl } from "./tool/executor.js";
export { builtInTools, registerBuiltInTools } from "./tool/handlers/index.js";
// The submit profile runtime guard of the dwf driver should replace the typed declaration back to the universal declaration.
export {
  createSubmitResultToolEntry,
  submitResultToolEntry,
} from "./tool/handlers/submit-result.js";
//Store/codec of saved workflows: The protocol handler of the GUI hub lives in
// bootstrap, but there can only be one copy of the parser and serializer - the symptom of the respective evolution of the write side and the read side is "the workflow just saved
//Cannot be listed", so export it from here instead of letting bootstrap copy it again.
export {
  SAVED_WORKFLOW_SENTINEL,
  findSavedWorkflowShadowing,
  listSavedWorkflows,
  moveSavedWorkflow,
  parseSavedWorkflow,
  resolveSavedWorkflow,
  saveSavedWorkflow,
  savedWorkflowExists,
  savedWorkflowFileName,
  savedWorkflowPath,
  savedWorkflowRoot,
  savedWorkflowRoots,
  serializeSavedWorkflow,
  validateWorkflowArgs,
} from "./tool/handlers/saved-workflows/index.js";
export type {
  ResolvedSavedWorkflow,
  SavedWorkflowListResult,
  SavedWorkflowMoveResult,
  SavedWorkflowParseErrorReason,
  SavedWorkflowParseResult,
  SavedWorkflowResolveFailure,
  SavedWorkflowResolveResult,
  SavedWorkflowRoot,
  SavedWorkflowRootsOptions,
  WorkflowArgsValidation,
} from "./tool/handlers/saved-workflows/index.js";
export {
  DEFAULT_BASH_MAX_TIMEOUT_MS,
  DEFAULT_BASH_TIMEOUT_MS,
  DEFAULT_BASH_TIMEOUT_POLICY,
  resolveBashTimeoutMs,
  resolveBashTimeoutPolicy,
} from "./tool/bash-timeout-policy.js";
export type { BashTimeoutPolicy } from "./tool/bash-timeout-policy.js";
export type {
  ToolMetadata,
  ToolHandler,
  ToolExecutionContext,
  ToolEntry,
  ToolExecutionResult,
  ToolResultSerialization,
  ToolBatchResult,
  ExecutableToolCall,
  ToolBatchEvent,
} from "./tool/types.js";

// Hooks
export * from "./hooks/index.js";

// MCP components
export * from "./mcp/index.js";

// Plugin conversation reference (@ Plugin capability hint)
export * from "./plugin-reference/index.js";

// Node REPL/browser-use plugin runtime primitives
export { NodeReplSession } from "./repl/node-repl-session.js";
export type {
  NodeReplCuaAppIdentity,
  NodeReplImage,
  NodeReplRequestMeta,
  NodeReplRunResult,
  NodeReplStructuredResult,
  NodeReplSessionOptions,
} from "./repl/node-repl-session.js";
export { setupBrowserRuntime } from "./browser-client/index.js";
export type { BrowserClientTransport } from "./browser-client/index.js";

// Subagent components
export * from "./subagent/index.js";

// Runtime task components
export * from "./runtime-task/index.js";

// Workflow components
export * from "./workflow/definition.js";
export * from "./workflow/expert.js";
export * from "./workflow/lifecycle.js";
export * from "./workflow/scheduler.js";

// Permission components
export {
  DenyPermissionBroker,
  ManualPermissionBroker,
  PermissionService,
  createDenyPermissionBroker,
  createManualPermissionBroker,
  defaultPermissionConfig,
} from "./permission/index.js";
export type {
  ManualPermissionBrokerOptions,
  PermissionBehavior,
  PermissionContext,
  PermissionDecisionResult,
  PermissionToolCapability,
} from "./permission/index.js";
export type { PermissionConfig } from "./permission/index.js";

// Runtime
export { AgentRuntime } from "./runtime.js";
export { createExternalTurnFaultError } from "./runtime/helpers/turn-errors.js";
export { repairPersistedRemoteSessionPaths } from "./runtime/helpers/persisted-remote-session-path-repair.js";
// A cloner that "copies a transcript into another session by value". The second consumer after fork is amend-resume of dwf
// Transcription truncation (bootstrap's workflow-actor-transcript.ts): the same action - the new session is continued with the local id,
// parentID / part inline anchors are remapped accordingly. Exporting rather than letting it be written again is a symptom of missing any remapping.
// (dangling parentID, anchor pointing to the parent session) are far from the cause.
export { cloneMessageForFork, clonePartForFork } from "./runtime/helpers/steering.js";
export type {
  ChildClientPortsContext,
  ClientFacingPorts,
} from "./runtime/helpers/child-client-ports.js";
export type {
  ActiveTurnInfo,
  AgentRuntimeConfig,
  AgentRuntimeDeps,
  ConversationRewindResult,
  ExecuteTurnOptions,
  ModelExecutionContext,
  PromptAdmissionOptions,
  PromptAdmissionReceipt,
  ProviderRuntimeHeadersPort,
  ResumeSessionOptions,
  ResumeSessionResult,
  StartSavedWorkflowRunResult,
  AmendWorkflowRunSettingsInput,
  AmendWorkflowRunSettingsResult,
  TurnResult,
  WorkspaceGenerateTextInput,
  WorkspaceGenerateTextResult,
  WorkspaceForkResult,
  WorkspaceCheckpointSummary,
  WorkspaceFileRewindApplyResult,
  WorkspaceFileRewindPreview,
  WorkspaceRewindRestoredFile,
  WorkspaceRewindResult,
  RuntimeFactory,
} from "./runtime.js";

// Output helpers
export { color, formatJson, supportsColor } from "./output.js";

// Environment helpers
export { getRuntimeInfo } from "./environment.js";
export type { RuntimeInfo } from "./environment.js";

export type {
  Logger,
  LoggerFactory,
  LogContext,
  LogEntry,
  SessionEvent,
  SessionEventSink,
} from "@zcode/contracts";
export { LogLevel, SessionEventType } from "@zcode/contracts";
