// ============================================================
// AmendWorkflow Tool Handler
// ============================================================
//
// 与 CreateWorkflow 的差别只有两处：resolveInput 解析的是**前驱 run**而不是保存的文件（省略的
// 脚本、并发上界与子代理模型都从前驱沿用，见 amend-workflow-resolve.ts；脚本的三条来路见
// amend-workflow-source.ts），handler 调的是
// `port.amend`（service 负责停在飞前驱、等结算、导入、启动）。编译、诊断路径、display 载荷、
// backgrounded 契约与 CreateWorkflow 逐字共用——模型面的 response 也照它的样子写，只多一句
// 「哪个 run 被替代了」，沿用脚本时再多说一句「脚本没变」。

import {
  AMEND_WORKFLOW_TOOL_NAME,
  AmendWorkflowInputJsonSchema,
  AmendWorkflowInputSchema,
  CreateWorkflowOutputJsonSchema,
  CreateWorkflowOutputSchema,
  type AmendWorkflowInput,
  type CreateWorkflowOutput,
  type ModelMessageContent,
  createWorkflowPhaseAlongside,
  createWorkflowPhaseNames,
} from "@zcode/contracts";
import type { ToolApprovalGate, ToolEntry, ToolHandler, ToolHandlerFailure } from "../types.js";
import { AMEND_WORKFLOW_TOOL_DESCRIPTION } from "./amend-workflow-description.js";
import {
  AMEND_WORKFLOW_ERROR_CODE,
  predecessorNotFoundFailure,
  resolveAmendWorkflowInput,
  scriptUnavailableFailure,
  validateAmendWorkflowSource,
} from "./amend-workflow-resolve.js";
import {
  DIAGNOSTICS_NOT_EXECUTED_NOTE,
  EXECUTION_UNAVAILABLE_NOTE,
  describeWorkflowConcurrencyLimit,
  resolveTraceContext,
} from "./create-workflow.js";
import { describeWorkflowSubagentModel, parseWorkflowSubagentModel } from "./model-reference.js";
import { boundGraphOfAnalysis, displayOfAnalysis } from "./workflow-analysis-display.js";
import { resolveWorkflowDraftName, writeWorkflowDraft } from "./workflow-drafts.js";
import {
  formatWorkflowDiagnosticLines,
  workflowAmendedScriptSentence,
  workflowScriptFileNote,
  type WorkflowScriptLocation,
} from "./workflow-script-notes.js";
import { analyzeScript } from "./workflow-script-analysis.js";
import { describeWorkflowScriptPath } from "./workflow-script-path.js";

const AMEND_WORKFLOW_TIMEOUT_MS = 15_000;
const AMEND_WORKFLOW_MODEL_BYTES = 24_000;

function amendUnavailableFailure(): ToolHandlerFailure {
  return {
    result: false,
    errorCode: AMEND_WORKFLOW_ERROR_CODE.AMEND_UNAVAILABLE,
    message:
      "workflow_amend_unavailable: this session cannot amend workflow runs — workflow execution is not available here. This is a capability gap, not a status problem.",
  };
}

/** 端口两种拒绝理由 → 各配可操作文案的结构化失败（判别键在 message 前缀）。 */
function amendRefusalFor(reason: string, runId: string): ToolHandlerFailure {
  switch (reason) {
    case "run_not_found":
      return predecessorNotFoundFailure(runId);
    case "missing_boundaries":
      return {
        result: false,
        errorCode: AMEND_WORKFLOW_ERROR_CODE.MISSING_BOUNDARIES,
        message: `workflow_amend_missing_boundaries: run ${runId}'s journal predates transcript-boundary bookkeeping (or its boundaries were never recorded), so its finished asks cannot seed an amended run — there is no fallback. Submit this script as a fresh CreateWorkflow instead. Nothing was stopped or created.`,
      };
    default:
      // 端口契约外的 reason：仍回结构化失败（throw 是接线故障的通道），文案带原词供日志排查。
      return {
        result: false,
        errorCode: AMEND_WORKFLOW_ERROR_CODE.MISSING_BOUNDARIES,
        message: `workflow_amend_refused: the workflow runtime refused to amend run ${runId} (reason: ${reason}). Nothing was stopped or created.`,
      };
  }
}

