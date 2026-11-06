/* eslint-disable max-lines -- Dynamic workflow run port keeps submission/observation/cancel types together for protocol alignment. */
// ============================================================
// Dynamic Workflow Run Port - dwf engine run submission/observation/cancel boundary
// ============================================================

import type {
  DynamicWorkflowRunHealth,
  DynamicWorkflowRunPhaseView,
  DynamicWorkflowRunSubagentView,
} from "./dynamic-workflow-run-roster.port.js";
import type { DynamicWorkflowRunProgressPayload } from "../events/session.events.js";
import type { ModelSelection } from "../model/model.js";
import type { SessionId, ToolCallId } from "./shared.js";
import type { TraceContext } from "../tracing/tracer.js";
import type { WorkflowTaskSnapshot } from "./workflow.port.js";

/**
* A submission request for a workflow run. The script text is the authoritative input (compilation, site table, schema synthesis, lowering
* are derived from it), so only the script and execution context are passed here, and no compilation products are passed - "compile once" occurs in
* On the port implementation side (run service), the caller should not have the concept of compiled products.
 */
export interface DynamicWorkflowRunSubmitRequest {
/** workflow script source code. Drop the library byte by byte (dwf_run.script_text), resume based on its hash. */
  scriptText: string;
/** The working directory of run (the root of the sandbox process cwd, world-read). */
  cwd: string;
  /**
* Display name for run (optional `input.name` for `CreateWorkflow`). Drop `dwf_run.name` to be used as a label for enumeration.
*Pure display metadata: does not participate in execution or resume verification; if absent, no name is given (press the first line of the script while reading).
   */
  name?: string;
  /**
* The actual parameters of this run (declarative parameters of saved workflow, the tool side has been verified according to the declaration and backfilled with default values).
   *
* Unlike `name`, this is not display metadata: it falls into `dwf_run.args_json` and is injected into the sandbox to make it readable by scripts
* The `args` global is part of the run identity - resume replays the saved copy and never accepts new ones. inline script
* There are no actual parameters, and the field is absent, which is `{}`.
   */
  args?: Record<string, unknown>;
/** Initiates the session for this run; engine events are projected back to the session. */
  parentSessionId?: SessionId | string;
/** Initiate the CreateWorkflow tool call for this run (association key of tool card → details page). */
  toolCallId?: ToolCallId | string;
  /**
* InputId of the round when the run is initiated: of the subagent
* `agent_step` is grouped under this message. Only if the hub starts directly fill it (`startSavedWorkflowRun` cast
* UUID v7, shared with the controlOnly launch wheel); the chat path is absent and is resolved by the run service from the parent runtime's active wheel.
   */
  launchInputId?: string;
  /**
* Stage table of script declaration (named stages of cause-and-effect diagram, declaration order, ≤ 32 × 128; `createWorkflowPhaseNames`). engine takes it
* Recorded along with the anchor point in `run-launched`, the sessions-index projection is used to draw the site in front of the sidebar mini-track. Pure display metadata: does not participate in execution,
* Does not participate in resume verification; absent when script does not have `phase()` mark.
   */
  phaseNames?: string[];
  /**
* This run’s own concurrency upper bound: flying at the same time
*Ask number, use `dwf_run.caps_max_concurrency` and resume as usual. **Absence is the ceiling** (machine derived value,
* `resolveWorkflowConcurrencyCeiling`); if given, clamp to `[1, ceiling]` - it can only lower concurrency, never raise it.
* The tool layer has been clamped once in `resolveInput` (the confirmation window should display the actual effective value), and what is clamped here is the port's own contract.
   */
  maxConcurrency?: number;
  /**
* On which model the subagent of this run runs. Journal event
* The source and resume of the canonical form of `run-launched` are still used; **In absence, the model of the initiated session is inherited**.
   *
* What is received is a structured {@link ModelSelection} instead of a string: the tool layer **has** put the name spoken by the user through the model directory
* After parsing once (it returned before the confirmation window if it could not be solved), the port should not do name matching again - that would make "where is the parsing
* happens" has two answers. The master agent itself is not affected: it remains on the session model.
   */
  subagentModel?: ModelSelection;
  /**
* The absolute path of the file from which this run script comes. Follow the same path as {@link subagentModel}: remember once with `run-launched`, never read by the engine,
* Zero SQL (there is no such column on `dwf_run`), two read planes are read back from the event.
   *
* ⚠ It has nothing to do with the `sourcePath` of the product in this file: that is where the product is placed, and this is the home of the **script**.
   *
* Absence means that this run does not have an editable script file (a project that cannot be written in draft, a run initiated before upgrading), and the model surface
* So fall back on the old saying of "change the script and submit it inline". Pure model polygon data: the desktop and TUI do not display it at all.
   */
  scriptPath?: string;
  /**
* "While running" table **aligned by position** with {@link DynamicWorkflowRunSubmitRequest.phaseNames}
* (`createWorkflowPhaseAlongside`): `phaseAlongside[i]` is when the strand is still entering `phaseNames[i]`
* **Subscripts** of other phases of the run (the subscripts fall in the same `phaseNames`). The sidebar mini track draws two parallel stations accordingly.
*Double line segment.
   *
* The same attitude as `phaseNames`: pure display metadata, dropped with the anchor point `run-launched`, the engine does not read; when there is no phase parallelism
* The entire field is absent (absent is "this track is a straight line").
   */
  phaseAlongside?: number[][];
  trace: TraceContext;
}

export interface DynamicWorkflowRunSubmitOptions {
  signal?: AbortSignal;
}

/**
* The result of submit. Success is only runId: it is both backgroundTaskId and cancelBackgroundWork
* workId (runId ≡ taskId ≡ workId), so the three paths do not need their own identity mapping tables. There is nothing objectionable about a new run:
* Wiring failures (compile products are damaged, journals are unavailable) are still thrown up.
 */
export type DynamicWorkflowRunSubmitResult = { ok: true; runId: string };

/**
* Request for {@link DynamicWorkflowRunPort.amend}.
 *
* The revision is **supersede**: cast **new run** with new script, import "completed ask for each named actor" from predecessor journal
* prefix" and world node for caching. The precursor can still be flying - that's the reason for this method: service stops it first,
* Wait for it to settle, then import and start again. Once the call is completed, the model no longer needs the three steps of TaskStop + polling + resubmission.
* Completely orthogonal to `scriptText`: revision run redeclares the script; arguments (only in saved sources) are not passed with revisions.
 */
