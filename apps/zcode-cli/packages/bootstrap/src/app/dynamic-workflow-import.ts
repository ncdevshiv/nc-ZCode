// ============================================================
// Import construction of amend-resume: read predecessor journal → ImportedRunCache
// ============================================================
//
// 本模块是 amend-resume 唯一的**读前驱**处，三个调用点共用它：
//   1. `port.amend` 的**预检**（{@link preflightAmendImport}）：停下在飞前驱之前就判定
//      `run_not_found` / `missing_boundaries`——被拒时前驱照旧在跑、一行不建；
//   2. `port.amend` 在前驱结算之后构建缓存（{@link buildImportedCache}）；
//   3. `port.resume` 见 `record.resumedFrom` 在场：崩溃后重建同一张表。
//
// 两处必须是**同一个纯函数**：修订 run 的 journal 只对「已到达的执行前缀」自含，未消费的导入
// 靠重建补回。所以确定性不是风格偏好而是正确性前提——同一份 journal 状态必须给出同一张表
// （无时间戳、无随机、遍历序全部来自 journal 的插入序）。
//
// 唯一的 I/O 是 journal 读与转录条数读（两者都经窄端口注入），本模块自己不碰会话存储实现。

import type { Logger, SessionId } from "@zcode/contracts";
import type {
  ActorRecord,
  ImportedActorCandidate,
  ImportedAskEntry,
  ImportedRunCache,
  ImportedWorldEntry,
  NodeRecord,
  RunRecord,
} from "@zcode/dynamic-workflow";
import { TERMINAL_RUN_STATUSES } from "./dynamic-workflow-run-observation.js";
import type { ActorTranscriptStore } from "./workflow-actor-transcript.js";

/**
 * 构建导入缓存所需的 journal 读面：三个读方法，全部按 runId。
 *
 * 结构上是引擎 `JournalStorePort` 的真子集（生产直接把 journal 传进来），窄化的理由与
 * {@link ActorTranscriptStore} 同款：构建器只读这三样，声明成整个端口会让「导入构建依赖
 * journal 的全部能力（含写）」变成一句真话——而它一个字节都不写。**前驱只读**是不变式，让类型把它说出来。
 */
interface ImportedCacheJournalReader {
  getRun(runId: string): RunRecord | undefined;
  listActors(runId: string): ActorRecord[];
  listNodes(runId: string): NodeRecord[];
}

/**
 * 导入构建被拒的三个理由。**判别键而非文案**：模型据它选下一步动作（换 run / 等它结算 /
 * 放弃修订走全新 run），所以三者必须可分辨。可操作文案归工具层。
 *
 * 与端口的 `DynamicWorkflowRunSubmitRefusalReason`（contracts）**逐字面同集**：service 把这里的
 * reason 原样交出去，所以两处一旦漂移就是编译错误，而不是一个悄悄变成 `undefined` 的判别键。
 * 刻意不从 contracts import：本模块是领域侧的构建器，端口词汇表反过来依赖它才是正确方向。
 */
type ImportedCacheRefusalReason =
  /** journal 里没有这个前驱 run。 */
  | "run_not_found"
  /** 前驱仍在飞（非终态）。修订**任意终态** run 都合法，含 completed。 */
  | "not_amendable"
  /** 前驱有已完结但缺消息边界的 ask：无 marker 前驱整体拒绝（无回退降级）。 */
  | "missing_boundaries";

/** 构建结果：成功带表与 lineage 指针，失败只带判别键。 */
type BuildImportedCacheResult =
  | { ok: true; cache: ImportedRunCache; resumedFrom: string }
  | { ok: false; reason: ImportedCacheRefusalReason };

/**
 * amend 预检的两个拒绝理由：与端口的 `DynamicWorkflowRunAmendRefusalReason` 逐字面同集。
 * 没有 `not_amendable`——在飞的前驱由 amend 停下，不是被拒。
 */
type AmendPreflightRefusalReason = Exclude<ImportedCacheRefusalReason, "not_amendable">;

type AmendPreflightResult =
  | { ok: true; run: RunRecord }
  | { ok: false; reason: AmendPreflightRefusalReason };

