// ============================================================
// Dynamic Workflow Run Service: three startup entrances (submit / amend / resume) and "compile once"
// ============================================================
// dynamic-workflow-run-service.ts reaches the upper limit of oxlint max-lines (400 lines), put `submit` /
// `resume` The two entries together with their shared compileOnce / mintRunId are split into this file; the public side is still from
// dynamic-workflow-run-service.ts export. The two entrances have opposite directions and never share the same door (the file header invariant 1 there),
// But they share the same registration form, the same parking list and the same settlement bookkeeping - these three things
// {@link DynamicWorkflowRunEntryContext} is passed in explicitly from service. This file does not hold any state of its own.

import { createHash, randomUUID } from "node:crypto";
import type {
  DynamicWorkflowRunAmendRequest,
  DynamicWorkflowRunAmendResult,
  DynamicWorkflowRunResumeResult,
  DynamicWorkflowRunSubmitRequest,
  DynamicWorkflowRunSubmitResult,
  TraceContext,
} from "@zcode/contracts";
import {
  buildAskSpecs,
  collectDiagnostics,
  collectSites,
  collectWorldRunCommands,
  createWorkflowProgram,
  deriveActorSubmitProfilesFor,
  lowerWorkflow,
  synthesizeAskSchemas,
  type Caps,
  type CompileDiagnostic,
  type ImportedRunCache,
  type RunSettlement,
  type WorkflowProgram,
} from "@zcode/dynamic-workflow";
import { formatModelPickerValue } from "@zcode/shared/model-selection";
import type { ModelSelection } from "@zcode/shared/model-selection";
import {
  buildImportedCache,
  preflightAmendImport,
  rebuildImportedCacheForResume,
} from "./dynamic-workflow-import.js";
import {
  launchDynamicWorkflowRun,
  type CompiledDynamicWorkflowScript,
} from "./dynamic-workflow-run-launch.js";
import {
  readRunLaunchAnchor,
  readRunScriptPath,
  readRunSubagentModel,
  resolveLaunchAnchor,
  type RunLaunch,
} from "./dynamic-workflow-run-launch-anchor.js";
import { isResumableRecord, type RunRegistryEntry } from "./dynamic-workflow-run-observation.js";
import type { DynamicWorkflowRunServiceDeps } from "./dynamic-workflow-run-service.js";
import type { WorkflowEscalationRegistry } from "./workflow-escalation-registry.js";

/**
* The internal state of the service used by the two entries. They are all **references** rather than copies: the registry and docking table are the copies of the service,
* `trackSettlement` is the settlement bookkeeping of the service (final state entry, failure normalization, settlement notification), `caps` is submit
* The upper bound calculated by current concurrency detection on the path (resume follows the caps in the journal record without passing it).
 */
export interface DynamicWorkflowRunEntryContext {
  deps: DynamicWorkflowRunServiceDeps;
  runs: Map<string, RunRegistryEntry>;
  escalations: WorkflowEscalationRegistry;
  /**
* Caps for this launch. The input parameter is the requested concurrency upper bound (`CreateWorkflow` / `AmendWorkflow`
* `max_concurrency`, has been normalized to a number by the tool layer or absent): absent is the ceiling, if given, it is clamped to [1, ceiling].
   */
  caps: (requestedMaxConcurrency?: number) => Caps;
  trackSettlement: (
    runId: string,
    entry: RunRegistryEntry,
    launched: Promise<RunSettlement>,
  ) => Promise<RunSettlement>;
}