export interface DynamicWorkflowRunAmendRequest {
  scriptText: string;
  cwd: string;
/** The revised predecessor run. Any state. */
  predecessorRunId: string;
/** The display name of the new run; in absence, the service inherits the name of the predecessor. */
  name?: string;
  parentSessionId?: SessionId | string;
/** Initiate the AmendWorkflow tool call for this revision (new run's tool card → details page associated key). */
  toolCallId?: ToolCallId | string;
/** **Declaration phase table for new script**; semantics are the same as {@link DynamicWorkflowRunSubmitRequest.phaseNames}. */
  phaseNames?: string[];
  /**
* The concurrency upper bound of the new run; the semantics are the same as {@link DynamicWorkflowRunSubmitRequest.maxConcurrency} (absence is the ceiling).
* "If omitted, the predecessor will be used, if `null` is released," is the three states of **tool surface**, in `resolveInput` of `AmendWorkflow`
* is normalized to a number here or absent - the confirmation window should display the inherited value, so that rule can only live before the handler.
   */
  maxConcurrency?: number;
  /**
* New run subagent model; semantics are the same as {@link DynamicWorkflowRunSubmitRequest.subagentModel} (absent means
*Inherits session model). "Omit it to use the predecessor, `null` means clear it" are the three states of **tool surface**, in `AmendWorkflow`
* `resolveInput` along with a reparse is normalized to an option or absence here.
   */
  subagentModel?: ModelSelection;
  /**
* The absolute path from which file the new script** comes; the semantics are the same as {@link DynamicWorkflowRunSubmitRequest.scriptPath}.
   *
* Unlike the concurrency upper bound and the subagent model, it has no three states: the revision is always recorded as the file from which the revised script comes.
* (`path` submission is the file, inline submission is the draft just written), never use the predecessor - the path of the predecessor points to
* is the **old script**, and recording it to the new run will allow the model to edit a file that is no longer running next time.
   */
  scriptPath?: string;
  /**
* **New script**'s "running at the same time" table; semantics are the same as {@link DynamicWorkflowRunSubmitRequest.phaseAlongside}
* (The subscript falls on the `phaseNames` of this request, not the predecessor table).
   */
  phaseAlongside?: number[][];
  /**
* The new run inherits the actual parameters of the predecessor library (`dwf_run.args_json`). Absent means revision takes no arguments (the toolpath's contract remains unchanged).
* Only the "Configuration" of the GUI passes it: it reruns the precursor's own script, and the script reads exactly the actual parameters when the precursor starts up; if not used, one with actual parameters
* Saved workflows launched from the hub will be rerun with empty `args`.
   */
  inheritArgs?: true;
  trace: TraceContext;
}

/**
* amend The structured reason for rejection. The two pairs of models are **two different next steps** (change a run id / give up the revision and go once
* new run), so must be distinguishable. The operable copy is at the tool layer, and the port only carries the discrimination key.
 *
* There is no "front wheel still flying" clause: the flying front wheel is stopped instead of rejected (the old version of `not_amendable` and the consequent
* "After stopping, poll to stopped and then resubmit" race condition disappears).
 *
* **Rejection means zero side effects**: the preflight runs before stopping the precursor**, and when rejected there is no dwf_run line, no registry entry,
* The front wheel drive is still running.
 */
export type DynamicWorkflowRunAmendRefusalReason =
/** There is no such precursor run in the journal. */
  | "run_not_found"
/** The predecessor has a completed ask but lacks message boundary accounting, so the imported transcription truncation cannot be discussed (whole rejection, no downgrade fallback). */
  | "missing_boundaries";

export type DynamicWorkflowRunAmendResult =
  | {
      ok: true;
      runId: string;
/** The predecessor is present when it is flying and stopped by this revision (= predecessorRunId); it is absent when the predecessor has already been settled. */
      supersededRunId?: string;
    }
  | { ok: false; reason: DynamicWorkflowRunAmendRefusalReason };

/**
* The initiator of the cancellation. `user` / `model` are the initiators of two stop entries; `{ superseded }` is the amend path
* Passed when the flying precursor is stopped: the id of the new run is dropped into the settlement bag of the precursor along with the reason (`supersededBy`).
 */
export type DynamicWorkflowRunCancelInitiator = "user" | "model" | { superseded: string };

/**
* run snapshot: follows the shape of {@link WorkflowTaskSnapshot} (the background task tracker and notification pipeline read it),
* Only relax `output` - the product of workflow run is the top-level return value of the script, the shape is determined by the script, not legacy
* The output type of the `Workflow` tool. The legacy port itself is not widened (the two workflow mechanisms do not share the port).
 *
* `reports` is the incremental product **original value** handed over by the script `report(item)`, in reporting order, from journal
* `kind = "report"` node row - that is the persistent home for these entries (`workflowRuns.reports` is bounded only
*memory-only display). Accordingly, the completion notification will be returned in completed / failed / canceled state:
* A run that dies on the 12th ask still completes the work of the 11th ask, and retrieving it is the reason for the existence of `report`.
 */
export type DynamicWorkflowRunSnapshot = Omit<WorkflowTaskSnapshot, "output"> & {
  output?: unknown;
  /**
* The real final form of run. The `status` of the base class is a background task
* Common vocabulary for trackers (`stopped` is folded into `cancelled`, `errored` is folded into `failed`), notification and tool copy
* Both fields must be read to tell the truth; `stopReason` is only present when `runStatus === "stopped"`.
   */
  runStatus?: DynamicWorkflowRunLifecycleStatus;
  stopReason?: DynamicWorkflowRunStopReason;
/** Initiates the session for this run (journal's parent_session_id; get the registry entry if it exists). */
  parentSessionId?: string;
/** The run from which this run was revised; if it is not a revision, it will be absent. */
  resumedFrom?: string;
/** This run is stopped and replaced by which revision; if it is not replaced, it is absent. */
  supersededBy?: string;
  /**
* This run's own concurrency upper bound (`dwf_run.caps_max_concurrency`), **only present when below the current ceiling**:
* There is nothing to say about run on the ceiling ("nothing means absence", the same as `reports`). `AmendWorkflow`
* `resolveInput` is used to decide what to use when `max_concurrency` is omitted.
   */
  maxConcurrency?: number;
  /**
* The subagent model of this run (the one on the journal event `run-launched`), canonical form
* `providerId/modelId[$reasoningLevel]`, **only present when set**: nothing to say about the run that inherits the session model
* ("Absent if none", same as `maxConcurrency`).
* This is a string instead of {@link ModelSelection}: the reading surface is only used for display and backfilling as it is, no one takes the value by field.
   */
  subagentModel?: string;
  /**
* The script file for this run (absolute path, the one on the journal event `run-launched`). **Only present while this run notes the file**.
   *
* According to the final notification, it replaces "modify the script and then submit it inline" with "edit the file in place and then revise `path`", so it must
* Can be read from the snapshot; the user interface is not displayed at all (unlike `subagentModel`, which will enter the run panel of the desktop).
   */
  scriptPath?: string;
/** Structured failure (same origin as {@link DynamicWorkflowRunDetail.error}); the `error` of the base class is its message. */
  failure?: DynamicWorkflowRunError;
  reports?: readonly unknown[];
  /**
* The upgrade question currently parked on this run, waiting for the master agent to answer.
   *
* **Projection from memory registry, not journal replay**: There are `escalation-raised` and
* `escalation-resolved` Two types of events, but "Who do I still owe an answer to now" is a living fact in the process - replayed
* Unpaired raised will only lie after the process dies (the parked deferred has already disappeared with the process, resume will let the actor
* Ask again and get new qid).
   *
* This is the **query after the notification is dropped (stale branch generation / shutdown drop): the main agent has no
* Questions to be answered can always be rediscovered through existing observations. When there are zero entries, the integer field is absent (empty array is not sent).
   */
  pendingQuestions?: readonly DynamicWorkflowRunPendingQuestion[];
  /**
* The **user interface products** released by this run are, in order of first appearance, from
* The `kind = "artifact"` line of the journal - that is the persistent home of the version history (`workflowRuns.artifacts` only takes
* Latest version metadata). Same as `reports`: **Only read in final state** (`getTask` is polled repeatedly, and the product line
*Consumer is final notification with GetWorkflowRun); part time integral field is absent.
   *
* ⚠ Terminology: The artifact here is the output of the script published to the user through `artifact.*`, which is the same as the `output` of this type.
* (The top-level return value of the script, called `RunSettlement.artifact` internally in the engine) is irrelevant.
   */
  artifacts?: readonly DynamicWorkflowRunArtifact[];
};