/**
 * 编不过时交回模型的文案。诊断行与 NOTE 照「Script files」写成文件坐标（脚本有文件时）。沿用来的
 * 脚本要先说清它是**继承
 * 来的**：模型这次调用没写这些行，光给诊断它会以为自己传错了参数——与「继承来的子代理模型已
 * 不可用」同一条理由。两种情形都补一句前驱未动：否则模型会以为它刚把一个在跑的 run 停掉了。
 */
function compileFailureResponse(
  input: AmendWorkflowInput,
  diagnostics: CreateWorkflowOutput["diagnostics"],
  location: WorkflowScriptLocation | undefined,
): string {
  const inherited = input.predecessor?.script_inherited === true;
  const note =
    location !== undefined
      ? workflowScriptFileNote(location)
      : inherited
        ? "NOTE: Nothing ran — pass a rewritten `script` that compiles."
        : DIAGNOSTICS_NOT_EXECUTED_NOTE;
  return [
    inherited
      ? `This amend kept run ${input.run_id}'s script, inherited because you omitted both \`script\` and \`path\`, and that script no longer compiles against the current workflow facade:`
      : "The revised workflow script has errors:",
    ...formatWorkflowDiagnosticLines(diagnostics, location),
    "",
    `${note} Run ${input.run_id} was not touched.`,
  ].join("\n");
}