/** The implementation of `DynamicWorkflowRunPort.submit`: a new run, no precursor, no cache. */
export async function submitDynamicWorkflowRun(
  ctx: DynamicWorkflowRunEntryContext,
  request: DynamicWorkflowRunSubmitRequest,
): Promise<DynamicWorkflowRunSubmitResult> {
  const runId = startNewRun(ctx, {
    runId: mintRunId(),
    scriptText: request.scriptText,
    cwd: request.cwd,
    ...(request.name === undefined ? {} : { name: request.name }),
    ...(request.args === undefined ? {} : { args: request.args }),
    ...(request.parentSessionId === undefined ? {} : { parentSessionId: request.parentSessionId }),
    ...(request.toolCallId === undefined ? {} : { toolCallId: request.toolCallId }),
    ...(request.launchInputId === undefined ? {} : { launchInputId: request.launchInputId }),
    ...(request.phaseNames === undefined ? {} : { phaseNames: request.phaseNames }),
    ...(request.maxConcurrency === undefined ? {} : { maxConcurrency: request.maxConcurrency }),
    ...(request.subagentModel === undefined ? {} : { subagentModel: request.subagentModel }),
// The script file is in the same vehicle as the subagent model: download it as it is.
// The port does not make any inferences - whether a draft was written or not is a fact on the tool side, and absence means there is really no document.
    ...(request.scriptPath === undefined ? {} : { scriptPath: request.scriptPath }),
    ...(request.phaseAlongside === undefined ? {} : { phaseAlongside: request.phaseAlongside }),
    trace: request.trace,
  });
  return { ok: true, runId };
}

/**
* Implementation of `DynamicWorkflowRunPort.amend`.
 *
* Order is all semantics:
* 1. **Pre-check** Precursor (existence ∧ boundary complete) - nothing is moved when rejected, the precursor is still running;
* 2. Create new id;
* 3. If the precursor is flying, it cancels it with `{ superseded: newRunId }` and **awaits its own settlement promise** - not a timeout,
* Not polling the journal; the predecessor therefore resolves to `stopped(superseded, supersededBy)`;
* 4. Build the cache from the settled predecessor - the preflight has passed, and any rejection here may only be caused by a host failure, so throw it up;
* 5. Started in the same launch path as the new submit, with `resumedFrom` and cache.
 *
* The old version made `submit({resumeFrom})` return `not_amendable` on the fly, and the model could only be polled by TaskStop →
* stopped → Three steps of resubmission; this method puts stopping and settlement waiting into service, and the race condition disappears.
 */