/**
* A version of a user-side product (the `ArtifactVersionRecord` on the journal `dwf_node.result_json`
*JSON image). **Intentionally restated here** instead of importing from @zcode/dynamic-workflow: the port only hosts
* JSON shape (same argument as {@link DynamicWorkflowRunLifecycleStatus}).
 *
* Content product (`file` / `markdown`) fill in `contentType` / `bytes` / `uri` / `sourcePath`; preset dashboard
* (`chart` / `table` / `metrics` / `board`) fill in `spec`. Bytes are never here - `uri` points to
 * tool-artifact store。
 */
export interface DynamicWorkflowRunArtifactVersion {
  version: number;
  title?: string;
  description?: string;
  contentType?: string;
  bytes?: number;
  uri?: string;
  sourcePath?: string;
  spec?: unknown;
/** Release time (epoch milliseconds). The driver is always written. */
  publishedAt: number;
/** This version is a deliverable of run. */
  primary?: true;
}

/** Member types of user interface artifacts (six members of facade `artifact.*`). */
export type DynamicWorkflowRunArtifactKind =
  | "file"
  | "markdown"
  | "chart"
  | "table"
  | "metrics"
  | "board";

/**
* A user interface product: all versions under id (in ascending order of version number) + the label report count fed to it.
* `title` / `description` / `contentType` / `sourcePath` / `spec` takes the value of **latest version**, which is convenient
* Readers who only care about "what is now" do not need to translate versions themselves.
 */
export interface DynamicWorkflowRunArtifact {
  id: string;
  kind: DynamicWorkflowRunArtifactKind;
  title?: string;
  description?: string;
  contentType?: string;
  sourcePath?: string;
  spec?: unknown;
/** The latest version number (= version of the last item in versions). */
  version: number;
  versions: readonly DynamicWorkflowRunArtifactVersion[];
/** The number of `report` entries tagged with this id (the data volume of the preset dashboard; the content product is always 0). */
  itemCount: number;
/** Deliverables for run (at most one). The `artifacts` list starts with it, and the rest are in order of first release. */
  primary?: true;
}

/** Feed a `report` entry to a certain preset product, positioned according to the journal sequence (the access side of the kanban board). */
export interface DynamicWorkflowRunArtifactItem {
  sequence: number;
  siteId: string;
  ordinal: number;
  item: unknown;
}

/** Paging bag for {@link DynamicWorkflowRunPort.listArtifactItems} (cursor = journal sequence, strictly greater than). */
export interface DynamicWorkflowRunArtifactItemPage {
  afterSequence?: number;
/** Required; the caller can pass "upper limit + 1" to detect hasMore, and the implementer cannot clamp it. */
  limit: number;
}

/** Return of {@link DynamicWorkflowRunPort.readArtifact}: all bytes of a certain version. */
export interface DynamicWorkflowRunArtifactBytes {
  bytes: Uint8Array;
  contentType: string;
}

// ————————————————————————————————————————————————————————————————
// workspace transcript
// ————————————————————————————————————————————————————————————————

/** Kind of workspace node: two world values ​​of journal `dwf_node.kind`. */
export type DynamicWorkflowRunWorkspaceNodeKind = "world-read" | "world-run";

/** The status of the node row, = journal's `NodeRecordStatus` (deliberately reiterated here, the reason is the same as lifecycle status). */
export type DynamicWorkflowRunWorkspaceNodeStatus = "running" | "completed" | "failed";

/**
* **Summary** of the line on the list: the numbers that can be reported without solving the text. Used by the storage layer using SQLite’s JSON functions in
* Calculate (`resultBytes` / `resultCount` / `exitCode` / `stdoutBytes` / `stderrBytes`) in the query,
* The port is transparently transmitted as it is. Which field is present depends on the op: the array body (glob/grep/changeFiles) has `resultCount`,
* `world.run` has exitCode and the number of bytes of the two outputs, and the string body only has `resultBytes`.
 */
export interface DynamicWorkflowRunWorkspaceNodeSummary {
/** The number of UTF-8 bytes after serialization of the body. */
  resultBytes: number;
  resultCount?: number;
  exitCode?: number;
  stdoutBytes?: number;
  stderrBytes?: number;
}

/**
* A line of workspace transcript: a `files.*` / `git.*` / `world.run` call, **without text**.
 *
* `op` / `args` comes from `input_json` added in migration 0030 (written when admission, ≤ 4 KB); historical lines before upgrade
* In the absence of both, the UI falls back to the step labels on the static diagram. `inputTruncated` means that args are itemized string previews rather than original values.
 */
export interface DynamicWorkflowRunWorkspaceNode {
  siteId: string;
  ordinal: number;
  kind: DynamicWorkflowRunWorkspaceNodeKind;
  op?: string;
  args?: readonly unknown[];
  inputTruncated?: true;
  status: DynamicWorkflowRunWorkspaceNodeStatus;
/** The structuring of the failed row failed (code + message of journal `error_json`; the remaining fields do not exit the port). */
  error?: DynamicWorkflowRunError;
/** This is only available if settlement is successful. */
  summary?: DynamicWorkflowRunWorkspaceNodeSummary;
/** Journal row creation/latest update time (epoch milliseconds); the difference between the two is the time taken for this step. */
  createdAt: number;
  updatedAt: number;
}

/** Paging bag for {@link DynamicWorkflowRunPort.readWorkspaceNodeResult}: byte limit for body. */
export interface DynamicWorkflowRunWorkspaceNodeResultQuery {
/** Required; the port is **bounded** by its body (truncated rather than rejected - this is the audit side, not the script's fetch side). */
  maxBytes: number;
}

/**
* The body of a workspace node: the shape-bounded `result`.
 *
* Truncation is **shape-preserving**: string trimming, array trimming, stdout / stderr of `world.run` are trimmed respectively.
* `truncated` indicates that truncation has occurred, `totalBytes` is the number of bytes before truncation. The running line has no text; the failed line
* Only `error`.
 */
export interface DynamicWorkflowRunWorkspaceNodeResult {
  status: DynamicWorkflowRunWorkspaceNodeStatus;
  result?: unknown;
  error?: DynamicWorkflowRunError;
  truncated: boolean;
  totalBytes: number;
}

