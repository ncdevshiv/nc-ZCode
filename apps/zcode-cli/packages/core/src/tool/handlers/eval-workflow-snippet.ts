// ============================================================
// EvalWorkflowSnippet Tool Handler
// ============================================================
// Experimental channel for dynamic workflow creation: synchronously compile and run a section
// scratch-facade snippet, passed to the execution surface of bootstrap via `DynamicWorkflowSnippetPort`.
// Completely transient: no background tasks, no persistence. Confirm that the gate only falls on the snippet carrying the world.run command
// （prepareApproval）。
//
//Two paths, determined by whether the port is injected:
// 1. Port present (production wiring): compile + execute, return {ok, diagnostics, logs, response}.
// 2. Port absence (unwired host, single test): Honest business failure - never pretend to perform.

import {
  EVAL_WORKFLOW_SNIPPET_DEFAULT_TIMEOUT_MS,
  EVAL_WORKFLOW_SNIPPET_SOURCE_ERROR,
  EVAL_WORKFLOW_SNIPPET_TOOL_NAME,
  EvalWorkflowSnippetInputJsonSchema,
  EvalWorkflowSnippetInputSchema,
  EvalWorkflowSnippetOutputJsonSchema,
  EvalWorkflowSnippetOutputSchema,
  serializeWorkflowArtifact,
  type EvalWorkflowSnippetInput,
  type EvalWorkflowSnippetOutput,
  type ModelMessageContent,
  type TraceContext,
} from "@zcode/contracts";
import {
  collectDiagnostics,
  collectSites,
  collectWorldRunCommands,
  createWorkflowProgram,
  SNIPPET_FACADE_DTS,
} from "@zcode/dynamic-workflow";
import type {
  ToolApprovalGate,
  ToolEntry,
  ToolExecutionContext,
  ToolHandler,
  ToolInputResolutionResult,
  ToolInputValidationResult,
} from "../types.js";
import { EVAL_WORKFLOW_SNIPPET_TOOL_DESCRIPTION } from "./eval-workflow-snippet-description.js";
import { readWorkflowScriptFile } from "./workflow-path-source.js";
import { formatWorkflowDiagnosticLines } from "./workflow-script-notes.js";
import { describeWorkflowScriptPath } from "./workflow-script-path.js";

// Tool-level timeout is **outer layer**, not snippet's wall clock: enter the parameter timeoutMs (≤600s) to drive the harness
// Kill the child process at the point and return failure normally; leave margin for compilation and finishing here. Dual clock semantics are deliberately absent -
// The normal path always reaches the inner layer first.
const EVAL_WORKFLOW_SNIPPET_TOOL_TIMEOUT_MS = 660_000;
const EVAL_WORKFLOW_SNIPPET_MODEL_BYTES = 24_000;

const NOT_EXECUTED_NOTE = "NOTE: The snippet was NOT executed — fix the errors above and call the tool again.";
const UNAVAILABLE_NOTE =
  "NOTE: The snippet was NOT executed — snippet evaluation is not available in this session.";

const EVAL_WORKFLOW_SNIPPET_FAILURE_CODE = 400;

/**
* Choose one of the two sources, and it is only true for the ** input parameters sent by the ** model: after normalization, `code` and `path` are present at the same time, which is a legal execution state.
* (The reason is the same as `CreateWorkflow`, see create-workflow-source.ts).
 */
function validateEvalWorkflowSnippetInput(input: unknown): ToolInputValidationResult {
  const parsed = EvalWorkflowSnippetInputSchema.safeParse(input);
  if (!parsed.success) return { result: true };
  const hasCode = parsed.data.code !== undefined;
  const hasPath = parsed.data.path !== undefined;
  if (hasCode === hasPath) {
    return {
      result: false,
      errorCode: EVAL_WORKFLOW_SNIPPET_FAILURE_CODE,
      message: EVAL_WORKFLOW_SNIPPET_SOURCE_ERROR,
    };
  }
  return { result: true };
}

/**
* `path` sources are normalized to `code`. The entire file is a fragment (metadata blocks are not parsed), and the path is written in absolute form - after that, the confirmation door
* (`prepareApproval` reads `code` to find the `world.run` site) and the handler see the same string of bytes.
 */
async function resolveEvalWorkflowSnippetInput(
  input: unknown,
  cwd: string,
): Promise<ToolInputResolutionResult> {
  const parsed = EvalWorkflowSnippetInputSchema.safeParse(input);
  if (!parsed.success || parsed.data.path === undefined) return { result: true, input };
  const read = await readWorkflowScriptFile({
    cwd,
    inputPath: parsed.data.path,
    parseFrontmatter: false,
  });
  if (!read.ok) {
    return { result: false, errorCode: EVAL_WORKFLOW_SNIPPET_FAILURE_CODE, message: read.message };
  }
  return {
    result: true,
    input: {
      ...parsed.data,
      code: read.file.source,
      path: read.file.path,
    } satisfies EvalWorkflowSnippetInput,
  };
}