export async function amendDynamicWorkflowRun(
  ctx: DynamicWorkflowRunEntryContext,
  request: DynamicWorkflowRunAmendRequest,
): Promise<DynamicWorkflowRunAmendResult> {
  const { deps, runs } = ctx;
  const preflight = preflightAmendImport(deps.journal, request.predecessorRunId);
  if (!preflight.ok) {
    deps.logger?.info?.("Dynamic workflow amend refused", {
      event: "dynamic_workflow.amend.refused",
      module: "bootstrap.app",
      predecessorRunId: request.predecessorRunId,
      reason: preflight.reason,
    });
    return { ok: false, reason: preflight.reason };
  }
  const predecessor = preflight.run;
  const runId = mintRunId();

// In flying judgment, look at the **registry of this process** instead of the journal status: journal says running but it is not in the registry. It is something else.
// When the process (or dead process) runs, this service cannot be stopped or waited for - it will be processed as settled and let the final state gate built by the import speak.
// (Throw if not final, see below).
  const live = runs.get(request.predecessorRunId);
  let supersededRunId: string | undefined;
  if (live !== undefined && live.terminal === undefined) {
    live.controller.abort({ superseded: runId });
// Wait for the predecessor's own settlement promise (the same promise as waitForTask; trackSettlement ensures that it will never reject).
    await live.settlement;
    supersededRunId = request.predecessorRunId;
    deps.logger?.info?.("Dynamic workflow run superseded by an amendment", {
      event: "dynamic_workflow.amend.superseded",
      module: "bootstrap.app",
      predecessorRunId: request.predecessorRunId,
      runId,
    });
  }

  const built = await buildImportedCache(deps, request.predecessorRunId);
  if (!built.ok) {
    // 预检刚通过：run 存在、边界齐全；到这里还被拒只剩「前驱非终态」——注册表说它不在飞而
    // journal 说它还在跑（别的进程持有）。这不是模型能改的输入，按接线故障上抛。
    throw new Error(
      `dynamic workflow amend could not import from run ${request.predecessorRunId} after preflight: ${built.reason}`,
    );
  }

  // 用量起点：**前驱结算之后**再读一次行。preflight 那一份是停止之前的快照，用它会漏掉前驱最后几轮的
  // 花费——修订一个在飞 run 恰好是这条路最常见的用法。读不到行按 0 处理（前驱刚被清理）。
  const settledPredecessor = deps.journal.getRun(request.predecessorRunId);
  const inheritedTokens = settledPredecessor?.spentTokens ?? 0;
  // 实参只在调用方明说时沿用（GUI「配置」重跑的是前驱自己的脚本，见端口字段注释）；工具路径
  // 不传，修订照旧不带实参。前驱没有实参（内联脚本 / 老行）时整个字段缺席。
  const inheritedArgs =
    request.inheritArgs === true && settledPredecessor?.args !== undefined
      ? settledPredecessor.args
      : undefined;

  startNewRun(ctx, {
    runId,
    scriptText: request.scriptText,
    cwd: request.cwd,
    inheritedTokens,
    // 展示名沿用前驱：修订是同一件工作的下一版，卡片与通知里换个名字只会让用户以为是另一条工作流。
    ...(request.name !== undefined
      ? { name: request.name }
      : predecessor.name === undefined
        ? {}
        : { name: predecessor.name }),
    ...(inheritedArgs === undefined ? {} : { args: inheritedArgs }),
    ...(request.parentSessionId === undefined ? {} : { parentSessionId: request.parentSessionId }),
    ...(request.toolCallId === undefined ? {} : { toolCallId: request.toolCallId }),
    ...(request.phaseNames === undefined ? {} : { phaseNames: request.phaseNames }),
    // 并发上界不从前驱继承：「省略即沿用前驱」是**工具面**的三态，`AmendWorkflow` 的
    // resolveInput 已经把它归一成这里的一个数或缺席（缺席 = 天花板）。端口若再继承一次，
    // 「解除限制」（`null`）就永远到不了这里。
    ...(request.maxConcurrency === undefined ? {} : { maxConcurrency: request.maxConcurrency }),
    // 子代理模型同样不从前驱继承，与上面的并发上界同一条论证：「省略即沿用前驱」是**工具面**
    // 的三态，`AmendWorkflow` 的 resolveInput 已经把它归一成这里的一条选择或缺席（缺席 = 回到
    // 会话模型）。端口若再继承一次，「回到会话模型」（`null`）就永远到不了这里。
    ...(request.subagentModel === undefined ? {} : { subagentModel: request.subagentModel }),
    // 脚本文件**绝不从前驱继承**：修订记的是这一次修订的脚本来自哪个文件（`path` 提交就是
    // 那个文件，内联提交就是刚写下的草稿）。沿用前驱的路径等于让模型下次去编辑旧脚本。
    ...(request.scriptPath === undefined ? {} : { scriptPath: request.scriptPath }),
    ...(request.phaseAlongside === undefined ? {} : { phaseAlongside: request.phaseAlongside }),
    trace: request.trace,
    imported: { cache: built.cache, resumedFrom: built.resumedFrom },
  });
  return { ok: true, runId, ...(supersededRunId === undefined ? {} : { supersededRunId }) };
}

interface StartNewRunInput {
  runId: string;
  scriptText: string;
  cwd: string;
  name?: string;
  args?: Record<string, unknown>;
  parentSessionId?: string;
  toolCallId?: string;
  launchInputId?: string;
  /** 脚本声明的阶段表（submit 与 amend 都传：修订用**新脚本**的阶段表）。 */
  phaseNames?: string[];
  /** 请求的并发上界；缺席即天花板。钳制在 {@link DynamicWorkflowRunEntryContext.caps} 里。 */
  maxConcurrency?: number;
  /**
   * 本 run 的子代理模型。缺席即子代理跑在会话模型上。
   * 结构化选择进来，落库前归一成 picker 字符串——见 startNewRun 里的注释。
   */
  subagentModel?: ModelSelection;
  /**
   * 本 run 脚本文件的绝对路径。缺席即这个
   * run 没有可编辑的脚本文件。纯模型面元数据：不参与执行，也不参与 resume 校验。
   */
  scriptPath?: string;
  /** 与 `phaseNames` 对齐的「同时在跑」表；下标指向的就是上面这张表，两者同来同走。 */
  phaseAlongside?: number[][];
  trace: TraceContext;
  /**
   * 本 run 的用量起点（前驱结算后的 `spentTokens`）。amend 路径给出，全新 submit 缺席（= 0）。
   */
  inheritedTokens?: number;
/** amend path: lineage pointer paired with import cache. */
  imported?: { cache: ImportedRunCache; resumedFrom: string };
}