/**
 * amend 的**预检**：前驱存在 ∧ 每个已完结 ask 都有消息边界。**不看状态**——这两条都是前驱
 * journal 的性质，停止不会改变它们，所以在停止之前就能判定；预检过了再停，被拒的 amend 才
* Will not leave a run that was stopped in vain. The completed ask of the run on the fly has already been dropped into the library together with the boundary, so for the predecessor of the fly
*'s preflight is as conclusive as that for settled precursors.
 */
export function preflightAmendImport(
  journal: Pick<ImportedCacheJournalReader, "getRun" | "listNodes">,
  predecessorRunId: string,
): AmendPreflightResult {
  const run = journal.getRun(predecessorRunId);
  if (run === undefined) return { ok: false, reason: "run_not_found" };
  if (!completedAsksHaveBoundaries(journal.listNodes(predecessorRunId))) {
    return { ok: false, reason: "missing_boundaries" };
  }
  return { ok: true, run };
}

/**
* The predicate of gate 3 is shared between preflight and build: no marker. There are only two causes - the journal written before the marker column is introduced, and the driver
* Side accounting failed - both mean "the boundary accounting of this run is not trustworthy", so strict to the whole table instead of just looking at it will be imported
* of those. It's **normal** for unfinished asks to have no boundaries (they never import prefixes), so only the completed lines are looked at.
 */
function completedAsksHaveBoundaries(nodes: readonly NodeRecord[]): boolean {
  for (const node of nodes) {
    if (node.kind !== "ask" || node.status !== "completed") continue;
    if (node.messageBoundary === undefined) return false;
  }
  return true;
}

/**
* Three dependencies required to build the import cache. Field names are aligned verbatim with `DynamicWorkflowRunServiceDeps` (this interface is its
* Structure subset), so the two call points of run service directly pass in `deps` as it is - one more layer of rename mapping, just one more place
* Wiring that will drift, and the symptom of drift is silent degradation such as "the transcription interface is clearly connected but no integrity check is performed."
 */
interface AmendImportDeps {
  journal: ImportedCacheJournalReader;
  /**
* Conversation transcript reading. Do an extra **source honesty check** when present (see {@link honorsBoundary}); when absent
* Candidate harvesting - seed cashing on the driver side will still fail loudly, which is a corruption-level trap.
   */
  actorTranscriptStore?: ActorTranscriptStore;
  logger?: Logger;
}

/**
* Read the predecessor journal and build {@link ImportedRunCache}. **Purely certain**: The same journal status always gives the same table.
 *
* The three gates are in order (door first, built later: if the gate is not outdated, there is no need to read a line):
* 1. The precursor does not exist → `run_not_found`;
* 2. Precursor non-final state → `not_amendable`;
* 3. The predecessor has completed but no `messageBoundary` ask → `missing_boundaries`.
 *
* The reason why gate 3 is **strict to the entire table** (instead of only checking those that will actually be imported): There are only two reasons for no marker——
* The marker column introduces the journal written before, and the driver side accounting fails - both mean "the boundary accounting of this run is not trustworthy",
* Rather than "This item happened to be missing." Releasing items one by one is equivalent to asking a semi-bad accounting pioneer to produce a seemingly complete table.
* Truncated to a wrong position when diverging (the boundary between the text seen by the model and the journal note is quietly misaligned). dwf is not published,
* Journals without markers only exist on development machines (rejected overall, no synthetic seeding fallback).
* It is **normal** for unfinished asks to have no boundaries (they never import prefixes), so the gate only looks at completed lines.
 */
export async function buildImportedCache(
  deps: AmendImportDeps,
  predecessorRunId: string,
): Promise<BuildImportedCacheResult> {
  const { actorTranscriptStore: transcripts, journal, logger } = deps;

  const run = journal.getRun(predecessorRunId);
  if (run === undefined) return { ok: false, reason: "run_not_found" };
// Revisable set = any final state, **intentionally not reusable** plain resume's isResumableRecord: that predicate is
// The gate of byte-identical resume (stopped), and the revision excludes exactly the two categories
// The most useful - script true failure (bug fix, save cache) and completed (warm startup extended analysis). Each of the two collections has its own story.
  if (!TERMINAL_RUN_STATUSES.has(run.status)) return { ok: false, reason: "not_amendable" };

  const nodes = journal.listNodes(predecessorRunId);
  if (!completedAsksHaveBoundaries(nodes)) return { ok: false, reason: "missing_boundaries" };

  const actors = new Map<string, ImportedActorCandidate>();
  for (const record of namedUniqueActors(journal.listActors(predecessorRunId), logger)) {
    const name = record.name!;
    const candidate = await buildActorCandidate({
      actor: record,
      journal,
      ...(logger === undefined ? {} : { logger }),
      nodes,
      predecessorRunId,
      ...(transcripts === undefined ? {} : { transcripts }),
    });
    if (candidate !== undefined) actors.set(name, candidate);
  }

  return {
    ok: true,
    cache: { actors, world: buildWorldQueues(nodes) },
    resumedFrom: predecessorRunId,
  };
}