const evalWorkflowSnippetHandler: ToolHandler = async (input, context) => {
  const parsed = EvalWorkflowSnippetInputSchema.parse(input) as EvalWorkflowSnippetInput;
  const code = parsed.code;
  if (code === undefined) {
// Unable to reach: validateInput blocks "neither", resolveInput will read `path` as `code`.
    throw new Error("EvalWorkflowSnippet handler received input without resolved code");
  }
// If there is a file, press **file line** to report diagnosis (the fragment has no metadata block, so the offset is always 0).
  const location =
    parsed.path === undefined
      ? undefined
      : {
          kind: "path" as const,
          described: describeWorkflowScriptPath(parsed.path, context.workingDirectory),
          lineOffset: 0,
        };
  const startedAt = Date.now();

  const port = context.dynamicWorkflowSnippetPort;
  if (port === undefined) {
// Unwired host: honest downgrade (same semantics as CreateWorkflow's EXECUTION_UNAVAILABLE).
    return {
      ok: false,
      diagnostics: [],
      logs: [],
      response: UNAVAILABLE_NOTE,
      durationMs: 0,
    } satisfies EvalWorkflowSnippetOutput;
  }

  const result = await port.evalSnippet(
    {
      code,
      cwd: context.workingDirectory,
      timeoutMs: parsed.timeoutMs ?? EVAL_WORKFLOW_SNIPPET_DEFAULT_TIMEOUT_MS,
      trace: resolveTraceContext(context),
    },
    { signal: context.abortSignal },
  );
  const durationMs = Math.max(0, Date.now() - startedAt);

  if (result.kind === "diagnostics") {
    return {
      ok: false,
      diagnostics: result.diagnostics,
      logs: [],
      response: [
        "The snippet has errors:",
        ...formatWorkflowDiagnosticLines(result.diagnostics, location),
        "",
        NOT_EXECUTED_NOTE,
      ].join("\n"),
      durationMs,
    } satisfies EvalWorkflowSnippetOutput;
  }

  const logsSection = renderLogs(result.logs, result.logsTruncated);

  if (result.kind === "failed") {
    return {
      ok: false,
      diagnostics: [],
      logs: result.logs,
      response: [
        `The snippet failed (${result.error.code}): ${result.error.message}`,
        ...logsSection,
      ].join("\n"),
      durationMs,
    } satisfies EvalWorkflowSnippetOutput;
  }

// completed. The product of `undefined` is "no return value" - literally, don't invent an empty object.
  const serialized = serializeWorkflowArtifact(result.artifact);
  return {
    ok: true,
    diagnostics: [],
    logs: result.logs,
    response: [
      `The snippet completed in ${durationMs}ms.`,
      serialized === undefined ? "It returned no value." : `Return value:\n${serialized}`,
      ...logsSection,
    ].join("\n"),
    durationMs,
  } satisfies EvalWorkflowSnippetOutput;
};

/** Logs are rendered into response (formatModelContent only delivers response, and the model cannot see them outside the logs field). */
function renderLogs(logs: string[], truncated: boolean): string[] {
  if (logs.length === 0) return [];
  return [
    "",
    "Logs:",
    ...logs.map((message) => `- ${message}`),
    ...(truncated ? ["- … (logs truncated)"] : []),
  ];
}

/** The port contract requires trace to be non-optional; it is synthesized by discrete fields and processed in the same way as CreateWorkflow handler. */
function resolveTraceContext(context: ToolExecutionContext): TraceContext {
  return (
    context.traceContext ??
    ({
      traceId: context.traceId,
      spanId: context.spanId,
      parentSpanId: context.parentSpanId,
      sessionId: context.sessionId,
      turnId: context.turnId,
    } as TraceContext)
  );
}

/**
* Confirmation gate for snippet: carry only
* The snippet of the world.run site is worth interrupting the user - pure reading snippet (files/git + pure calculation) is silently released,
* Even if it cannot be compiled, it will be released (the handler returns diagnosis through the port, and it is meaningless to interrupt the confirmation of a piece of code that cannot be compiled.
* Same posture as Gate of CreateWorkflow).
 *
* v1 is **normal permissions ask** (no rich preview): the permissions for display schema are .strict() on deployed desktops,
* New payload type will cause the old desktop to discard the entire permission.requested event - confirming that the door itself disappears (similar
*Regression has been tested and reproduced). Command set users can directly read from
* Input parameter code (cmd is a compile-time literal, which is the readability that literal rules buy).
 *
* The compilation here and the compilation on the port side are two times (gate is in core, execution is in bootstrap, ts.Program is not shared across packages);
* The snippet is small in size, and the gate does not depend on the port presence at the cost of two compilations.
 */