/** A pending upgrade issue. Fields have the same origin as the `escalation-raised` event, plus the question time. */
export interface DynamicWorkflowRunPendingQuestion {
/** Globally unique question id (formed as `dwfq-<runId fragment>-<seq>`); `resolveQuestion` only recognizes it. */
  qid: string;
/** The actor asking the question, in `refToString` form (such as `actor#1@1`). Always present and uniquely positioned within the run. */
  actor: string;
  /**
   * 这个 actor 的人类可读名（脚本里 `agent("poet")` 的 `"poet"`）。
   *
   * **匿名 actor 缺席本字段，且这里不合成任何兜底标签**：兜底是渲染决策，通知面与侧栏各有
   * 各的合适写法（一个要读成句子，一个要塞进一列）。在这里合成一个「actor#1@1」当名字，
   * 只会让两个消费者都拿不回「这个 actor 其实没有名字」这条事实。
   */
  actorName?: string;
  question: string;
  /** actor 补充的上下文（`escalate` 的可选 `context`）。 */
  context?: string;
  /** 提问时刻（epoch ms）。主代理据它判断「这个问题已经等了多久」。 */
  askedAt: number;
}

/**
 * {@link DynamicWorkflowRunPort.resolveQuestion} 的结构化拒绝理由。三者对模型是**三个不同的
 * 下一步**，所以必须可分辨：去快照里取正确的 id / 什么都不用做 / 这个 run 已经不需要答案了。
 */
export type DynamicWorkflowResolveQuestionRefusalReason =
  /** 注册表里没有这个 qid：拼错了，或来自已亡故进程的陈旧 id（停驻项不持久化）。 */
  | "unknown_question"
  /** 这个问题已经被回答过，actor 早已带着那次答案继续。 */
  | "already_resolved"
  /** qid 所属的 run / ask 已不在飞行中（被取消、失败或已结束），没有人在等这个答案。 */
  | "run_not_in_flight";

/**
 * `resolveQuestion` 的结构化结果。失败走 reason 而不是 throw，与
 * {@link DynamicWorkflowRunSubmitResult} 同一条论证：三种理由全是调用方可预期的业务分支。
 *
 * `message` 由实现侧写好（陈述现状与下一步）而不是留给工具层拼：判别键与文案分开维护，
 * 两处迟早会说不同的话，而这里的读者是模型——它读到的就是它的下一步。
 */
export type DynamicWorkflowResolveQuestionResult =
  | { ok: true; qid: string }
  | { ok: false; reason: DynamicWorkflowResolveQuestionRefusalReason; message: string };

export interface DynamicWorkflowRunWaitOptions {
  signal?: AbortSignal;
}

/** 事件日志的分页参数；cursor = journal sequence（appendEvent 单调分配）。 */
export interface DynamicWorkflowRunEventPage {
  /** 只取 sequence 严格大于该值的事件；缺省从头取。 */
  afterSequence?: number;
  limit?: number;
}

/**
 * 一条 run 事件的**协议形态**：sequence + 事件种类 + JSON 载荷。
 *
 * 刻意不复用引擎的 `RunEvent`：那是领域包（@zcode/dynamic-workflow）的词汇表，把它
 * import 进 contracts 会让每一个持有端口的层都编译期依赖引擎内部类型。端口只承载
 * JSON 形状，`type` 是不透明字符串，`payload` 由读端按需解释。
 */
export interface DynamicWorkflowRunEvent {
  sequence: number;
  type: string;
  payload: Record<string, unknown>;
  /** 载荷经 {@link boundDynamicWorkflowRunEventPayload} 裁剪过（原始事实仍在 journal）。 */
  truncated?: boolean;
}

/**
 * run 事件载荷的界。**协议边界上的所有载荷有界**，而引擎的事件
 * 里有两个天然无界的字段：`actor-created` 的 `persona.system`（整段 system prompt）与
 * node 级错误的 `finalText`（一整轮模型输出）。它们不该按「大概不会很长」放行。
 *
* The bounds are **structural** (string length / number of array items / number of keys / depth) rather than the total number of bytes: structural bounds can
* Apply field by field in place, no need to serialize it first and then roll back, nor will the remaining fields be thrown away because of a huge field.
 */
export const DYNAMIC_WORKFLOW_RUN_EVENT_PAYLOAD_LIMITS = {
  maxStringLength: 2_048,
  maxArrayItems: 32,
  maxKeys: 32,
  maxDepth: 6,
} as const;

/**
* Cut the payload of a run event to within {@link DYNAMIC_WORKFLOW_RUN_EVENT_PAYLOAD_LIMITS},
* And incidentally normalized into a **JSON serializable** shape.
 *
* Two consumers share this serialization:
* 1. The event page returned by `listEvents` (event log of the details page);
* 2. Append to the `dynamic_workflow_run_progress` session event of the parent session (→ `workflowRuns` projection).
 *
* Normalization is not an optional side job, but required: any non-finite number (`Infinity` / `NaN`) is modified by `JSON.stringify`
* will become `null` - that means "the same load, before and after it is dropped into the warehouse". Rather than having each reader face this inconsistency individually,
* Here, it is folded into `null` at once, so that the return value satisfies the structural equivalence of `JSON.parse(JSON.stringify(x)) === x`.
 */
export function boundDynamicWorkflowRunEventPayload(payload: Record<string, unknown>): {
  payload: Record<string, unknown>;
  truncated: boolean;
} {
  let truncated = false;
  const markTruncated = (): void => {
    truncated = true;
  };
  const bounded = boundJsonValue(payload, 0, markTruncated);
  return {
    payload: isJsonRecord(bounded) ? bounded : {},
    truncated,
  };
}

function isJsonRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Recursive cropping. Returning `undefined` indicates that the value cannot be carried (the caller omitted it from the object/array). */
function boundJsonValue(value: unknown, depth: number, markTruncated: () => void): unknown {
  const limits = DYNAMIC_WORKFLOW_RUN_EVENT_PAYLOAD_LIMITS;

  if (value === null) return null;
  if (typeof value === "boolean") return value;
  if (typeof value === "number") {
// Infinity / NaN are not legal JSON numbers; collapse to null instead of letting JSON.stringify do it secretly.
    return Number.isFinite(value) ? value : null;
  }
  if (typeof value === "string") {
    if (value.length <= limits.maxStringLength) return value;
    markTruncated();
    return truncateSurrogateSafe(value, limits.maxStringLength);
  }
  if (typeof value !== "object") {
// undefined / function / symbol / bigint: omitted (bigint is not JSON serializable).
    return undefined;
  }

  if (depth >= limits.maxDepth) {
    markTruncated();
    return undefined;
  }

  if (Array.isArray(value)) {
    const items =
      value.length > limits.maxArrayItems ? value.slice(0, limits.maxArrayItems) : value;
    if (items.length < value.length) markTruncated();
    const out: unknown[] = [];
    for (const item of items) {
      const boundedItem = boundJsonValue(item, depth + 1, markTruncated);
// Holes in the array change subscript semantics, so unbearable elements are null instead of being skipped.
      out.push(boundedItem === undefined ? null : boundedItem);
    }
    return out;
  }

  const entries = Object.entries(value as Record<string, unknown>);
  const kept = entries.length > limits.maxKeys ? entries.slice(0, limits.maxKeys) : entries;
  if (kept.length < entries.length) markTruncated();
  const out: Record<string, unknown> = {};
  for (const [key, item] of kept) {
    const boundedItem = boundJsonValue(item, depth + 1, markTruncated);
    if (boundedItem !== undefined) out[key] = boundedItem;
  }
  return out;
}