/**
* The launch tail shared by submit and amend: compile → anchor → registry entry → fire-and-forget launch.
* Return runId (synchronous: the registry entry already exists before this function returns, see note below).
 */
function startNewRun(ctx: DynamicWorkflowRunEntryContext, input: StartNewRunInput): string {
  const { deps, runs, escalations } = ctx;
  const { imported, runId } = input;
  const compiled = compileOnce(input.scriptText);
// The clamped upper bound only counts **once**: it must both fall with EngineConfig dwf_run.caps_max_concurrency and be used as
// Gap copy of the registry entry (the only place getTask / getRunDetail can read before the journal line appears).
// Counting twice is equivalent to having the two reading surfaces give different numbers at the moment the ceiling changes.
  const caps = ctx.caps(input.maxConcurrency);
// Standard string form (`providerId/modelId[$reasoningLevel]`). The port accepts structured selection, and the journal
//The event, two reading surfaces and the progress load all require a string - normalized once here and transported throughout the downstream process.
  const subagentModel =
    input.subagentModel === undefined ? undefined : formatModelPickerValue(input.subagentModel);

// Initiate anchor point: use predecessor for revision, explicit value for direct start, activity wheel for chat,
// If you don't have any, just cast one. When the engine is being run, remember it as run-launched.
// The stage table declared by the script is merged side by side with the subagent model and anchor point of this run: the revised continuation uses the inputId of the predecessor,
// However, the stage table of **new script** is used, and the model of the predecessor is never inherited, so neither of them enter resolveLaunchAnchor.
  const launch: RunLaunch = {
    ...resolveLaunchAnchor({
      ...(input.launchInputId === undefined ? {} : { requested: input.launchInputId }),
      trace: input.trace,
      ...(deps.resolveLaunchInputId === undefined
        ? {}
        : { resolveLaunchInputId: deps.resolveLaunchInputId }),
      ...(imported === undefined
        ? {}
        : (() => {
            const predecessor = readRunLaunchAnchor(deps.journal, imported.resumedFrom);
            return predecessor === undefined ? {} : { predecessor };
          })()),
    }),
    ...(input.phaseNames === undefined ? {} : { phaseNames: input.phaseNames }),
// The subagent model is in the same car as the stage table (also does not enter resolveLaunchAnchor: the revision never inherits the predecessor's model).
// Zero SQL - it lives in this event, there is no corresponding column on `dwf_run` (no migration is done on purpose).
    ...(subagentModel === undefined ? {} : { subagentModel }),
// The script file is in the same car as the subagent model (also not included in resolveLaunchAnchor: the revision records the file of the new script).
// Zero SQL - it lives in this event, there is no corresponding column on `dwf_run`.
    ...(input.scriptPath === undefined ? {} : { scriptPath: input.scriptPath }),
    ...(input.phaseAlongside === undefined ? {} : { phaseAlongside: input.phaseAlongside }),
  };

// Registration must precede startup: cancellation may arrive at any time after submit returns, and background trackers may
// Start polling the snapshot immediately - the registry is the only synchronized fact that "run exists" (journal's dwf_run line
// Wait for the engine to be constructed, a few microtasks later).
  const controller = new AbortController();
  const entry: RunRegistryEntry = {
    controller,
    startedAt: new Date(),
    ...(input.toolCallId === undefined ? {} : { toolCallId: input.toolCallId }),
    ...(input.parentSessionId === undefined ? {} : { parentSessionId: input.parentSessionId }),
// The only item key and tag source that the enumeration side can read before the journal line appears (see RunRegistryEntry).
    cwd: input.cwd,
    ...(input.name === undefined ? {} : { name: input.name }),
    scriptText: input.scriptText,
// The effective concurrency upper bound (= the share that falls into the library). Same gap argument: resolveInput of `AmendWorkflow` reads
// getTask determines "what to use", and revising a run that has just started will fall into this gap.
    maxConcurrency: caps.maxConcurrency,
// Gap copy of the subagent model, consistent with the concurrency upper bound (see RunRegistryEntry.subagentModel). Here it is
// The **only** thing about "structured selection → canonical string" is that it is recorded as `run-launched`, and the two reads are the same as
// `run-started` is what the payload reads, so the format cannot be forked between three places.
    ...(subagentModel === undefined ? {} : { subagentModel }),
// Gap copy of script file, same as subagent model (see RunRegistryEntry.scriptPath).
    ...(input.scriptPath === undefined ? {} : { scriptPath: input.scriptPath }),
    ...(imported === undefined ? {} : { resumedFrom: imported.resumedFrom }),
// The gap copy of the usage starting point is the same as the concurrency upper bound: before the journal line falls, the two reading surfaces can only read the usage from the entry.
// And revising a just-started run happens to fall into those microtasks - reporting 0 will cause the details to say "This lineage costs nothing."
    ...(input.inheritedTokens === undefined ? {} : { inheritedTokens: input.inheritedTokens }),
// The real settlement promise is replaced below; place space first to satisfy the type (sync visible).
    settlement: Promise.resolve<RunSettlement>({ status: "stopped", reason: "user" }),
  };
  runs.set(runId, entry);

// fire-and-forget: Never await settlement in submit. The contract of submit is "start and hand over runId".
  entry.settlement = ctx.trackSettlement(
    runId,
    entry,
    launchDynamicWorkflowRun({
      caps,
      compiled,
      cwd: input.cwd,
      deps,
// name is in the same metadata path as scriptText / cwd: launch → harness is transferred to EngineConfig as it is,
// The engine drops dwf_run.name on createRun. The service side deliberately does not do the second UPDATE - then
// Create the second writer of dwf_run (same argument for file header invariant 1).
      ...(entry.name === undefined ? {} : { name: entry.name }),
//The actual parameters follow the same metadata path: launch → harness → EngineConfig (fall args_json) + spawn
// payload (args global injected into the sandbox). Inline scripts have no arguments and fields are completely absent.
      ...(input.args === undefined ? {} : { args: input.args }),
      ...(entry.parentSessionId === undefined ? {} : { parentSessionId: entry.parentSessionId }),
      escalationRegistry: escalations,
      runId,
      scriptText: input.scriptText,
      signal: controller.signal,
      ...(entry.toolCallId === undefined ? {} : { toolCallId: entry.toolCallId }),
// The lineage pointer and cache table are downloaded in pairs (launch → harness → EngineConfig): the former falls into dwf_run
// resumed_from (createRun is written once), the latter only lives in this execution.
      ...(imported === undefined
        ? {}
        : { importedCache: imported.cache, resumedFrom: imported.resumedFrom }),
// The starting point of usage goes the same way (launch → harness → EngineConfig): the engine writes the initial value of spent_tokens according to it.
      ...(input.inheritedTokens === undefined ? {} : { inheritedTokens: input.inheritedTokens }),
// The subagent model doesn't just go all the way: it's in launch (see RunLaunch above), along with a `run-launched`
// After entering the journal, the launch side will take it out from the same object and parse it into a selection and hand it to the actor runtime factory.
      launch,
    }),
  );

  return runId;
}

