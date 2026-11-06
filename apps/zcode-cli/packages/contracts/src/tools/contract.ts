// ============================================================
// Tool Contract - shared tool declaration surface
// ============================================================

import type { RiskLevel } from "../interfaces/session.port.js";

export type ToolSideEffectScope =
  | "none"
  | "workspace"
  | "git"
  | "network"
  | "system"
  | "session"
  | "userInteraction";

/**
* Will rewrite the side effect scope of the workspace: file system, git,
* and unproven read-only shell/host execution. `network`, `session`, `userInteraction`, `none` do not touch the workspace.
* The import cache of dynamic-workflow determines the "first write" based on this - the ability of a tool call to parse `readOnly !== true`
* and the range falls within this collection, it is a write. The decision is placed on contracts because the executor (core) that generates the tag is different from the one that consumes it.
* Driver (bootstrap) must use the same rule.
 */
export const WORKSPACE_MUTATING_SIDE_EFFECT_SCOPES: ReadonlySet<ToolSideEffectScope> = new Set<ToolSideEffectScope>([
  "workspace",
  "git",
  "system",
]);

/** Whether a tool call (by parsed capabilities) will overwrite the workspace. The scope of absence will be treated as if it were rewritten (conservative: if it is not stated, it is not credible). */
export function isWorkspaceMutatingToolCall(capability: {
  readOnly?: boolean | undefined;
  sideEffectScope?: ToolSideEffectScope | undefined;
}): boolean {
  if (capability.readOnly === true) return false;
  return capability.sideEffectScope === undefined
    ? true
    : WORKSPACE_MUTATING_SIDE_EFFECT_SCOPES.has(capability.sideEffectScope);
}

/**
* The scope of side effects that only serve the protocol itself and do not look at or touch the external world: `session` is to return the result/problem to the caller
* (dynamic-workflow's `submit_result` and `escalate` are in this file), `userInteraction` is to ask the user.
 */
const PROTOCOL_ONLY_SIDE_EFFECT_SCOPES: ReadonlySet<ToolSideEffectScope> = new Set<ToolSideEffectScope>([
  "session",
  "userInteraction",
]);

/**
* Whether a tool call **sees or touches the outside world** (read files, run commands, access the network...).
 *
* Division of work with `isWorkspaceMutatingToolCall`: that judgment "write" is used to decide when to turn off the import cache of amend-resume;
* This judgment "touch" is used to determine whether a cache entry is **pure** (pure = only answers according to the command and transcription prefix, and can still hit after closing the door).
* Reading counts as a hit: `Read` is declared in scope `none` (it produces no side effects), but its answer depends on the workspace. Therefore, the judgment is elimination method
* ——Only exclude the protocol files (`session` / `userInteraction`), and the rest will be counted as collisions; the absence of scope will also be counted as collisions.
 */
export function isWorldTouchingToolCall(capability: {
  sideEffectScope?: ToolSideEffectScope | undefined;
}): boolean {
  return capability.sideEffectScope === undefined
    ? true
    : !PROTOCOL_ONLY_SIDE_EFFECT_SCOPES.has(capability.sideEffectScope);
}

export type ToolPermissionPatternSource =
  | "none"
  | "toolName"
  | "input"
  | "path"
  | "command"
  | "network"
  | "custom";

export type ToolResultBudgetStrategy = "inline" | "truncate" | "artifact";

export type ToolExecutionMode = "client" | "providerNative";

export interface ProviderNativeToolSpec {
  kind: "provider_native";
  logicalName: string;
  providerToolName: string;
  providerIds?: string[];
  args?: Record<string, unknown>;
  fallback: "disabled";
}

export interface ToolPermissionSpec {
  permission: string;
  reason: string;
  riskLevel: RiskLevel;
  sideEffectScope: ToolSideEffectScope;
  needsApproval: boolean;
  patternSources: ToolPermissionPatternSource[];
  alwaysAllowPatternSources?: ToolPermissionPatternSource[];
  denyPriority: "beforeAsk" | "afterStaticAllow";
  /**
   * Requires explicit user approval in every permission mode. Unlike `needsApproval`, which
   * the permissive modes are allowed to short-circuit, this survives yolo's pass-through and
   * plan mode's read-only pass-through: the tool's unit of work is large enough that no
   * permissiveness setting should be able to run it unattended.
   *
   * It overrides allow-granting branches only, never deny-granting ones — an explicitly
   * disallowed tool stays denied.
   */
  alwaysAsk?: true;
  /**
   * Narrows the options offered when this tool asks. `allowAlways: false` suppresses the
   * persistent project rule: a tool whose input is different code on every call cannot
   * have a decision remembered without permanently disabling its gate. `"session"` swaps
   * the persistent rule for an in-memory, session-scoped one ("Always allow in this
   * session"): the gate stays closed for the rest of this runtime only, and every new
   * session (restart, cold resume, `/new`) asks again first.
   */
  askOptions?: { allowAlways: false | "session" };
}

export interface ToolResultBudget {
  /** Inline UI/event threshold before a result should be summarized, truncated, or redirected. */
  maxInlineBytes: number;
  /**
   * Provider-visible inline threshold. For artifact strategy this is the threshold that triggers
   * artifact persistence; the successful <persisted-output> preview uses its own preview budget.
   */
  maxModelBytes: number;
  strategy: ToolResultBudgetStrategy;
  preview?: {
    maxBytes?: number;
    maxLines?: number;
    direction?: "head" | "tail";
  };
  artifact?: {
    enabled: boolean;
    retention?: "session" | "project" | "temporary";
  };
}

export interface NoToolTimeoutPolicy {
  kind: "none";
}

export interface TimedToolTimeoutPolicy {
  kind?: "timed";
  defaultMs: number;
  maxMs?: number;
  allowCallOverride: boolean;
  /**
   * Extra wall-clock budget reserved for cancellation and adapter cleanup after the
   * tool's own timeout. The user-facing timeout still comes from defaultMs/input.
   */
  cleanupGraceMs?: number;
}

export type ToolTimeoutPolicy = NoToolTimeoutPolicy | TimedToolTimeoutPolicy;

export interface ToolCancellationPolicy {
  supported: boolean;
  cleanup: "none" | "bestEffort" | "required";
  userVisibleMessage: string;
}

export interface ToolTracePolicy {
  required: true;
  propagateToAdapters: boolean;
  recordInput: "summary" | "full" | "none";
  recordOutput: "summary" | "full" | "none";
}

export interface ToolContractDeclaration {
  capability: string;
  executionMode?: ToolExecutionMode;
  providerNative?: ProviderNativeToolSpec;
  inputSchema: Record<string, unknown>;
  outputSchema: Record<string, unknown>;
  /**
* Declare `inputSchema` **eligible** to take the provider's strict mode (Anthropic `strict: true`: constrained
* decoding ensures that tool_use.input exactly satisfies the schema). Just a qualification, not a command: adapter by provider /
* The model decides whether to actually deliver it, and is responsible for folding keywords that cannot be expressed by the strict subset into description. Absence is not strict.
* The first user is typed `submit_result` of the dwf mono subagent.
   */
  strict?: boolean;
  requiresUserInteraction?: boolean;
  permission: ToolPermissionSpec;
  resultBudget: ToolResultBudget;
  timeout: ToolTimeoutPolicy;
  cancellation: ToolCancellationPolicy;
  trace: ToolTracePolicy;
}