/**
* Truncate to UTF-16 code units, but never leave a lone surrogate - that's not legal text,
* Also causes downstream JSON encoding and decoding to report errors at certain runtimes. Prefer one less symbol when falling in the middle of a surrogate pair.
 */
function truncateSurrogateSafe(value: string, maxLength: number): string {
  const cut = value.slice(0, maxLength);
  const lastCode = cut.charCodeAt(cut.length - 1);
  const isHighSurrogate = lastCode >= 0xd800 && lastCode <= 0xdbff;
  return isHighSurrogate ? cut.slice(0, -1) : cut;
}

/**
* **Any script value** in workflow run (top-level returned product, `report(item)` entry) → shown to the model or reader
* text. The implementation has been moved into `@zcode/shared/zcode-protocol-v4` with the shared workflowRuns reducer
* (workflow-artifact.ts, see the comments there for rules and ins and outs): The reduction of `reports[].preview` sinks to
* shared becomes the fourth consumer, and the dependency direction is contracts → shared, so only the function can be moved accordingly. Reserved here
* Re-export, the existing three consumers (completion notification, TaskOutput's resultText, v4 projection) do not change a single line.
 */
export { serializeWorkflowArtifact } from "@zcode/shared/zcode-protocol-v4";

/**
* Narrow port of workflow run. Parallel to legacy {@link import("./workflow.port.js").WorkflowPort} instead of
* Merge: The latter serves `Workflow` tools and `workflow_*` old tables. Sharing a port is equal to
* This boundary "independent of the existing workflow mechanism" is recoupled.
 *
* Cancel has no exclusive RPC: the details page button and the stop of the background panel share the existing v4 `cancelBackgroundWork`
* command, it falls here {@link cancel} (runId ≡ workId).
 */