/** Implementation of `DynamicWorkflowRunPort.resume`. */
export async function resumeDynamicWorkflowRun(
  ctx: DynamicWorkflowRunEntryContext,
  runId: string,
): Promise<DynamicWorkflowRunResumeResult> {
  const { deps, runs, escalations } = ctx;
  const record = deps.journal.getRun(runId);
  if (record === undefined) return { ok: false, reason: "not_found" };
// in-process is running at the same time as runId: journal status is running, which is not recoverable; single column reason is
// Because it and "irrecoverable final state" are different actions for the caller (wait for it to resolve vs give up).
  const live = runs.get(runId);
  if (live !== undefined && live.terminal === undefined) {
    return { ok: false, reason: "already_running" };
  }
// The revised run single-column reason: "Unrecoverable final state" is a different action for the caller (see follow-up vs abandonment).
  if (record.status === "stopped" && record.stopReason === "superseded") {
    return { ok: false, reason: "superseded" };
  }
  if (!isResumableRecord(record)) return { ok: false, reason: "not_resumable" };
// scriptText is dropped into the library starting from workflow-live-run; earlier records have no rerunable scripts.
  if (record.scriptText === undefined) return { ok: false, reason: "script_missing" };

// The original text of the journal of the old run was written according to the facade at that time. After reconstruction,
// May no longer pass type checking. compileOnce is a hard fail for dirty scripts (that's a miswired channel), resume hits it but
// A business branch that users can expect - structured rejection + bounded diagnosis, UI / TUI / tool surface points to AmendWorkflow accordingly,
// Instead of a general "execution failure". Diagnosis and subsequent compilation share the same Program and are still "compile once".
  const workflow = createWorkflowProgram(record.scriptText);
  const diagnostics = collectDiagnostics(workflow.program);
  if (diagnostics.length > 0) {
    deps.logger?.warn?.("Dynamic workflow resume refused: stored script no longer compiles", {
      diagnosticCount: diagnostics.length,
      event: "dynamic_workflow.resume.compile_failed",
      module: "bootstrap.app",
      runId,
    });
    return {
      ok: false,
      reason: "compile_failed",
      message: boundedResumeDiagnostics(runId, diagnostics),
    };
  }
  const compiled = compileProgram(record.scriptText, workflow);
// Server-side priori (the engine is still verified twice during construction, defense depth): the record itself is overwritten by an external force (scriptText and
// scriptHash is no longer self-consistent), the registry entry will not be created and will not be started - a synchronization error on the engine side will record the entry as
// failed final state, covering up the fact that "the record can still be repaired".
  if (record.scriptHash !== undefined && record.scriptHash !== compiled.scriptHash) {
    deps.logger?.warn?.("Dynamic workflow resume refused: script hash mismatch", {
      event: "dynamic_workflow.resume.script_hash_mismatch",
      expected: record.scriptHash,
      got: compiled.scriptHash,
      module: "bootstrap.app",
      runId,
    });
    return { ok: false, reason: "script_mismatch" };
  }

// Revised resume of run: rebuild the import cache.
// Consumed hits are true in the journal of this run, and replay is short-circuited as usual; unconsumed imports only live in memory.
// If it is not rebuilt, it will become a live rerun in this continuation run. Reconstruction and commit-time construction are the same pure function that reads the same precursor.
// journal, so the certainty is established - the engine re-pushes the divergence point according to the record line of this run (imported-cache.ts
// reconcileRecorded), so the two sides will not be misaligned.
  //
// **Reconstruction failure will never bring down resume**: The precursor journal is the survival dependency of the revised run, but it is only an acceleration structure——
// It becomes expensive if you lose it, but it’s not wrong. When the precursor is cleared/the boundary gate is no longer passed, it will start without cache as usual, and unconsumed imports will degrade into
// Live re-executes, while run's own journal lines are still replayed one by one.
  const rebuilt =
    record.resumedFrom === undefined
      ? undefined
      : await rebuildImportedCacheForResume(deps, {
          predecessorRunId: record.resumedFrom,
          runId,
        });

// Replace registry entries: same runId, new AbortController, new settlement promise - cancel will be available from now on.
// Must precede launch (header invariant 5: entry is a prerequisite for watcher).
  const resumedSubagentModel = readRunSubagentModel(deps.journal, runId);
  const resumedScriptPath = readRunScriptPath(deps.journal, runId);
  const controller = new AbortController();
  const entry: RunRegistryEntry = {
    controller,
    startedAt: new Date(),
    ...(record.toolCallId === undefined ? {} : { toolCallId: record.toolCallId }),
    ...(record.parentSessionId === undefined ? {} : { parentSessionId: record.parentSessionId }),
// Gap metadata for the enumeration side (see RunRegistryEntry): the resume authority is in the journal record,
// This is just a memory copy of the same fact. cwd has the same origin as the value of launch below.
    cwd: record.cwd ?? process.cwd(),
    ...(record.name === undefined ? {} : { name: record.name }),
    scriptText: record.scriptText,
// Subagent model: read the event header once and copy it into the entry (see RunRegistryEntry.subagentModel). It’s worth building and running that life
// Just write it on `run-launched`, and the run will remain unchanged for the rest of its life, so copying it will not branch from the event; after copying it, there will be two readings
// There is only one rule left - read the entries if there are entries, and scan the events only if there are cold entries.
    ...(resumedSubagentModel === undefined ? {} : { subagentModel: resumedSubagentModel }),
// Script file: The same reading and the same argument as the sub-agent model (build run and write it to death in that life, and it will remain unchanged in the rest of your life. It will not work if you copy it.
// fork with event). After resume, the two reading pages are still "there are entries to read entries".
    ...(resumedScriptPath === undefined ? {} : { scriptPath: resumedScriptPath }),
    settlement: Promise.resolve<RunSettlement>({ status: "stopped", reason: "user" }),
  };
  runs.set(runId, entry);

  entry.settlement = ctx.trackSettlement(
    runId,
    entry,
    launchDynamicWorkflowRun({
// caps follow the journal record: spentTokens are accumulated against this set of caps.
// Recalculation is like quietly moving the goalposts.
      caps: record.caps,
      compiled,
      cwd: record.cwd ?? process.cwd(),
      deps,
      ...(record.name === undefined ? {} : { name: record.name }),
// resume **Replay** the saved actual parameters, never accept new ones: the identity of a run includes its actual parameters,
// This is the same rule as "only valid for byte-identical scripts". The resume entry deliberately has no actual parameters.
// So the only source here is the journal record; there are no old rows for this column (absent → sandbox
// pronounced as `{}`). Changing actual parameters = a new run = a new confirmation window.
      ...(record.args === undefined ? {} : { args: record.args }),
// The sub-agent model is not downloaded either: it lives with the anchor point `run-launched`, and the run is written to death in the launch side.
//Read back from the event header of the journal yourself (the entry copied above is the same read). Same discipline as caps / args
// ——The identity of a run includes which model it is run on. Resume replays the saved copy and never accepts new ones.
      ...(record.parentSessionId === undefined ? {} : { parentSessionId: record.parentSessionId }),
// Both entries share the same docking table (see field comments above).
      escalationRegistry: escalations,
      runId,
      scriptText: record.scriptText,
      signal: controller.signal,
      ...(record.toolCallId === undefined ? {} : { toolCallId: record.toolCallId }),
// resumedFrom will no longer be downloaded: createRun has been written to death as early as submission, and the engine hits the existing file on the resume path.
// OK, don't run createRun at all. Only the rebuilt cache needs to be deleted.
      ...(rebuilt === undefined ? {} : { importedCache: rebuilt }),
    }),
  );

  return {
    ok: true,
    runId,
    ...(record.toolCallId === undefined ? {} : { toolCallId: record.toolCallId }),
  };
}

