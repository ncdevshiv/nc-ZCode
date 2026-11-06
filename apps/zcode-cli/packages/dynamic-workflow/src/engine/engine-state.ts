/**
* engine.ts reaches the upper limit of oxlint max-lines (400 lines) and splits the explicit seams of the engine's private state into this file;
* The public side is still exported from engine.ts.
 *
* Several groups of methods of WorkflowEngine (user interface products, report, world node and import cache, run settlement) are split into brothers
* Free functions in modules (engine-artifacts.ts / engine-report.ts / engine-world.ts / engine-settlement.ts).
* They do not hold state individually, but read and write the private fields of the engine through this seam: the seam is closed by the engine with an arrow in the constructor
* Assembling, the field itself is still private, so the public side of the engine has zero changes; only a thin layer of delegate methods is left on the class.
 *
* Variable scalars (report counts, import gates) are exposed with "read method + one-way write method" instead of getter/setter - the writing method is only
* One type (count only increases, door only closes), this is clearly stated on the seam surface.
 */

import type { ImportedWorldQueue } from "./imported-cache.js";
import type { ArtifactOp } from "../facade/registry.js";
import type {
  ImportedRunCache,
  JournalStorePort,
  RunEvent,
  RunStopReason,
  WorkflowDriver,
  WorkflowError,
} from "./types.js";

/**
* Final settlement of run: `completed` (script return)/
* `errored` (script error, cannot be resumed) / `stopped` (stopped, can be resumed; `error` is only for
* `provider` / `interrupted` present).
 */
export type RunSettlement =
  | { status: "completed"; artifact: unknown }
  | { status: "errored"; error: WorkflowError }
  | { status: "stopped"; reason: RunStopReason; supersededBy?: string; error?: WorkflowError };

/**
* The status of a user interface product id in this run: which member type it belongs to, how many versions it has been successful in, (only preset) its
* Standardize spec. All three are derived from journal rows, so resume reconstruction and live accounting result in the same table.
 */
export interface ArtifactIdState {
  kind: ArtifactOp;
/** Number of successful versions (+1 for each successful content member; preset declaration is always 1). */
  versions: number;
/** CanonicalJson of the preset spec (the idempotent judgment of repeated declarations is compared against it). Content member absent. */
  spec?: string;
/** This id is the deliverable (primary) of run. Full run with at most one id; rebuild from completed line. */
  primary?: true;
}

/** The seam of engine private state: free functions in sibling modules read and write the private fields of WorkflowEngine through it. */
export interface EngineState {
  readonly runId: string;
  readonly driver: WorkflowDriver;
  readonly journal: JournalStorePort;
  /**
* The status of each **user interface product** id of this run. resume from journal
* The `kind: "artifact"` line is rebuilt and thereafter maintained in memory - the upper limit and version number are run-level facts, continuous across resumes.
   *
* ⚠ Terminology: artifact = user surface artifact, not `RunSettlement.artifact` (top-level return value).
   */
  readonly artifacts: Map<string, ArtifactIdState>;
/** The import cache of amend-resume (pure data, absent means this is not a revised continuation). */
  readonly importedCache: ImportedRunCache | undefined;
/** World imports the consumption cursor of the queue (nth occurrence vs. nth item). */
  readonly importedWorld: ImportedWorldQueue;

  isRunSettled(): boolean;
/** Error used to reject / throw when run has settled. */
  runError(): WorkflowError;
/** run-level failure (first-wins; see settleFailed in engine-settlement.ts). */
  failRun(error: WorkflowError): void;
/** Events both fall into the journal and fan out (Boundary C); the birth stage is made up in the engine's funnel. */
  record(event: RunEvent): void;
/** The unique casting point of the site serial number. */
  nextOrdinal(siteId: string): number;

/** The number of reports published by this run (counter of REPORT_CAPS.maxItemsPerRun, continuous across resume). */
  reportCount(): number;
  /** 一条报告过了上限检查、即将落库：计数 +1。 */
  countReport(): void;

  /** 导入缓存是否已关闭。 */
  importClosed(): boolean;
  /** 关门（永不重开）。 */
  closeImport(): void;

  /** 置 run 为已结算；带 failure 时同时记下 run 级失败原因（供 runError 复用）。 */
  markSettled(failure?: WorkflowError): void;
  /** 中止所有在飞 ask（委托调度器）。 */
  abortInFlight(error: WorkflowError, emitCancelled: boolean): void;
  /** 兑现 `engine.settled`。 */
  resolveSettled(settlement: RunSettlement): void;
}