export interface DynamicWorkflowRunPort {
/** Compile once and start the engine; return runId (i.e. backgroundTaskId). */
  submit(
    request: DynamicWorkflowRunSubmitRequest,
    options?: DynamicWorkflowRunSubmitOptions,
  ): Promise<DynamicWorkflowRunSubmitResult>;
  /**
* Revise a run: pre-check the precursor → cast a new id → cancel the precursor with `{ superseded: newRunId }` and wait for it to settle when it is in flight
* → import cache from predecessor → start a new run (see {@link DynamicWorkflowRunAmendRequest}). Pre-inspection is rejected
* Structuring failed and **nothing** was moved. Old hosts may not have this method (optional member): the tool layer is normalized according to ability detection.
* "This session does not support revisions".
   */
  amend?(
    request: DynamicWorkflowRunAmendRequest,
    options?: DynamicWorkflowRunSubmitOptions,
  ): Promise<DynamicWorkflowRunAmendResult>;
  /**
* run's own concurrency upper bound ceiling (`max(1, min(16, availableParallelism() − 2))`, one value per process). Synchronous, no side effects.
   *
* Two readers of the tool layer: `resolveInput` of `CreateWorkflow` / `AmendWorkflow` gives the model
* `max_concurrency` is clamped below it (the confirmation window must display the value that will take effect), `GetWorkflowRun` is determined by it
* Is an upper bound on run worth mentioning? **Optional member** (consumer `typeof` detection): The port stub does not need to be run for it,
* When absent, the tool layer will not be clamped and will be downloaded as it is (the port implementation will clamp it again).
   */
  concurrencyCeiling?(): number;
  getTask(taskId: string): Promise<DynamicWorkflowRunSnapshot | undefined>;
  waitForTask(
    taskId: string,
    options?: DynamicWorkflowRunWaitOptions,
  ): Promise<DynamicWorkflowRunSnapshot | undefined>;
  /**
* Stop a run: Stop the flying ask and kill the child process, and the run resolves `stopped(initiator)`. `initiator`
* Default `user`; the main agent passes `model` when stopped via TaskStop (the reason has been dropped from the library and no longer only lives in the background task registry); the amend path passes `{ superseded: newRunId }`.
* Unknown runId returns false.
   */
  cancel(runId: string, initiator?: DynamicWorkflowRunCancelInitiator): Promise<boolean>;
/** Press the cursor to fetch the event log; out of bounds cursor returns an empty page instead of reporting an error. */
  listEvents(
    runId: string,
    options: DynamicWorkflowRunEventPage,
  ): Promise<DynamicWorkflowRunEvent[]>;
  /**
* Enumerate runs by project (cwd), with the most recently updated first. Services `ListWorkflowRuns` tool.
   *
* **Optional member**, following the precedent of {@link cancel} (consumer `typeof` detection): the implementation only has the journal
* It is only provided with introspection query, and the existing port stub does not need to be fully supported for a read-only enumeration surface. Consumer pair
* "Port Absent" and "Method Absent" give the same business failure - it is the same thing for the model (this session does not have this capability).
   */
  listRuns?(query: DynamicWorkflowRunListQuery): Promise<DynamicWorkflowRunListResult>;
  /**
* Single run details (progress summary + artifacts/failures). Service `GetWorkflowRun` tool. Unknown runId returned
* `undefined` (consumer normalized to `run_not_found`), **does not do wait/block semantics** - waiting is
* {@link waitForTask}'s work, here is an instant snapshot. Optional members are available for the same reason as {@link listRuns}.
   */
  getRunDetail?(runId: string): Promise<DynamicWorkflowRunDetail | undefined>;
  /**
* The original text of the script archived by run (`dwf_run.script_text`, the same byte replayed by resume), byte by byte, without any processing.
* Read it in two places in `AmendWorkflow`: when the script is omitted, the predecessor script is backfilled in
* Input parameters, and `path` revised `script_unchanged` preflight -
* It must be compared with bytes, not hash. What the tool reads is the file content, not the compiled product. GUI "configuration" follows the same reading path.
   *
* A single read surface instead of a field of {@link DynamicWorkflowRunSnapshot}: the snapshot is polled repeatedly by the background tracker, and
* The script is the largest string on the port ({@link DynamicWorkflowRunSummary.label} for the same reason). unknown run
* and "No script in the record" (the old run before being dropped) both return `undefined` - the same fact for the caller: there is no one to inherit.
* Script. Read only, does not check whether the service is closed. **Optional member** (consumer `typeof` detection), the same reason as {@link listRuns}:
* Revisions of omitted scripts fail outright when absent, and the `script_unchanged` preflight is skipped (it's a net, not a gate).
   */
  getScript?(runId: string): Promise<string | undefined>;
  /**
* Resume a run that has been canceled/interrupted by process death: rerun with the same runId (the engine takes the resume branch, journal
* Hit short circuit, unfinished nodes are redistributed). The gate is on the implementation side: only `cancelled` or `failed` and the failure code is
* The run of `Interrupted` is resumable.
   *
* **Optional member**, follow the previous cancel precedent of {@link listEvents} (consumer `typeof` detection):
* The port stub does not need to be fully supported for resume; for the consumer, "port absence" and "method absence" are the same business failure.
   */
  resume?(runId: string): Promise<DynamicWorkflowRunResumeResult>;
  /**
* Enumerate the run summaries under the name of the parent session of this service (most recently updated first, journal-backed). After restarting the UI, the following is found:
* `workflowRuns` projection does not survive across processes, and the availability of the tool card join and Resume buttons can only be restored from here.
* Deliberately does not accept the parentSessionId parameter - the service instance is constructed according to the parent session (per-app), allowing the caller to pass any
* A session is equivalent to opening a cross-session read hole. The reasons for optional membership are the same as {@link resume}.
   */
  listRunsForSession?(limit?: number): Promise<DynamicWorkflowRunSessionSummary[]>;
  /**
* Cold playback: put **this service's parent session** under the name of this process
* Runs that have not been run are played back from the journal into progress event payloads - the same type as those delivered by `onRunEvent` during live
* Payloads, the same casting chain, cold materialization feed them as memory events to the same reducer, `workflowRuns` projection therefore
* Consistent byte-for-byte before and after restart.
   *
* - The upper bound is the same as the elimination of projections (8 most recently updated), with the oldest run first;
* - `excludeRunIds`: Runs that already have events in the caller's memory (this process has run/is running) will not be played back;
* - Append a run if the line is final and the event stream does not have `run-settled` (the line was overwritten by orphan convergence after the process died)
* **Memory state** synthesizes the settle payload (portable status / failure / resumable) and never writes to the journal.
   *
* The reason for optional members is the same as {@link listRunsForSession}: the memory journal has no enumeration surface, and there is nothing to restore during playback.
   */
  replayProgressForSession?(input: {
    excludeRunIds: ReadonlySet<string>;
  }): Promise<DynamicWorkflowRunProgressPayload[]>;
  /**
* Answer a blocking question after actor upgrade. Serve
* `ResolveWorkflowQuestion` tool.
   *
* Only accept one opaque token instead of `(runId, qid)` Right: qid is globally unique (across runs), when multiple runs are running concurrently
* Letting models pair themselves is a breeding ground for mismatches. The answer becomes the utility result of the actor's `escalate` call as it is.
* The actor's turn continues immediately; the run state remains unchanged throughout the process (the upgrade is a slow tool call inside ask,
* is not a run lifecycle event).
   *
* **Optional member**, following the example of {@link resume} (consumer `typeof` detection): the port stub does not have to be a
* The response side is fully supported; for the consumer, "port absence" and "method absence" are the same business failure.
   */
  resolveQuestion?(qid: string, answer: string): Promise<DynamicWorkflowResolveQuestionResult>;
  /**
* List of user interface products for this run (journal `kind = "artifact"` rows are grouped by id, in ascending order of version). Durable readout of UI cold recovery and hub details.
* Unknown runId returns `undefined`. **Optional member**, the reason is the same as {@link listRuns} (journal with products
* Provided only when reading; consumer side `typeof` detection).
   */
  listArtifacts?(runId: string): Promise<readonly DynamicWorkflowRunArtifact[] | undefined>;
  /**
* Feed the `report` entries of a certain preset product, and page them in ascending order according to the journal sequence (the access side of the Kanban board).
* Out-of-bounds cursor returns an empty page instead of reporting an error. Optional member for the same reason as {@link listArtifacts}.
   */
  listArtifactItems?(
    runId: string,
    artifactId: string,
    page: DynamicWorkflowRunArtifactItemPage,
  ): Promise<readonly DynamicWorkflowRunArtifactItem[]>;
  /**
* Read the bytes of a certain product version: **First** confirm that `(runId, artifactId, version)` has a line in the journal
* `completed` record, then press the `uri` on the line to get it through the tool-artifact store - any id passed by the caller will
* Not directly a path. No such version / non-content product /
* store absent → `undefined`. Chunked return gateway (v4 `workflowRunArtifactRead`, ≤ 512 KiB block).
* Optional member for the same reason as {@link listArtifacts}.
   */
  readArtifact?(
    runId: string,
    artifactId: string,
    version: number,
  ): Promise<DynamicWorkflowRunArtifactBytes | undefined>;
  /**
* The workspace of this run transcript: journal
* The lines of `kind ∈ {world-read, world-run}` are listed first and first without text.
   *
* Authorization is in the same chain as {@link readArtifact}: run must belong to the parent session of this service, otherwise `undefined`
* (Same answer as "no such run" - don't tell the overriding caller which half it guessed correctly). The text may contain workspace files
* content, so the list does not allow other conversations. Optional member for the same reason as {@link listArtifacts}.
   */
  listWorkspaceNodes?(
    runId: string,
  ): Promise<readonly DynamicWorkflowRunWorkspaceNode[] | undefined>;
  /**
* The body of a workspace node, conformally bounded by `maxBytes`. The authorization chain is the same as {@link listWorkspaceNodes};
* No such node / non-world line / not your run → `undefined`. Optional member for the same reason as {@link listArtifacts}.
   */
  readWorkspaceNodeResult?(
    runId: string,
    siteId: string,
    ordinal: number,
    query: DynamicWorkflowRunWorkspaceNodeResultQuery,
  ): Promise<DynamicWorkflowRunWorkspaceNodeResult | undefined>;
}

// ————————————————————————————————————————————————————————————————
//run introspection (the access side of ListWorkflowRuns/GetWorkflowRun)
// ————————————————————————————————————————————————————————————————

/**
* run life cycle status. Literally the same as journal's `dwf_run.status`, but **deliberately restated here**
* Instead of importing from @zcode/dynamic-workflow: the port only carries JSON shapes, once the engine's vocabulary is entered
* contracts, each layer holding a port will rely on engine internal types at compile time (with {@link DynamicWorkflowRunEvent}
* of the same argument of `type`).
 *
* Deliberately different from status of {@link DynamicWorkflowRunSnapshot}: the latter is the vocabulary of the background task tracker,
* Fold `pending` into `running`. The introspection surface must be retained `pending` - "has been submitted, the engine has not been built yet" is a
* Differences that are understandable and meaningful to the model.
 */
export type DynamicWorkflowRunLifecycleStatus =
  | "completed"
  | "errored"
  | "pending"
  | "running"
  | "stopped";

/**
* Reason for `stopped`: `user` user canceled / `model` master agent
* TaskStop / `provider` Deterministic model side error / `interrupted` The holding process died or the sandbox failed / `superseded`
* Stopped and replaced by an AmendWorkflow. The first four can be resumed, but `superseded` cannot (the living one is its successor);
* `errored` (script error) is not allowed. The literal is the same as the engine `RunStopReason`, and is deliberately restated here.
 */
export type DynamicWorkflowRunStopReason =
  | "user"
  | "model"
  | "provider"
  | "interrupted"
  | "superseded";