const amendWorkflowHandler: ToolHandler = async (input, context) => {
  const parsed = AmendWorkflowInputSchema.parse(input) as AmendWorkflowInput;
  // resolveInput 恒把脚本落定进来（`path` 读成 `script`、省略的从前驱回填，或当场失败）；到这里还
  // 缺脚本，只可能是绕过归一化的调用方——同样回结构化失败，而不是把 undefined 交给编译器。
  const script = parsed.script;
  if (script === undefined) return scriptUnavailableFailure(parsed.run_id, "host");
  const cwd = context.workingDirectory;

  const analysis = analyzeScript(script);
  const { diagnostics, ok } = analysis;
  const causalityGraph = boundGraphOfAnalysis(analysis);

  // 内联修订与 `CreateWorkflow` 同一条纪律：工作副本**无论编译结果如何**都落盘，好让一段编不过的
  // 修订也有文件可改；分析排在前面只为取名（无 `name`、无前驱名时取第一个阶段名）。`path` 在场
  // 就不写——那个文件已经是工作副本。沿用的脚本两头都可能：前驱的脚本文件仍是这份字节时
  // resolveInput 已把它填进 `path`（新 run 继续记它），否则这里照「不来自文件的脚本」写一份新的。
  const inlineDraft =
    parsed.path === undefined
      ? await writeWorkflowDraft({
          cwd,
          name: resolveWorkflowDraftName(parsed.name ?? parsed.predecessor?.name, causalityGraph),
          source: script,
        })
      : undefined;
  // run 记的永远是**装着这一次脚本**的文件。脚本改了就绝不沿用前驱的路径：那个文件装的是旧
  // 脚本，记到新 run 上就是让模型下次去编辑一段已经不在跑的代码。沿用脚本时前驱的文件可以继续
  // 记，但只在它此刻的字节仍是这份脚本时（resolveKeptScriptFile 已核对过）。
  const scriptPath = parsed.path ?? inlineDraft?.path;
  const location: WorkflowScriptLocation | undefined =
    scriptPath === undefined
      ? undefined
      : {
          kind: parsed.path === undefined ? "draft" : "path",
          described: describeWorkflowScriptPath(scriptPath, cwd),
          lineOffset: parsed.script_line_offset ?? 0,
        };

  if (!ok) {
    // 编不过：什么都没停、什么都没建。
    return {
      diagnostics,
      ok,
      response: compileFailureResponse(parsed, diagnostics, location),
      ...(causalityGraph === undefined ? {} : { causalityGraph }),
    } satisfies CreateWorkflowOutput;
  }

  const port = context.dynamicWorkflowRunPort;
  if (port === undefined) {
    return {
      diagnostics,
      ok,
      response: `The revised workflow script compiled cleanly.\n\n${EXECUTION_UNAVAILABLE_NOTE}`,
      ...(causalityGraph === undefined ? {} : { causalityGraph }),
    } satisfies CreateWorkflowOutput;
  }
  if (typeof port.amend !== "function") return amendUnavailableFailure();

  const amended = await port.amend(
    {
      scriptText: script,
      cwd: context.workingDirectory,
      predecessorRunId: parsed.run_id,
      ...(parsed.name === undefined ? {} : { name: parsed.name }),
      parentSessionId: context.sessionId,
      toolCallId: context.toolCallId,
      // 新脚本的声明阶段表：修订沿用前驱的 inputId，侧栏轨道却要画新脚本的站点。「同时在跑」表的下标
      // 指向的正是这张新表，所以两者必须一起从同一张图上取。
      ...(() => {
        const phaseNames = createWorkflowPhaseNames(causalityGraph);
        if (phaseNames === undefined) return {};
        const phaseAlongside = createWorkflowPhaseAlongside(causalityGraph);
        return { phaseNames, ...(phaseAlongside === undefined ? {} : { phaseAlongside }) };
      })(),
// The three states have been normalized to "a number or none" in resolveInput; `null` here can only come from bypassing the normalization
// Caller (the port does not accept it), also read as absent.
      ...(typeof parsed.max_concurrency === "number"
        ? { maxConcurrency: parsed.max_concurrency }
        : {}),
// Same as above: the three states have been reduced to "a canonical form or none" in resolveInput, `null` can only come from bypassing here
// Normalized caller, synonymous with absent (the port does not accept it).
      ...(() => {
        const subagentModel = parseWorkflowSubagentModel(parsed.subagent_model ?? undefined);
        return subagentModel === undefined ? {} : { subagentModel };
      })(),
// This revised script file. Absence means the draft cannot be written
// Go down and the model will return to the old copy.
      ...(scriptPath === undefined ? {} : { scriptPath }),
      trace: resolveTraceContext(context),
    },
    { signal: context.abortSignal },
  );
  if (!amended.ok) return amendRefusalFor(amended.reason, parsed.run_id);

  const superseded =
    amended.supersededRunId === undefined
      ? `Run ${parsed.run_id} had already settled; its finished work is imported as cache.`
      : `Run ${amended.supersededRunId} was still running: it has been stopped and superseded, and everything it finished before the stop is imported as cache. It will not send a notification of its own.`;
// When inheriting the script, indicate "the script has not changed": the model knows from this that it is only the settings that have been changed this time, rather than going through a new script that it has not written.
  const started =
    parsed.predecessor?.script_inherited === true
      ? `The script of run ${parsed.run_id} started unchanged in the background as run ${amended.runId}.`
      : `The revised script started in the background as run ${amended.runId}.`;
  return {
    diagnostics,
    ok,
//Copywriting photo CreateWorkflow's backgrounded guide: Give the id, explain that it is still running, and the results will be returned in the form of notifications.
// Explicitly discourage default polling.
    response: `${superseded} ${started} It is still running — you will be notified with the final output when it completes. Do not wait for it or poll it with TaskOutput; continue with other work unless the user asked you to wait.${describeWorkflowConcurrencyLimit(parsed.max_concurrency ?? undefined, port.concurrencyCeiling?.())}${describeWorkflowSubagentModel(parsed.subagent_model ?? undefined)}${location === undefined ? "" : workflowAmendedScriptSentence(location)}`,
    status: "backgrounded",
    backgroundTaskId: amended.runId,
    ...(causalityGraph === undefined ? {} : { causalityGraph }),
  } satisfies CreateWorkflowOutput;
};