function prepareEvalWorkflowSnippetApproval(input: unknown): ToolApprovalGate {
  const parsed = EvalWorkflowSnippetInputSchema.safeParse(input);
// After normalization, `code` must be present (`path` has been read into it); its absence means that someone has bypassed the life cycle and released it to the handler.
  if (!parsed.success || parsed.data.code === undefined) return { gate: "proceed" };
  const workflow = createWorkflowProgram(parsed.data.code, { facadeDts: SNIPPET_FACADE_DTS });
  if (collectDiagnostics(workflow.program).length > 0) return { gate: "proceed" };
  const { commands, diagnostics } = collectWorldRunCommands(workflow, collectSites(workflow));
  if (diagnostics.length > 0) return { gate: "proceed" };
  return commands.length > 0 ? { gate: "ask" } : { gate: "proceed" };
}

export const evalWorkflowSnippetToolEntry: ToolEntry = {
  capability:
    "Compile and synchronously run a small dynamic-workflow TypeScript snippet (world reads + pure logic) against the real workflow execution path, fully ephemerally",
  metadata: {
    name: EVAL_WORKFLOW_SNIPPET_TOOL_NAME,
    description: EVAL_WORKFLOW_SNIPPET_TOOL_DESCRIPTION,
// After world.run lands, the snippet can express the effect: readOnly flips over and confirms that the door is in prepareApproval
// (ask only when carrying world.run site).
    readOnly: false,
    destructive: false,
    concurrentSafe: true,
    timeoutMs: EVAL_WORKFLOW_SNIPPET_TOOL_TIMEOUT_MS,
    maxOutputBytes: EVAL_WORKFLOW_SNIPPET_MODEL_BYTES,
    sideEffectScope: "workspace",
    riskLevel: "low",
    needsApproval: true,
  },
  prepareApproval: prepareEvalWorkflowSnippetApproval,
  handler: evalWorkflowSnippetHandler,
// Select one of the two sources to be true for the model input parameters; `path` is read as `code` in normalization, confirming that the gate and handler are therefore the same shape.
  validateInput: (input) => validateEvalWorkflowSnippetInput(input),
  resolveInput: (input, context) =>
    resolveEvalWorkflowSnippetInput(input, context.workingDirectory ?? "."),
  inputSchema: EvalWorkflowSnippetInputJsonSchema,
  outputSchema: EvalWorkflowSnippetOutputJsonSchema,
  runtimeInputSchema: EvalWorkflowSnippetInputSchema,
  runtimeOutputSchema: EvalWorkflowSnippetOutputSchema,
  formatModelContent: formatEvalWorkflowSnippetModelContent,
  permission: {
    permission: "evalWorkflowSnippet",
    reason:
      "evalWorkflowSnippet.runConfirmation: snippets carrying world.run commands must be confirmed",
    riskLevel: "low",
    sideEffectScope: "workspace",
    needsApproval: true,
// Input is code text, no path body: pattern only matches by tool name.
    patternSources: ["toolName"],
    alwaysAllowPatternSources: ["toolName"],
    denyPriority: "beforeAsk",
//Normal permission posture
// alwaysAsk only belongs to CreateWorkflow - the whole workflow is load-bearing and expensive, and it's worth it above all else
// Release the branch. snippet is cheap, transient, and follows the same standard pattern/rule flow as Bash (always-allow,
// yolo takes effect as usual); prepareApproval still separates "pure reading snippet" and "cannot edit" in the default ask process.
// snippet" is cut back to proceed, and the pop-up window only lands on the snippet that actually carries the command.
  },
  resultBudget: {
    maxInlineBytes: EVAL_WORKFLOW_SNIPPET_MODEL_BYTES,
    maxModelBytes: EVAL_WORKFLOW_SNIPPET_MODEL_BYTES,
    strategy: "truncate",
    preview: {
      maxBytes: EVAL_WORKFLOW_SNIPPET_MODEL_BYTES,
      direction: "head",
    },
  },
  timeout: {
    defaultMs: EVAL_WORKFLOW_SNIPPET_TOOL_TIMEOUT_MS,
    maxMs: EVAL_WORKFLOW_SNIPPET_TOOL_TIMEOUT_MS,
    allowCallOverride: false,
  },
  cancellation: {
    supported: true,
// The abort signal goes all the way to the harness and kills the sandbox process (bestEffort: the child process may have exited in the final race state).
    cleanup: "bestEffort",
    userVisibleMessage: "Snippet evaluation cancelled",
  },
  trace: {
    required: true,
    propagateToAdapters: true,
    recordInput: "summary",
    recordOutput: "summary",
  },
};

function formatEvalWorkflowSnippetModelContent(output: unknown): ModelMessageContent {
  const parsed = EvalWorkflowSnippetOutputSchema.safeParse(output);
  if (!parsed.success) return "EvalWorkflowSnippet returned an invalid result.";
  return parsed.data.response;
}
