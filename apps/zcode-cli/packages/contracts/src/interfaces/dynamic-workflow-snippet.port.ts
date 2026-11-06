// ============================================================
// Dynamic Workflow Snippet Port - synchronous compilation execution boundary of snippet
// ============================================================
// Parallel rather than merged with {@link import("./dynamic-workflow-run.port.js").DynamicWorkflowRunPort}:
// The construction premise of the run port is durable journal (without persistence, there is no run), while snippet is completely transient.
// Only relies on two execution ports - hanging it on the run port is equivalent to making the experimental channel connected to the durability condition.

import type { TraceContext } from "../tracing/tracer.js";

/** A JSON shape of a compile diagnostic (same shape as CreateWorkflow's diagnostic; the port does not import zod schema). */
export interface DynamicWorkflowSnippetDiagnostic {
  code: number;
  column: number;
  line: number;
  message: string;
}

export interface DynamicWorkflowSnippetEvalRequest {
/** snippet source code (scratch facade vocabulary: files.*, git.*, log, pure TS). */
  code: string;
/** Execution working directory (root of sandbox processes cwd, world-read). */
  cwd: string;
/** The wall clock (ms) of the entire snippet. Clamping is at the tool level; ports are only executed by value. */
  timeoutMs: number;
  trace: TraceContext;
}

export interface DynamicWorkflowSnippetEvalOptions {
  signal?: AbortSignal;
}

/**
* The structured result of eval. The three forms are mutually exclusive:
* - failed to compile: `diagnostics` is not empty, nothing was executed;
* - Execution completed: `artifact` is the top-level return value of the script (`undefined` product means the field is absent);
* - Execution failure: `error` carries a stable error code (timeout/script error/cap rejection are all in this form).
* logs are present in both latter forms (pre-failure narrative is equally valuable).
 */
export type DynamicWorkflowSnippetEvalResult =
  | { kind: "diagnostics"; diagnostics: DynamicWorkflowSnippetDiagnostic[] }
  | { kind: "completed"; artifact?: unknown; logs: string[]; logsTruncated: boolean }
  | {
      kind: "failed";
      error: { code: string; message: string };
      logs: string[];
      logsTruncated: boolean;
    };

export interface DynamicWorkflowSnippetPort {
/** Compile once and execute synchronously to settlement (fully transient: no dwf_* lines, no background tasks). */
  evalSnippet(
    request: DynamicWorkflowSnippetEvalRequest,
    options?: DynamicWorkflowSnippetEvalOptions,
  ): Promise<DynamicWorkflowSnippetEvalResult>;
}