/**
* Confirmation window preview: the same code as CreateWorkflow (if it cannot be edited, it will be released to the handler for diagnosis). display kind
* Still `create_workflow` - there is only one implementation of diagrams, scratch pens and diagnostic cards on the UI side; "This is a revision" is determined by the tool name and input parameters
* The `run_id` / `predecessor` is spoken.
 */
function prepareAmendWorkflowApproval(input: unknown): ToolApprovalGate {
  const parsed = AmendWorkflowInputSchema.safeParse(input);
// After normalization, `script` must be present; if it is absent, someone has bypassed the life cycle and released it to the handler: it will return structural failure and the window is opened.
//There is nothing to criticize either.
  if (!parsed.success || parsed.data.script === undefined) return { gate: "proceed" };
  const analysis = analyzeScript(parsed.data.script);
  if (!analysis.ok) return { gate: "proceed" };
  const display = displayOfAnalysis(analysis, AMEND_WORKFLOW_TOOL_NAME);
  return { gate: "ask", ...(display ? { display } : {}) };
}

export const amendWorkflowToolEntry: ToolEntry = {
  capability:
    "Revise an existing dynamic-workflow run with a new script: stop it if it is still running, import its finished work, and start the revision in the background",
  metadata: {
    name: AMEND_WORKFLOW_TOOL_NAME,
    description: AMEND_WORKFLOW_TOOL_DESCRIPTION,
    readOnly: false,
    destructive: false,
    concurrentSafe: true,
    timeoutMs: AMEND_WORKFLOW_TIMEOUT_MS,
    maxOutputBytes: AMEND_WORKFLOW_MODEL_BYTES,
    sideEffectScope: "none",
    riskLevel: "low",
    needsApproval: true,
  },
  handler: amendWorkflowHandler,
// At most one revised script is given, and it is only true for model input parameters (after normalization, if `script` and `path` are present at the same time, it is a legal execution state).
  validateInput: (input) => validateAmendWorkflowSource(input),
  resolveInput: resolveAmendWorkflowInput,
  prepareApproval: prepareAmendWorkflowApproval,
  inputSchema: AmendWorkflowInputJsonSchema,
  outputSchema: CreateWorkflowOutputJsonSchema,
  runtimeInputSchema: AmendWorkflowInputSchema,
  runtimeOutputSchema: CreateWorkflowOutputSchema,
  formatModelContent: formatAmendWorkflowModelContent,
  permission: {
    permission: "createWorkflow",
    reason: "amendWorkflow.runConfirmation: user must confirm running the revised script",
    riskLevel: "low",
    sideEffectScope: "none",
    needsApproval: true,
    patternSources: ["toolName"],
    alwaysAllowPatternSources: ["toolName"],
    denyPriority: "beforeAsk",
// The same door as CreateWorkflow (alwaysAsk); this session's own run is served by the owner rule of the permission.
// always-ask is released in the branch.
    alwaysAsk: true,
    askOptions: { allowAlways: "session" },
  },
  resultBudget: {
    maxInlineBytes: AMEND_WORKFLOW_MODEL_BYTES,
    maxModelBytes: AMEND_WORKFLOW_MODEL_BYTES,
    strategy: "truncate",
    preview: { maxBytes: AMEND_WORKFLOW_MODEL_BYTES, direction: "head" },
  },
  timeout: {
    defaultMs: AMEND_WORKFLOW_TIMEOUT_MS,
    maxMs: AMEND_WORKFLOW_TIMEOUT_MS,
    allowCallOverride: false,
  },
  cancellation: {
    supported: false,
    cleanup: "none",
    userVisibleMessage: "AmendWorkflow typechecks synchronously and cannot be cancelled",
  },
  trace: {
    required: true,
    propagateToAdapters: true,
    recordInput: "summary",
    recordOutput: "summary",
  },
};

function formatAmendWorkflowModelContent(output: unknown): ModelMessageContent {
  const parsed = CreateWorkflowOutputSchema.safeParse(output);
  if (!parsed.success) return "AmendWorkflow returned an invalid result.";
  return parsed.data.response;
}