/**
* Candidate actor row in the precursor: the name is not empty ∧ The name is unique within this run.
 *
* Anonymous will not be accepted (the name is the cached identity key, without a name there will be no comparable coordinates); duplicate names **Both will not be accepted**——
* The engine's `DuplicateActorName` is a run-time invariant added later, which may actually exist in the journal written earlier than it is written.
* Duplicate names work, and "candidate by name" is a roll of the dice on that kind of precursor. It’s better to choose neither: the price is that both actors are brand new
* Rerun, and the cost of picking the wrong one is to treat another actor's session prefix as the previous actor's.
 */
function namedUniqueActors(records: ActorRecord[], logger?: Logger): ActorRecord[] {
  const byName = new Map<string, ActorRecord[]>();
  for (const record of records) {
    const name = record.name;
    if (name === undefined || name === "") continue;
    const bucket = byName.get(name);
    if (bucket === undefined) byName.set(name, [record]);
    else bucket.push(record);
  }
  const unique: ActorRecord[] = [];
  for (const [name, bucket] of byName) {
    if (bucket.length === 1) {
      unique.push(bucket[0]!);
      continue;
    }
    logger?.warn?.("Dynamic workflow amend: duplicate actor name in predecessor, skipped", {
      actorName: name,
      count: bucket.length,
      event: "dynamic_workflow.amend.duplicate_actor_name",
      module: "bootstrap.app",
    });
  }
  return unique;
}

/**
* An importable prefix + transcription source for a candidate actor. If there is a lack of material in any link, it will return `undefined` (the actor will be re-run).
 *
* **Downgrade without failure** is the general tone here (as opposed to the "whole rejection" of the door): missing prefix, missing persona, no session on the chain,
* The source session is cleared and it's all just "this actor is not cached" - no cache line in the journal means there isn't one, no lie.
* The only thing that would be rejected outright is that boundary accounting is not trustworthy, because that would truncate accepted candidates to the wrong location.
 */
async function buildActorCandidate(input: {
  actor: ActorRecord;
  journal: ImportedCacheJournalReader;
  logger?: Logger;
  nodes: NodeRecord[];
  predecessorRunId: string;
  transcripts?: ActorTranscriptStore;
}): Promise<ImportedActorCandidate | undefined> {
  const { actor, journal, logger, nodes, predecessorRunId, transcripts } = input;
  const name = actor.name!;

// persona is a frozen identity that the engine synchronously drops when creatingActor, so it must be present normally; absence can only be caused by external force
// Rewritten lines. Without a comparison object, it is impossible to talk about persona consistency during runtime comparison - discard the candidate instead of taking `{}` as the top one.
  if (actor.persona === undefined) return undefined;

  const entries = completedAskPrefix(nodes, actor);
  if (entries.length === 0) return undefined;

  const source = resolveTranscriptSource({
    actorName: name,
    journal,
    startRunId: predecessorRunId,
  });
  if (source === undefined) {
// There is no ancestor in the chain that holds a session for this actor (it was never created, or the session has been cleaned up). Full fidelity transcription is a feature of this
// Verdict, without transcription there is no continuation - downgraded to a completely new actor.
    logger?.info?.("Dynamic workflow amend: no transcript source for actor, import dropped", {
      actorName: name,
      event: "dynamic_workflow.amend.transcript_source_missing",
      module: "bootstrap.app",
      runId: predecessorRunId,
    });
    return undefined;
  }

  const boundary = entries[entries.length - 1]!.messageBoundary;
  if (
    transcripts !== undefined &&
    !(await honorsBoundary(transcripts, source.sessionId, boundary))
  ) {
    logger?.warn?.(
      "Dynamic workflow amend: transcript source shorter than boundary, import dropped",
      {
        actorName: name,
        event: "dynamic_workflow.amend.transcript_source_short",
        messageBoundary: boundary,
        module: "bootstrap.app",
        sessionId: source.sessionId,
      },
    );
    return undefined;
  }

  return {
    persona: actor.persona,
    entries,
    transcriptSourceSessionId: source.sessionId,
    ...(source.resolvedModel === undefined ? {} : { resolvedModel: source.resolvedModel }),
  };
}