/** Query bag for {@link DynamicWorkflowRunPort.listRuns}. */
export interface DynamicWorkflowRunListQuery {
  /**
* Project key, **required**. Literal equivalent matches `dwf_run.cwd` (drop as it is on the writing side, check as it is on the reading side).
* The port does not guess a default cwd for the caller: the tool surface always checks `context.workingDirectory`, and the model does not have the right to scan libraries across projects.
   */
  cwd: string;
/** Return the upper limit of the number of entries, **required**. Clamping strategies belong to the tool surface ([1, 50]); ports do not do unbounded enumeration. */
  limit: number;
/** Optional subset of states. The default is no filtering; an empty array means "does not match any status" (returns to an empty list). */
  statuses?: readonly DynamicWorkflowRunLifecycleStatus[];
}

/**
* Lists and Details **Common Sections**. Tags, attribution annotations and timestamps must have the same origin field by field on both reading surfaces——
* The same run displays different names or attributions in the list and details, which is the most difficult to catch by testing and the most direct damage to trust.
* That kind of inconsistency. So here is a shared base interface, rather than two independently evolved field tables.
 */
export interface DynamicWorkflowRunSummary {
  runId: string;
  /**
* Show labels. **Cooked**: The implementation side (run service) is derived in the order of name → first line of script → runId.
* Directly displayed by the consumer. The reason why we don’t hand over the raw materials (name/scriptText) and let the tool layer do it by itself: that hidden chain
* is a heuristic when reading. If the two tools are combined once each, they will drift, and scriptText is the largest string on the port.
* (The list surface should never move 50 scripts across the border just to get the first line).
   */
  label: string;
/** Tag source: `"name"` = name given by the user; `"script"` = derived from the script when reading (including runId). */
  labelSource: "name" | "script";
  status: DynamicWorkflowRunLifecycleStatus;
/** Only present if `status === "stopped"` is present. */
  stopReason?: DynamicWorkflowRunStopReason;
/** Which run this run was revised from (`dwf_run.resumed_from`); if it is not a revision, it will be absent. */
  resumedFrom?: string;
/** Which revision this run was stopped and replaced (the settlement bag of `stopped(superseded)`); if it is not replaced, it is absent. */
  supersededBy?: string;
/** Whether this session is the initiator of this run (the parent_session_id of the journal is hit, or in the session registry). */
  ownedByThisSession: boolean;
  /**
* "This session cannot confirm that it is alive": journal is not final ∧ Not in this session ∧ Not in this session's registry. It may be a dead process
* is a relic, or it may be a running run in which a sibling session of the same process is running - so this is an **notation rather than a status rewrite**, and it will never be read.
* Collect corpses for others (the execution rights of orphan collection only belong to the organization time of owning session). Be present when it is true.
   */
  possiblyInterrupted?: boolean;
/** `time_created` / `time_updated` (epoch ms) of journal. */
  createdAt: number;
  updatedAt: number;
}

/** One item in the list: common section + usage. Deliberately light - no actors, no node counting, no product previews. */
export interface DynamicWorkflowRunListItem extends DynamicWorkflowRunSummary {
/** Direct reading of `dwf_run.spent_tokens` (the only authority for run-level token usage). */
  spentTokens: number;
}

/**
* Return of `listRuns`. Intentionally an object rather than a naked array: page-level fields (like {@link truncated}) are pure appends
* Change, while the naked array can only change the shape as a whole.
 */
export interface DynamicWorkflowRunListResult {
  runs: DynamicWorkflowRunListItem[];
  /**
* There are more runs for this project that are not included on this page. **Be present only when it is true**.
   *
* The criterion is "take one more" (to realize the query and then fall back after pressing `limit + 1`), not `length === limit`: the latter is
* A false positive occurs when the number of items is exactly equal to the limit, and a false positive will cause the model to chase a page of non-existent history. The same convention for v4 gateways
*(`hasMore`) has been used once on the event page.
   */
  truncated?: boolean;
}

/**
* The progress and usage of run (observation surface, without any upper limit). `nodesObserved`
* is the number of rows of dropped nodes** (the sum of three states), and never pretends to be the "total number of steps": there is no static total in dynamic workflows, but
* `queued` only exists in the event phase and does not fall into the queue.
 */
export interface DynamicWorkflowRunUsage {
  spentTokens: number;
  nodesObserved: number;
  nodesRunning: number;
  nodesCompleted: number;
  nodesFailed: number;
}

/** An actor site instance. `persona` is not intentional: the entire system prompt is a naturally unbounded field on the port. */
export interface DynamicWorkflowRunActor {
  siteId: string;
  ordinal: number;
  name?: string;
}

/** A `log()` narrative. */
export interface DynamicWorkflowRunLogEntry {
  sequence: number;
/** Bounded by port's string upper limit ({@link DYNAMIC_WORKFLOW_RUN_EVENT_PAYLOAD_LIMITS}). */
  message: string;
  /**
* The time when this event was dropped into the journal (`dwf_event.time_created`). Serial number positioning, always answer "how long ago"——
* The three sets of fields in the situation section all calculate age according to this ruler. There is no reason to use another ruler for the narrative tail. **There is no old one in this column
* Absent in the journal **, the reading side does not give the age accordingly, and there is no need to use `Date.now()` when reading.
   */
  at?: number;
}

/** Structuring failed. `code` is a stable discriminant key - the model must be able to distinguish between "the process is dead" and "the script really failed". */
export interface DynamicWorkflowRunError {
  code: string;
  message: string;
/** Only present when `code === "ProviderStop"` (JSON image of engine `ProviderStopDetails`). */
  providerStop?: DynamicWorkflowRunProviderStop;
}

/** Structured details for `ProviderStop` (mirror of engine `ProviderStopDetails`, port only hosts JSON shapes). */
export interface DynamicWorkflowRunProviderStop {
  kind: "auth" | "not_configured" | "model_unavailable" | "invalid_request" | "quota" | "other";
  reason: string;
  providerId?: string;
  providerLabel?: string;
  modelId?: string;
  providerCode?: string;
  subagent?: string;
  subagentName?: string;
  phase?: string;
  rawMessage?: string;
  resetAt?: number;
}

// Types of situation sections (stage/subagent/health) live in dynamic-workflow-run-roster.port.ts (ditto),
// Export here as it is to keep the import path of `@zcode/contracts` unchanged.
export type * from "./dynamic-workflow-run-roster.port.js";