/**
* Compile once: a ts.Program simultaneously feeds site tables, schema synthesis and lowering.
 *
* The dirty script fails hard here and **does not build run**: the handler only calls submit when `ok`, so the dirty script goes here
* It can only be a wiring error. The defensive check reads the program diagnosis of the same compilation and does not start a second Program
* (That would break "compile once"). resume uses the same function to reorganize the original text in the journal - byte-identical
* The script must re-pass the same set of checks.
 */
function compileOnce(scriptText: string): CompiledDynamicWorkflowScript {
  return compileProgram(scriptText, createWorkflowProgram(scriptText));
}

/** resume rejects the upper limit of diagnosis in the copy (the same magnitude as compile_failed started directly by the hub). */
const RESUME_DIAGNOSTICS_MAX_CHARS = 2000;

/** Human-readable diagnosis of compile_failed: one `L:C message` per line, bounded overall. */
function boundedResumeDiagnostics(runId: string, diagnostics: CompileDiagnostic[]): string {
  const body = [
    `The stored script of run ${runId} no longer compiles against the current workflow facade:`,
    ...diagnostics.map(
      (diagnostic) => `L${diagnostic.line}:C${diagnostic.column} ${diagnostic.message}`,
    ),
  ].join("\n");
  return body.length > RESUME_DIAGNOSTICS_MAX_CHARS
    ? `${body.slice(0, RESUME_DIAGNOSTICS_MAX_CHARS - 1)}…`
    : body;
}