/**
* The **longest fully completed ask prefix** for this actor (continuous by actorSeq 0..k).
 *
* The prefix stops at the first non-completed position, and the three stopping methods are handled in the same way: failure, crash (running), and empty sequence number.
* A failed ask is **non-binding** to the new run (the model is random, and revisions are often just to overcome a failure), so it itself
* Does not import; but skipping it to import subsequent entries smuggles context - the Q&A for the skipped round is still in the source conversation transcript,
* And the cache claims it didn't happen. Stopping at the first non-completed word is the only self-consistent way of reading it.
 */
function completedAskPrefix(nodes: NodeRecord[], actor: ActorRecord): ImportedAskEntry[] {
  const bySeq = new Map<number, NodeRecord>();
  for (const node of nodes) {
    if (node.kind !== "ask") continue;
    if (node.actorSiteId !== actor.siteId || node.actorOrdinal !== actor.ordinal) continue;
    if (node.actorSeq === undefined) continue;
    bySeq.set(node.actorSeq, node);
  }

  const entries: ImportedAskEntry[] = [];
  for (let seq = 0; ; seq++) {
    const node = bySeq.get(seq);
    if (node === undefined || node.status !== "completed") break;
// The boundary must be present: Gate 3 has checked the entire precursor, so this is not an optimistic reading but a fulfillment of the invariant.
    const entry: ImportedAskEntry = {
      inputHash: node.inputHash,
      result: node.result,
      messageBoundary: node.messageBoundary!,
    };
    if (node.stats !== undefined) entry.stats = node.stats;
    entries.push(entry);
  }
  return entries;
}

/**
* Follow the `resumed_from` chain back to the **most recent** ancestor run that held the named actor session.
 *
* Why it is necessary to move the chain: An actor in run B hits the cache ⇒ B has never created a session for it (lazy creation), so
* When a revision of B→C is to succeed the actor, the transcription only exists in A. count boundaries are copied across prefixes unchanged, so any
* At the session ancestor, the boundary values ​​copied by B are directly available - this is the basis for chain revision.
 *
* `resolvedModel` is taken from the same line as the session: pin means "don't change the model when continuing this transcription" and is taken from another line
* is committing to a transcription that does not belong to it.
 *
* Ring defense (seen) is pure defense: supersede can only point to an earlier run that has ended, and cannot construct a ring. But if this while is true
* Encountering damaged data is an infinite loop, and the cost of defense is a Set.
 */
function resolveTranscriptSource(input: {
  actorName: string;
  journal: ImportedCacheJournalReader;
  startRunId: string;
}): { sessionId: string; resolvedModel?: string } | undefined {
  const { actorName, journal, startRunId } = input;
  const seen = new Set<string>();
  let runId: string | undefined = startRunId;

  while (runId !== undefined && !seen.has(runId)) {
    seen.add(runId);
    const matches = journal.listActors(runId).filter((actor) => actor.name === actorName);
// 0 = This actor does not exist in this generation (the chain is broken for this name); >1 = Duplicate name, session by name is a roll of the dice.
// Either way, stop here instead of going up: the session from the previous generation is not the source of this transcription.
    if (matches.length !== 1) return undefined;
    const actor = matches[0]!;
    if (actor.sessionId !== undefined) {
      return {
        sessionId: actor.sessionId,
        ...(actor.resolvedModel === undefined ? {} : { resolvedModel: actor.resolvedModel }),
      };
    }
    runId = journal.getRun(runId)?.resumedFrom;
  }
  return undefined;
}