/** Single run details: common section + progress + state section + product forked by final state / failure. */
export interface DynamicWorkflowRunDetail extends DynamicWorkflowRunSummary {
  usage: DynamicWorkflowRunUsage;
  /**
* This run's own concurrency upper bound has the same semantics as {@link DynamicWorkflowRunSnapshot.maxConcurrency}: only when it is lower than the current
* Ceiling is present. Deliberately not {@link DynamicWorkflowRunSummary} - list rows should not be widened for a field that is rarely set.
   */
  maxConcurrency?: number;
  /**
* The subagent model of this run has the same semantics as {@link DynamicWorkflowRunSnapshot.subagentModel}: only when setting
* Be present. Same as `maxConcurrency` {@link DynamicWorkflowRunSummary} - list rows should not be one
* Rarely set fields are widened.
   */
  subagentModel?: string;
  /**
* The script file of this run has the same semantics as {@link DynamicWorkflowRunSnapshot.scriptPath}: only when the file is written down
* Be present. According to `GetWorkflowRun` it says the next step in `<amendable>` is "Edit this file in place".
   */
  scriptPath?: string;
  actors: DynamicWorkflowRunActor[];
/** `log()` The tail of events, in chronological order (sequence ascending order). If there are no log events, the array will be empty. */
  logTail: DynamicWorkflowRunLogEntry[];
  /**
* Phase table: the declared phases in the declaration sequence, followed by those that have been "entered but not declared"
   *
* **The script does not declare a stage, and no one has passed through it, so the entire field is absent** - there is no such thing as a stage in a run like that.
* Posting an empty array reads like "the stage table is empty", which is another sentence.
   */
  phases?: DynamicWorkflowRunPhaseView[];
  /**
* Subagent roster, in actor row order (=casting order). **Constant presence**, a run without an actor is empty
* Array: Unlike `phases`, "how many subagents does this run have" is always a question with an answer, and 0 is that answer.
   *
* Deliberately not merged with parallel `actors`: `actors` is a constant identity table (siteId / ordinal / name),
* The consumer has pressed it to join; each item here is a **read snapshot**, and the same run will be different every second.
   */
  subagents: DynamicWorkflowRunSubagentView[];
/** The run is still running as a whole (see {@link DynamicWorkflowRunHealth}). Always present. */
  health: DynamicWorkflowRunHealth;
  /**
* The top-level return value of the script, **original value** (not serialized). Only completed runs are present; `undefined` products
* That is, the entire field is absent.
   *
* Why not serialize here: model-oriented text projection already has the only implementation (core's
* `serializeWorkflowArtifact`, completion notification shares it with TaskOutput). Port do it again and it will appear
* "The products of the same run look different in the notification and in this tool" - exactly the type of loss that sharing should exclude.
* So the serialization stays in the core, and the port is only responsible for sending the original value to the boundary.
   */
  result?: unknown;
/** errored is always present; stopped is only present for provider / interrupted. The code is exposed as it is, not folded. */
  error?: DynamicWorkflowRunError;
  /**
* The upgrade question currently parked on this run, waiting for the master agent to answer.
   *
* Same source and same projection as {@link DynamicWorkflowRunSnapshot.pendingQuestions} (both read in-process upgrades
* Parking table, all zero fields are absent), but one reading surface is changed: snapshot service background task tracker, this field service
* `GetWorkflowRun` - and the latter is the only discovery surface on the model side**. This link is not an optional icing on the cake: Upgrade Notice
* There are two known discard paths (stale branch generation/shutdown). Query is the backbone of these two paths. The `unknown_question` copy of `resolveQuestion` also explicitly asks the model to come here to find the qid.
* Without it, both promises point to a tool that returns nothing.
   */
  pendingQuestions?: readonly DynamicWorkflowRunPendingQuestion[];
  /**
* The user interface products of this run (any status is attached; journal-backed, and {@link DynamicWorkflowRunSnapshot.artifacts}
*same origin). `GetWorkflowRun` accordingly tells the model "These have been presented to the user as cards, just reference them by title."
* Part time integral field is absent.
   */
  artifacts?: readonly DynamicWorkflowRunArtifact[];
}

/** Structured failure reason for {@link DynamicWorkflowRunPort.resume}. */
export type DynamicWorkflowRunResumeErrorReason =
/** This run does not exist in the journal. */
  | "not_found"
/** run is not in the recoverable set (completed or errored; only stopped is recoverable). */
  | "not_resumable"
/** run is stopped and replaced by an AmendWorkflow: the live one is the successor, and replaying it is equivalent to doing the same thing twice. */
  | "superseded"
/** The run with the same runId is flying within this process. */
  | "already_running"
/** The record is missing scriptText (the old run before dropping this field), and there is no rerunable script. */
  | "script_missing"
/** The recorded scriptHash is inconsistent with the one recalculated according to scriptText (the record itself has been rewritten by an external force). */
  | "script_mismatch"
  /**
* The recorded scriptText no longer passes type checking under the **current** facade (old run after facade refactoring). verbatim replay
* will only fail; the way out is to rewrite the script according to the current facade and then use AmendWorkflow. `message` carries bounded diagnostics.
   */
  | "compile_failed";

/**
* Structured results of resume. If failure occurs, use reason instead of throw: the five reasons are all business branches that the caller can expect.
* (error code instead of error text is used for process judgment, house rule), throw is only reserved for real wiring faults.
 */
export type DynamicWorkflowRunResumeResult =
  | { ok: true; runId: string; toolCallId?: string }
  | { ok: false; reason: DynamicWorkflowRunResumeErrorReason; message?: string };

/**
* Run summary for {@link DynamicWorkflowRunPort.listRunsForSession}. Literal and journal
* Same set as `dwf_run.status`, but **intentionally restated here**: the port only carries JSON shapes, the engine vocabulary is once
* In contracts, each layer holding a port will rely on engine internal types at compile time.
 */
export interface DynamicWorkflowRunSessionSummary {
  runId: string;
/** The CreateWorkflow tool call id that initiated the run (the associated key of Tool Card → Details Page/Resume); the old run is absent. */
  toolCallId?: string;
  /**
* Display label, derived when the server reads (`name` → first line of script → runId, see bootstrap
* `resolveDynamicWorkflowRunLabel`). Optional for **skew security**: the old server does not send this key,
* Just fall back to runId on the reading side - one less tag in the list is a degeneration, not an error.
   *
* The same reason as `resumable` is concentrated on the server side: each of the two places tries to find out, and the same run is
* Different names will be displayed in `/dwf list` and on the tool card.
   */
  label?: string;
  /**
* Last updated time (epoch milliseconds, `dwf_run.time_updated` from journal). Optional:
* The old server is absent and the time column is not displayed on the reading side. List sorting is still the responsibility of the storage layer (most recently updated first),
* This field is for display only - do not use it for rearrangement on the reading side, otherwise it will drift with the tie-break on the server side.
   */
  updatedAt?: number;
  status: "completed" | "errored" | "pending" | "running" | "stopped";
/** Only present if `status === "stopped"` is present. */
  stopReason?: DynamicWorkflowRunStopReason;
/** The run from which this run was revised; if it is not a revision, it will be absent. */
  resumedFrom?: string;
/** This run is stopped and replaced by which revision; if it is not replaced, it is absent. */
  supersededBy?: string;
/** Structured failure encoding of errored / stopped(provider|interrupted) (`ProviderStop` / `Interrupted` …). */
  failureCode?: string;
  failureMessage?: string;
  /**
* Whether it is recoverable. **The server presses the same predicate of the resume gate to calculate the result**: If the UI presses status+failureCode on its own
* Re-derivation, the two predicates will one day be inconsistent - the button is on but the command is rejected.
   */
  resumable: boolean;
}