/**
* The second half of compileOnce: do site table / schema synthesis / lowering for the **already built** Program. resume first use the same
* If Program retrieves the diagnosis and submits it here, it will still be "compiled once" (Program caches its own diagnosis and does not recalculate it after rereading).
 */
function compileProgram(
  scriptText: string,
  workflow: WorkflowProgram,
): CompiledDynamicWorkflowScript {
  const diagnostics = [
    ...workflow.program.getSyntacticDiagnostics(),
    ...workflow.program.getSemanticDiagnostics(),
  ];
  if (diagnostics.length > 0) {
    throw new Error(
      `dynamic workflow submit received a script that does not typecheck (${diagnostics.length} diagnostics); no run was created`,
    );
  }

  const table = collectSites(workflow);
  const { diagnostics: schemaDiagnostics, schemas } = synthesizeAskSchemas(workflow, table);
  if (schemaDiagnostics.length > 0) {
    throw new Error(
      `dynamic workflow submit received a script with unsupported ask result types: ${schemaDiagnostics
        .map((diagnostic) => `L${diagnostic.line}:C${diagnostic.column} ${diagnostic.message}`)
        .join("; ")}`,
    );
  }
// The command set of world.run is collected in the same compilation (authorization surface: compile-time literal + confirmation window display + driver review).
// The non-literal cmd has been blocked in the analyze stage of the handler; here, a wiring error still occurs, and the run is not executed due to hard failure.
  const worldRun = collectWorldRunCommands(workflow, table);
  if (worldRun.diagnostics.length > 0) {
    throw new Error(
      `dynamic workflow submit received a script with non-literal world.run commands (${worldRun.diagnostics.length} diagnostics); no run was created`,
    );
  }

// buildAskSpecs is the only correct construct for askSpecs: untyped site explicit notation {typed:false}.
// Constructing with schemas keys will make the untyped site completely absent, and the engine will treat the absence as a wiring error and hard fail.
  const askSpecs = buildAskSpecs(table, schemas);

  return {
    askSpecs,
// submit profile for each actor site: in **the same**
//Explanation on Program + site map projection (analyzeWorkflowScript has already analyzed the same text in the analyze phase of the handler
// After running these two steps), it is still "compile once". resume recalculates byte-identical text using the same function, deterministically.
    actorSubmitProfiles: deriveActorSubmitProfilesFor(workflow, table, askSpecs),
    declaredRunCommands: new Set(worldRun.commands),
    lowered: lowerWorkflow(workflow, table).code,
// The ownership of scriptHash is **here**, not in the harness. harness collects scriptText and lowered at the same time,
// And deliberately not verify whether the two are self-consistent - verification is equivalent to running the compilation again, which is what "compile once" should omit.
// (harness.ts writes this as an invariant for the caller). So the hash must be calculated on the author's original text:
// If the harness hashes "the text it sees", the lowered path will be the hash of the lowered function body.
// And resume compares the author's original text - the comparison object will be silently misaligned. This function is produced simultaneously from the same compilation
// lowered and hash are structurally consistent.
    scriptHash: createHash("sha256").update(scriptText, "utf8").digest("hex"),
  };
}

/**
* run id. The character set must be safe: it will access actor session ids, URLs, file paths and logs, so just use
* `[A-Za-z0-9-]` (the output of randomUUID is this character set), never `#`/`@`/`/`.
 */
function mintRunId(): string {
  return `dwfrun-${randomUUID()}`;
}