/**
* Whether the source session actually meets the boundary (number of messages ≥ boundary).
 *
* This check connects the driver's **fail loud** semantics with the build-time **downgrade** semantics: driver's
* `seedActorTranscript` throws `DriverError` (corruption level, see property 3 there) for short sessions; downgrades to a fresh rerun when transcription is missing during build. Both are correct, but the scope is different - **foreseeable** starvation (session cleared/truncated)
* Candidates should be discarded during the build phase, so failure on the driver side degenerates into a fallback when it really shouldn't happen.
 *
* Read failures (session does not exist, etc.) are also treated as "cannot be honored": the builder is not responsible for the wrong classification of the session storage, and any
* Sources that cannot be read are not available sources.
 */
async function honorsBoundary(
  transcripts: ActorTranscriptStore,
  sessionId: string,
  boundary: number,
): Promise<boolean> {
  try {
    const messages = await transcripts.messages({ sessionID: sessionId as SessionId });
    return messages.length >= boundary;
  } catch {
    return false;
  }
}

/**
* Content table of world node: `inputHash` → Record queue arranged in journal insertion order (nth occurrence versus nth item).
 *
* The key directly uses the inputHash** recorded by the **precursor without recalculation: the engine hash caliber of `{op,args}` (engine.ts
* worldRead) is the value written into this column. Recalculating it is equivalent to copying a hash contract here, and once it drifts,
* On the surface it is "an inexplicable cache miss".
 *
* Only completed: failed world reading is re-executed (failure has no binding effect on new runs), not to mention running ones.
* world-run and world-read have the same table - importing world-run is a **safety feature** rather than an optimization: revised continuation will never be silent
* Replay journaled effects once (deployment script runs twice).
 */
function buildWorldQueues(nodes: NodeRecord[]): ReadonlyMap<string, ImportedWorldEntry[]> {
  const world = new Map<string, ImportedWorldEntry[]>();
  for (const node of nodes) {
    if (node.kind !== "world-read" && node.kind !== "world-run") continue;
    if (node.status !== "completed") continue;
    const queue = world.get(node.inputHash);
    const entry: ImportedWorldEntry = {
      inputHash: node.inputHash,
      kind: node.kind,
      result: node.result,
    };
    if (queue === undefined) world.set(node.inputHash, [entry]);
    else queue.push(entry);
  }
  return world;
}

/**
* Entry on the resume side: Rebuild the import cache of the revision run. **Any failure will only downgrade, and resume will not be refused**.
 *
* Share the same {@link buildImportedCache} with the submission side - this is not a convenience for reuse, but a prerequisite for correctness: revise the run
* The journal only contains the "arrived execution prefix". Unconsumed imports are compensated by this reconstruction. Different tables are calculated on both sides. This means
* "Rebuild" became "Create another one".
 *
* Three reasons make "rebuild failed" and "build failed on commit" completely different:
* - Revision run already exists. Rejecting resume is equivalent to turning a resumable run into a permanently stuck run;
* - Consumed hits are **true rows** in this run's journal, replay does not need this table - the run's self-containment does not depend on it;
* - Unconsumed imports degenerate into live re-executions, the results are correct, and only the tokens that could have been saved are spent.
 *
* So there is no distinction between the three reasons here: for resume `run_not_found` (the predecessor is cleared) and
* `missing_boundaries` is the same thing - "no cache available this time". The predecessor journal is therefore the continuation of the revision run
* Depends on, but only accelerates the structure**: it becomes more expensive if lost, but does not cause errors. Make a note of info so you can explain the bill later.
 */
export async function rebuildImportedCacheForResume(
  deps: AmendImportDeps,
/** The resumed revision run with its `resumed_from` (the caller has acknowledged the presence of the latter). */
  run: { runId: string; predecessorRunId: string },
): Promise<ImportedRunCache | undefined> {
  const { predecessorRunId, runId } = run;
  const built = await buildImportedCache(deps, predecessorRunId);
  if (built.ok) return built.cache;
  deps.logger?.info?.("Dynamic workflow amend cache rebuild skipped; resuming without it", {
    event: "dynamic_workflow.amend.rebuild_skipped",
    module: "bootstrap.app",
    reason: built.reason,
    resumedFrom: predecessorRunId,
    runId,
  });
  return undefined;
}
