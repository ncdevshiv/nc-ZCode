/**
 * engine.ts 顶到 oxlint max-lines 上限（400 行），把 world 节点（world-read / world-run）
 * 的准入-结算路径与 amend-resume 导入缓存的关门 / 恢复接缝拆到本文件；公开面仍从 engine.ts 导出。
 *
 * 自由函数经 {@link EngineState} 接缝读写引擎状态；WorkflowEngine.worldRead 只是薄委托，
 * 关门由调度器经 SchedulerHost 触发，恢复在引擎构造函数的 resume 分支里调用。
 */

import { canonicalJson, inputHash } from "./hash.js";
import { boundWorldReadInput } from "./world-read-input.js";
import { hashMismatch } from "./scheduler.js";
import type { EngineState } from "./engine-state.js";
import type {
  InstanceRef,
  JournalStorePort,
  NodeKind,
  WorkflowErrorJson,
  WorldReadInput,
  WorldReadOp,
} from "./types.js";
import { refToString, WorkflowError } from "./types.js";

export function readWorld(
  state: EngineState,
  siteId: string,
  op: WorldReadOp,
  args: unknown[],
): Promise<unknown> {
  if (state.isRunSettled()) return Promise.reject(state.runError());
  const ordinal = state.nextOrdinal(siteId);
  const instance: InstanceRef = { siteId, ordinal };
  // inputHash 覆盖 `{op, args}`（而非单实参的 `{op, arg}`）。这是**有意的载荷形变**：
  // 旧 journal（多参 world-read 之前写下的）里，每个 world-read 命中都会 inputHash 不符而让 run 以
  // InputHashMismatch 大声失败。可接受——v1 的 resume 本就要求脚本文本逐字节相同
  // （script_hash），facade 表面早于多参 world-read 的脚本不会是要被 resume 的那个脚本。
  // 写在这里是因为故障点在 resume，离这一行很远。
  const hash = inputHash({ op, args });
  // 有界的 `{op, args}`：与 hash 同一次准入写下，
  // 并在结算的 upsert 里原样带过去——putNode 是整条替换，漏一处就把它抹回 NULL。
  const input = boundWorldReadInput(op, args);

  const recorded = state.journal.getNode(state.runId, siteId, ordinal);
  if (recorded !== undefined) {
    if (recorded.inputHash !== hash) {
      const err = hashMismatch(instance, recorded.inputHash, hash);
      state.failRun(err);
      return Promise.reject(err);
    }
    // 完结命中短路（journal 化世界读取使 resume 免疫于 run 与 resume 之间的磁盘变化）。
    if (recorded.status === "completed") {
      state.record({ type: "node-settled", instance, outcome: "ok", cached: true });
      return Promise.resolve(recorded.result);
    }
    if (recorded.status === "failed") {
      state.record({
        type: "node-settled",
        instance,
        outcome: "failed",
        cached: true,
        error: recorded.error,
      });
      return Promise.reject(WorkflowError.fromJSON(recorded.error!));
    }
    // status === "running"：崩溃于执行中，落到下面重新 live 执行。
  }

  // op "run" 是效应而不是读，journal 单列一种（world-run）：机制同构，审计面诚实。其余 op 保持 world-read。
  const kind: NodeKind = op === "run" ? "world-run" : "world-read";

  // amend-resume：本 run 的 journal 里没有这一行，才轮到导入缓存（journal replay 永远优先——
  // 修订 run 自己崩溃后 resume 时，已消费的命中早已是真行，重放它们不能再动游标）。
  // 命中即写一行与 settleWorldRead 同形的 completed 记录 + 发 node-settled(cached)，
  // **不调 driver**：world-run 的效应绝不静默重放。
  // 缓存关闭后不再问表（也不推进游标）：命令文本没变，但它读到的世界可能已被某个 live 子代理
// Rewrite.
  const imported = state.importClosed() ? undefined : state.importedWorld.take(hash);
  if (imported !== undefined) {
    state.journal.putNode({
      runId: state.runId,
      siteId,
      ordinal,
      kind,
      inputHash: hash,
      input,
      status: "completed",
      result: imported.result,
    });
    state.record({ type: "node-settled", instance, outcome: "ok", cached: true });
    return Promise.resolve(imported.result);
  }

// live world-read: not subject to actor FIFO / concurrency upper limit (harness IO).
  if (state.isRunSettled()) return Promise.reject(state.runError());
// world.run of live is a write (effect, not read): once it is executed, the workspace is different from the one left by the predecessor, so in
// Turn off the import cache before dispatching - the same rules and the same time as the subagent's rewrite tool (before action). world-read is not relevant:
// Reading doesn’t change the world.
  if (kind === "world-run") closeImportCache(state, instance, "world-run");
// Fall after admission running: Crash in the executing world read resume can be re-executed accordingly.
  state.journal.putNode({
    runId: state.runId,
    siteId,
    ordinal,
    kind,
    inputHash: hash,
    input,
    status: "running",
  });
  state.record({ type: "node-queued", instance, kind });
  state.record({ type: "node-dispatched", instance });
  return state.driver.executeWorldRead(op, args).then(
    (value) =>
      settleWorldRead(state, instance, kind, hash, input, { status: "completed", result: value }),
    (cause: unknown) => {
      const err =
        cause instanceof WorkflowError
          ? cause
          : new WorkflowError("DriverError", `World read failed: ${op} ${canonicalJson(args)}.`, {
              cause,
            });
      return settleWorldRead(state, instance, kind, hash, input, {
        status: "failed",
        error: err.toJSON(),
      });
    },
  );
}

function settleWorldRead(
  state: EngineState,
  instance: InstanceRef,
// putNode is an upsert (whole replacement). If settle is hard-coded, world-read will replace the world-run at the time of admission.
// Quietly change it back - kind must have the same origin as admission.
  kind: NodeKind,
  hash: string,
// Same reason: the bounded input written by the admission must be replaced with the entire settlement.
  input: WorldReadInput,
  outcome:
    | { status: "completed"; result: unknown }
    | { status: "failed"; error: WorkflowErrorJson },
): unknown {
  if (state.isRunSettled()) throw state.runError();
  state.journal.putNode({
    runId: state.runId,
    siteId: instance.siteId,
    ordinal: instance.ordinal,
    kind,
    inputHash: hash,
    input,
    status: outcome.status,
    ...(outcome.status === "completed" ? { result: outcome.result } : { error: outcome.error }),
  });
  if (outcome.status === "completed") {
    state.record({ type: "node-settled", instance, outcome: "ok" });
    return outcome.result;
  }
  state.record({ type: "node-settled", instance, outcome: "failed", error: outcome.error });
  throw WorkflowError.fromJSON(outcome.error);
}

/**
* Turn off import caching (idempotent). The two trigger points are both **before the first write**:
* The driver reports that a live subagent is about to execute the mutating tool (`mutating-tool`), or a `world.run` is to be executed live
* (`world-run`). The `import-cache-closed` event is only sent when this revision is run - it is resumed when resume
* The only source of truth for "the door is closed"; a non-revised run has no table to close, the flag is harmless but no event occurs.
 */
export function closeImportCache(
  state: EngineState,
  instance: InstanceRef,
  cause: "mutating-tool" | "world-run",
  actorName?: string,
): void {
  if (state.importClosed()) return;
  state.closeImport();
  if (state.importedCache === undefined) return;
  state.record({
    type: "import-cache-closed",
    instance,
    cause,
    ...(actorName === undefined || actorName === "" ? {} : { actorName }),
  });
}

/**
* When resume is resumed, the closing judgment is restored and the "ask instances that were once live" are collected.
 *
* The source of fact is this run's own event: the live node sends `node-queued` (ask with actor ref) during admission, cache
* Hits only emit `node-settled cached:true`, so "which ask was live" is an exact set. Whether the door is closed depends on
* `import-cache-closed` event decision - ask to live no longer means closing the door (it may not touch a file), only
* It is only closed when writing, and only this event is remembered for writing. Zero schema changes.
 */
export function recoverImportClosure(
  journal: JournalStorePort,
  runId: string,
): { live: ReadonlySet<string>; closed: boolean } {
  const live = new Set<string>();
  let closed = false;
  for (const { event } of journal.listEvents(runId)) {
    if (event.type === "import-cache-closed") closed = true;
    if (event.type !== "node-queued" || event.kind !== "ask") continue;
    live.add(refToString(event.instance));
  }
  return { live, closed };
}
