/**
* Parent process harness (host side of Boundary A + sandbox process orchestration).
 *
* Responsibility: Run a workflow script (or lowered function body) in a controlled sub-process and use NDJSON to bridge it
* The child process's `__host.*` calls to a {@link WorkflowEngine} instance and ultimately returns the engine's {@link RunSettlement}.
* This package **only** relies on `@zcode/dynamic-workflow` and node built-in - proving that the entire pipeline can run app-free,
* Never import `@zcode/core`/`@zcode/contracts`/`@zcode/bootstrap`/`@zcode/adapters`.
 *
* Timing (happy path):
 *
 *   parent                         child(vm)
* │ write <cwd>/.zcode/workflow-runs/<runId>.mjs (payload: lowered+args embedded)
 *     │  spawn(node <entry>)
 *     │──────────────────────────▶│  build __host in context
* │◀── create-actor(local#1) ──│ createActor returns local#1 synchronously
 *     │  map local#1 → ActorId     │
 *     │◀── request ask(local#1) ───│  await __host.ask(...)
 *     │  engine.ask → driver.startAsk … settle
 *     │── response(value) ────────▶│  resolve
* │◀────── complete(ok,value) ──│ script return
 *     │  engine.complete(value) → settled=completed
 *
* Failure/cancellation: The decision of the run belongs to the engine. Termination failed (script threw error-complete, child process crashed/non-zero exit,
* Wall clock timeout, subprocess line JSON parsing failure) are called `engine.fail(error)` (settlement failed + journal failure_json);
* The abort signal is the only "true cancellation", adjust `engine.stop(initiator)` (clearance stopped; `signal.reason` is
* `"model"` means the main agent TaskStop, `"interrupted"` means stopping its own run when the host App is closed, otherwise it is counted as a user). The engine's own run level failed
* (reportCap/inputHash/unknownActor) also appears via engine.settled. first-wins on harness side
* finalize only cleans up the child process (clearing timer, turning off stdin, killing child), and does not create its own settlement.
 */

import { spawn, type ChildProcess } from "node:child_process";
import { createInterface, type Interface } from "node:readline";
import {
  lowerWorkflowScript,
  WorkflowEngine,
  WorkflowError,
  type ActorId,
  type AskSpec,
  type Caps,
  type ImportedRunCache,
  type RunSettlement,
  type ValidateFn,
  type WorkflowDriver,
  type WorkflowReportSink,
} from "@zcode/dynamic-workflow";
import { type ChildMessage, type ChildPayload, type ResponseMessage } from "./protocol.js";
import { renderChildEntry } from "./child-source.js";
import { writeChildEntryFile, type HarnessWarning } from "./child-entry-file.js";

/** Driver factory: The harness first builds the sink (the upward return side of the engine), gives it to the factory to build the driver, and then uses the driver to build the engine. */
export type DriverFactory = (sink: WorkflowReportSink) => WorkflowDriver;

/**
* Input parameters of {@link runWorkflowScript}. The execution body comes from `scriptText` or `lowered` (the latter takes precedence);
* Both give the form of "compile once" at the same time - the caller compiles it by himself and gets `lowered`, and at the same time, the author writes
* `scriptText` is handed over and dropped into the library.
 *
* **harness never checks whether this pair is self-consistent**: it cannot be checked (whether `lowered` is really determined by `scriptText`
* Downgrading is equivalent to running the compilation again, and that is the time that "compile once" will omit). Hand in a copy that doesn't match
* combination, the `script_text` dropped into the library will be inconsistent with the actual executed code, and the comparison benchmark of resume will be distorted——
* This self-consistency belongs to the caller.
 */
export interface RunWorkflowOptions {
  /**
* The workflow script source code written by the author has two uses:
* 1. `lowered` defaults to harness lower to get the execution body;
* 2. **Always** falls into `dwf_run.script_text` as run metadata (the comparison benchmark of resume is the author’s original text,
* is not a lowered function body).
   */
  scriptText?: string;
  /**
* Lowered async function body (free identifier only `__host`). Bypass the compiler and feed the sandbox directly——
* Use it for security testing to feed handwritten lowered bodies (the vm contract itself is the object under test), and use it for compiled paths
* Hand over the product of your compilation. When given, takes precedence over `scriptText` as the executable body.
   */
  lowered?: string;
  runId?: string;
/** driver factory (see {@link DriverFactory}). The driver comes with journal and emit. */
  makeDriver: DriverFactory;
  caps: Caps;
/** Static specs for each ask site. **Every ask site in the script must be overridden** - the engine treats absences as hard failures as wiring errors. */
  askSpecs: ReadonlyMap<string, AskSpec>;
/** Injected schema validator (engine does not import schema implementation). */
  validate: ValidateFn;
/** External cancellation signal: abort ask on the fly and kill the child process, run clear cancelled. */
  signal?: AbortSignal;
/** Wall clock timeout (ms): At the point where the child process is killed, run settlement failed. The default is no limit. */
  timeoutMs?: number;
/** The upper limit of the child process heap (MB), mapped to `--max-old-space-size`. Default 256. */
  maxOldSpaceSizeMb?: number;
  /**
* Alternative form of child process spawn strategy: when given spawn `process.execPath [...argsPrefix, <entry path>]`,
* **Without any Node CLI flags**.
   *
* Reason for existence: SEA single-file binary does not interpret Node CLI flags, the default path's `--max-old-space-size` will be used as
* Ordinary tokens that fall into the CLI's strict parseArgs will inevitably exit with an error (each workflow run fails under SEA). SEA under
* The hidden subcommand name is passed in as argsPrefix by bootstrap, and the subprocess will re-exec the binary automatically before parseArgs
* `import()` entry file, call its `start`.
   *
* Cost: The heap upper limit can only be set by the entry file itself `v8.setFlagsFromString` best-effort.
   */
  childSpawn?: { argsPrefix: readonly string[] };
  /**
* Reporting port for non-fatal situations (there is only one type today: the entry file cannot be written into the project `.zcode/` and falls back to the OS temporary directory).
   * harness 是 app-free 的，没有 logger；bootstrap 把它接到自己的 warn 日志。
   */
  onWarning?: (warning: HarnessWarning) => void;
  /** 子进程工作目录，缺省 process.cwd()；同时作为 run 元数据落 dwf_run.cwd。 */
  cwd?: string;
  /**
   * run 元数据，**原样**转交 `EngineConfig`（createRun 落 dwf_run）。全可选，harness
   * 不加工也不推断：`scriptHash` **由调用方计算**。harness 若哈希"它看到的文本"，在
   * lowered 路径上落库的就是 lowered 函数体的哈希——resume 校验会拿到一个静默错误的
   * 比对对象。脚本原文走上面的 `scriptText`。
   */
  scriptHash?: string;
  parentSessionId?: string;
  /** run 的展示名（宿主枚举面的标签来源）。与 `scriptText` 同路，harness 只转交。 */
  name?: string;
  /**
   * 本次 run 的实参（saved workflow 的声明式参数，已由调用方校验并回填默认值）。
   *
   * **两个去处**，这是它与其余元数据的不同之处：既随 `EngineConfig` 落 `dwf_run.args_json`
   * （resume 从那里读回重放），也进 spawn payload 注入沙箱成为冻结的 `args` 全局。harness
   * 不校验、不加工——声明与校验都在调用方（工具侧）。缺席即 `{}`。
   */
  args?: Record<string, unknown>;
  /** 发起 run 的 CreateWorkflow 工具调用 id（重启后 join/通知锚点）；verbatim 转交 EngineConfig。 */
  toolCallId?: string;
  /**
   * 修订续跑（amend-resume）的一对入参，与其余元数据同规：**verbatim 转交 EngineConfig**，
   * harness 不读、不加工、不推断。`resumedFrom` 是落 `dwf_run.resumed_from` 的 lineage 指针，
   * `importedCache` 是注入引擎的纯数据缓存表。
   *
   * 与 `scriptText`/`lowered` 那一对同样的分工：**两者的一致性归调用方所有**。harness 无从判断
   * 这张表是否真由那个前驱 run 构建而来（那要求它自己去读 journal，而本包连存储都不认识）。
   * 递进来一张对不上的表，得到的就是一个把别人的答案当成自己缓存的 run。构建与门都在 run
   * service（bootstrap 的 dynamic-workflow-import.ts）。
   */
  resumedFrom?: string;
  /**
   * 发起 run 那一轮的锚点；verbatim 转交 EngineConfig。
   * `subagentModel` 与锚点、阶段表同车：本 run 子代理的规范 picker 串
   * （`providerId/modelId[$reasoningLevel]`），harness 同样不读、不解析、不加工——它零 SQL 地
   * 活在 `run-launched` 事件里，解析与优先级都在宿主侧（bootstrap 的 workflow-actor-model.ts）。
   * `phaseAlongside` 与 `phaseNames` 按位置对齐（下标指向同一张表），同车同规。
   */
  launch?: {
    inputId: string;
    phaseNames?: string[];
    subagentModel?: string;
    phaseAlongside?: number[][];
  };
  importedCache?: ImportedRunCache;
  /**
   * 建 run 时的用量起点（前驱 run 的 `spentTokens`）；与其余元数据同规：**verbatim 转交
   * EngineConfig**，harness 不读、不加工。读前驱的行发生在 run service。
   */
  inheritedTokens?: number;
}

const DEFAULT_MAX_OLD_SPACE_MB = 256;
const STDERR_LIMIT = 64 * 1024;

/**
 * 跑一份 workflow 脚本至结算。返回引擎的 {@link RunSettlement}。
 * 失败一等公民，但分两类：脚本抛错是脚本之错，
* Normalized to `{status:"errored", error}`; the child process cannot start / crashes / wall clock timeout / protocol damage is a host-side fault,
* It is possible to just rerun, and return it to `{status:"stopped", reason:"interrupted", error}` (can be resumed).
* Neither is swallowed in silence.
 */
export async function runWorkflowScript(options: RunWorkflowOptions): Promise<RunSettlement> {
  const lowered = resolveLowered(options);
  const runId = options.runId ?? "run";

// Sink late binding: The engine is constructed after the driver, but the driver needs the sink to return - use a forward proxy to break the ring dependency.
  let engine!: WorkflowEngine;
  const sink: WorkflowReportSink = {
    askSubmitAttempted: (instance, payload) => engine.askSubmitAttempted(instance, payload),
    askTurnEnded: (instance, finalText) => engine.askTurnEnded(instance, finalText),
    askProgress: (instance, progress) => engine.askProgress(instance, progress),
    askStats: (instance, stats) => engine.askStats(instance, stats),
    askFailed: (instance, error) => engine.askFailed(instance, error),
// Deterministic model side error → The entire run stops, and it is also only forwarded.
    stopRun: (error) => engine.stopRun(error),
// The three pure observations and run-level stagnant observations of adaptive concurrency are also only forwarding.
    askWaiting: (instance, info) => engine.askWaiting(instance, info),
    askExecuting: (instance) => engine.askExecuting(instance),
// The only observation that allows the engine to make decisions (off import cache) is also just forwarding.
    askMutating: (instance) => engine.askMutating(instance),
    concurrencyChanged: (change) => engine.concurrencyChanged(change),
    runStalled: (info) => engine.runStalled(info),
  };
  const driver = options.makeDriver(sink);
  engine = new WorkflowEngine({
    runId,
    driver,
    caps: options.caps,
    askSpecs: options.askSpecs,
    validate: options.validate,
// Metadata verbatim transfer, default remains absent (undefined key is not implemented). cwd is actually used by the child process
// is the same value, so the bottom line of process.cwd() is also recorded here - the cwd dropped into the library is the directory where run actually runs.
    ...(options.scriptText === undefined ? {} : { scriptText: options.scriptText }),
    ...(options.name === undefined ? {} : { name: options.name }),
    ...(options.args === undefined ? {} : { args: options.args }),
    ...(options.scriptHash === undefined ? {} : { scriptHash: options.scriptHash }),
    ...(options.parentSessionId === undefined ? {} : { parentSessionId: options.parentSessionId }),
    ...(options.toolCallId === undefined ? {} : { toolCallId: options.toolCallId }),
    ...(options.resumedFrom === undefined ? {} : { resumedFrom: options.resumedFrom }),
    ...(options.launch === undefined ? {} : { launch: options.launch }),
    ...(options.importedCache === undefined ? {} : { importedCache: options.importedCache }),
    ...(options.inheritedTokens === undefined ? {} : { inheritedTokens: options.inheritedTokens }),
    cwd: options.cwd ?? process.cwd(),
  });

  const maxOldSpaceSizeMb = options.maxOldSpaceSizeMb ?? DEFAULT_MAX_OLD_SPACE_MB;
  const cwd = options.cwd ?? process.cwd();
  const payload: ChildPayload = {
    lowered,
//The actual parameter only crosses the bounds once at startup (it is a constant during the run life cycle).
    ...(options.args === undefined ? {} : { args: options.args }),
//The upper limit of the heap is determined by the payload entry file and only serves the argsPrefix path: there is no Node flag to pass on that path.
//The entry file can only be set by best-effort yourself. The default path is taken into effect by the true flag, and the entry file is skipped when it sees the flag.
    maxOldSpaceSizeMb,
  };

// payload is limited to the command line: the Windows command line limit is 32,767 characters, and the entire lowered script is about 18 KB
// Just `spawn ENAMETOOLONG`. Written as an entry file,
// argv has only one path left, and its length has nothing to do with the script size.
  let child: ChildProcess;
  try {
    const entry = writeChildEntryFile({
      cwd,
      runId,
      source: renderChildEntry(payload, { runId }),
      ...(options.onWarning === undefined ? {} : { onWarning: options.onWarning }),
    });
    child = spawn(
      process.execPath,
      options.childSpawn === undefined
        ? [`--max-old-space-size=${maxOldSpaceSizeMb}`, entry.path]
        : [...options.childSpawn.argsPrefix, entry.path],
      {
        cwd,
        stdio: ["pipe", "pipe", "pipe"],
// The desktop agent is run by the Electron Helper (process.execPath points to the Helper), and the CLI
// ELECTRON_RUN_AS_NODE will be sanitized from its own env during startup. If you do not bring it explicitly, the child process will press the complete
// Electron/Chromium application starts and gets stuck on GPU initialization - always silent and never exits, run gets stuck on run-started.
// This variable is invalid under pure Node's execPath and has no side effects (same processing as official-plugin-runtime.ts).
//Both spawn strategies are required: Desktop packaging state may also use the argsPrefix path.
        env: { ...process.env, ELECTRON_RUN_AS_NODE: "1" },
      },
    );
  } catch (cause) {
// Node only converts EACCES/EAGAIN/EMFILE/ENFILE/ENOENT into error
//Event, other spawn failed (ENAMETOOLONG, E2BIG...) **synchronous throw**. At this point the engine has been constructed and the journal line has been
// Running is dropped; letting the exception pop up directly will bypass the engine settlement - the registry is recorded as final, but the journal line is forever
// running, subsequent resume_from is rejected by "has not settled yet". The entry file cannot be written (both directories are
// failed) the same way. All resolved by the engine stopped (interrupted): Host side failure, can be resumed.
    const message = cause instanceof Error ? cause.message : String(cause);
    engine.stop(
      "interrupted",
      new WorkflowError("Interrupted", `Could not start the workflow sandbox process: ${message}`, {
        cause,
      }),
    );
    return engine.settled;
  }

  return bridge({ child, engine, runId, signal: options.signal, timeoutMs: options.timeoutMs });
}

/** Parse lowered body: `lowered` is preferred, otherwise lower `scriptText` (dirty script throws an error and does not start the child process). */
function resolveLowered(options: RunWorkflowOptions): string {
  if (options.lowered !== undefined) return options.lowered;
  if (options.scriptText === undefined) {
    throw new Error("runWorkflowScript: pass either scriptText or lowered");
  }
  const result = lowerWorkflowScript(options.scriptText);
  if (!result.ok || result.lowered === undefined) {
    const detail = result.diagnostics.map((d) => `${d.line}:${d.column} ${d.message}`).join("; ");
    throw new Error(
      `runWorkflowScript: the script failed compilation/analysis and is not run degraded: ${detail}`,
    );
  }
  return result.lowered.code;
}

interface BridgeDeps {
  child: ChildProcess;
  engine: WorkflowEngine;
/** Only used for error text in abort normalization (the "interrupted" one should specify which run was shut down). */
  runId: string;
  signal?: AbortSignal;
  timeoutMs?: number;
}

/**
* NDJSON bridging + lifecycle convergence. single-writer finalize: regardless of engine settlement, child process exit, timeout, or
* When abort is reached, the first one to arrive wins, and then kills the child process and honors the result.
 */
function bridge(deps: BridgeDeps): Promise<RunSettlement> {
  const { child, engine, runId, signal, timeoutMs } = deps;

// mapping of local#N (child-local handle) → engine ActorId; stdio FIFO + synchronous create-actor handling guarantees
// The map is ready before any ask that references a handle arrives.
  const actorMap = new Map<string, ActorId>();
  let stderr = "";
  let finalized = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let reader: Interface | undefined;

  return new Promise<RunSettlement>((resolve) => {
    const finalize = (settlement: RunSettlement): void => {
      if (finalized) return;
      finalized = true;
      if (timer !== undefined) clearTimeout(timer);
      if (signal !== undefined) signal.removeEventListener("abort", onAbort);
      reader?.close();
      if (child.exitCode === null && child.signalCode === null) child.kill();
      resolve(settlement);
    };

// Termination failures (script throw error / child process crash / timeout / protocol damage) are all run failures: handed over to the public fail() of the engine,
// Settlement by first-wins, driver side cancellation on the fly ask, journal record failed + failure_json——run decision
// Owned by the engine, the journal does not diverge from the results seen by the caller. Child process cleanup is handled by finalize (via engine.settled).
    const failRun = (error: WorkflowError): void => {
      if (finalized) return;
      engine.fail(error);
    };
// Host-side failure (sandbox crash/timeout/protocol damage): stopped (interrupted), can be resumed.
    const interruptRun = (message: string, cause?: unknown): void => {
      if (finalized) return;
      engine.stop(
        "interrupted",
        new WorkflowError("Interrupted", message, cause === undefined ? undefined : { cause }),
      );
    };

// abort is the only "true cancellation": go to engine.stop(initiator) and settle stopped. initiator
// `AbortController.abort(reason)` comes from: literal "model", amend path
// `{ superseded: newRunId }` (successor id will be stored with the same reason), literal "interrupted" (host App
// Close, see below), the rest are all "user".
    function onAbort(): void {
      if (finalized) return;
      const reason: unknown = signal?.reason;
      const supersededBy = readSupersededBy(reason);
      if (supersededBy !== undefined) {
        engine.stop("superseded", undefined, supersededBy);
        return;
      }
// The host actively stops its own run (close() of run service). It is not user cancelled: Failed encoding with Interrupted dropped library, with timeout /
// Sandbox crashes are in the same family - stopped(interrupted) can be resumed, while stopped(user) is read on the UI as
// "The user pressed stop". The error text names the run and the cause, and the details page will interpret this line according to it the next time it is activated.
      if (reason === "interrupted") {
        engine.stop(
          "interrupted",
          new WorkflowError(
            "Interrupted",
            `dynamic workflow run ${runId} was interrupted: the owning session closed before the run settled`,
          ),
        );
        return;
      }
      engine.stop(reason === "model" ? "model" : "user");
    }

// All settlements (complete/fail/stop/engine internal cap failure) emerge to finalize through engine.settled.
    void engine.settled.then(finalize);

    if (signal !== undefined) {
      if (signal.aborted) {
        onAbort();
        return;
      }
      signal.addEventListener("abort", onAbort, { once: true });
    }

    if (timeoutMs !== undefined) {
      timer = setTimeout(() => {
        interruptRun(
          `The workflow sandbox process exceeded its wall-clock timeout of ${timeoutMs}ms`,
        );
      }, timeoutMs);
      timer.unref?.();
    }

    child.stderr?.on("data", (chunk: Buffer) => {
      stderr = trimTail(stderr + chunk.toString("utf8"), STDERR_LIMIT);
    });
// There may still be a response to be written after the child process exits: hang error monitoring on stdin to prevent EPIPE from causing uncaught exceptions.
    child.stdin?.on("error", () => undefined);

    reader = createInterface({ input: child.stdout! });
    reader.on("line", (line) => {
      if (line.trim().length === 0) return;
      let message: ChildMessage;
      try {
        message = JSON.parse(line) as ChildMessage;
      } catch (cause) {
// Child process row JSON corruption: exposed instead of swallowed.
        interruptRun(
          `The workflow sandbox process emitted a line that is not valid NDJSON: ${line}`,
          cause,
        );
        return;
      }
      handleChildMessage(message, { engine, actorMap, child, failRun });
    });

    child.on("error", (error) => {
      interruptRun(`Could not start the workflow sandbox process: ${error.message}`, error);
    });

    child.on("close", (code, sig) => {
      if (finalized) return;
// Child process exited but engine not settled: crashed/killed without complete. Attributed to stderr.
      const reason =
        stderr.trim().length > 0
          ? stderr.trim()
          : `The workflow sandbox process exited (code=${code}, signal=${sig}) before completing`;
      interruptRun(reason);
    });
  });
}

interface MessageDeps {
  engine: WorkflowEngine;
  actorMap: Map<string, ActorId>;
  child: ChildProcess;
  failRun: (error: WorkflowError) => void;
}

/** Distributes a child→parent message. ask/world-read asynchronously bridges to the engine and responds (with the latest budget). */
function handleChildMessage(message: ChildMessage, deps: MessageDeps): void {
  const { engine, actorMap, child, failRun } = deps;

  switch (message.kind) {
    case "create-actor": {
// Synchronous processing: Build the mapping before any ask that references the handle (stdio FIFO prerequisite). createActor is purely synchronous,
// If the run has been settled, an error will be thrown synchronously - there is no response channel at this time, and the run will be classified as a failure after capture (mostly a harmless ending race condition).
      try {
        const actorId = engine.createActor(
          message.siteId,
          message.name,
          message.persona as string | undefined,
        );
        actorMap.set(message.localId, actorId);
      } catch (cause) {
        failRun(
          cause instanceof WorkflowError
            ? cause
            : new WorkflowError("DriverError", `createActor failed at site ${message.siteId}`, {
                cause,
              }),
        );
      }
      return;
    }
    case "event":
// Synchronous dispatch, consistent with log: the FIFO order of the event channel is load-bearing for both report and declare-artifact
// (The journal of the parent process is what arrives, and a tagged report must be dropped later than its declaration),
// Asynchronization will decouple the arrival order from the journal order.
      if (message.type === "report") {
        engine.report(message.siteId, message.item, message.artifactId);
      } else if (message.type === "declare-artifact") {
        engine.declareArtifact(message.siteId, message.op, message.args);
      } else if (message.type === "phase-entered") {
        engine.enterPhase(message.name);
      } else {
        engine.log(message.message);
      }
      return;
    case "complete":
      if (message.ok) {
        engine.complete(message.value);
      } else {
// The script throws an error: run failed (error details come from the sandbox).
        const err = message.error;
        failRun(
          new WorkflowError("DriverError", err?.message ?? "The workflow script threw an error", {
            cause: err?.stack ?? err?.name,
          }),
        );
      }
      return;
    case "request":
      handleRequest(message, { engine, actorMap, child, failRun });
      return;
    default: {
      const _exhaustive: never = message;
      void _exhaustive;
    }
  }
}

/** Bridge a host call that needs to be answered (ask / world-read) to the engine, and return the response after settling. */
function handleRequest(
  message: Extract<ChildMessage, { kind: "request" }>,
  deps: MessageDeps,
): void {
  const { engine, actorMap, child } = deps;

  const respond = (ok: boolean, value: unknown, error?: WorkflowError): void => {
    const response: ResponseMessage = {
      kind: "response",
      id: message.id,
      ok,
      ...(ok ? { value } : { error: toWireError(error) }),
    };
// The child process may have exited (cancelled/failed to end): only write when writable, I/O races such as EPIPE are swallowed at this boundary (run is already settling).
    const stdin = child.stdin;
    if (stdin === null || !stdin.writable) return;
    stdin.write(`${JSON.stringify(response)}\n`, () => undefined);
  };

  let promise: Promise<unknown>;
  if (message.type === "ask") {
    const actorId = actorMap.get(message.actor ?? "");
    if (actorId === undefined) {
// Missing mapping (should not happen: FIFO guarantee) - normalizes to UnknownActor structured rejection, not silent.
      respond(
        false,
        undefined,
        new WorkflowError("UnknownActor", `Unknown subagent handle: ${message.actor}`),
      );
      return;
    }
    promise = engine.ask(message.siteId, actorId, message.instructions ?? "");
  } else if (message.type === "publish-artifact") {
    const op = message.artifactOp;
    if (op === undefined) {
// Missing op is a wiring error (lowering always fill it in). **Do not program a default value**: one is treated as file
// Markdown publishing, errors will appear far away from the point of failure. Normalized into structured rejection, the script is visible.
      respond(
        false,
        undefined,
        new WorkflowError(
          "DriverError",
          `publish-artifact request is missing artifactOp (site ${message.siteId})`,
        ),
      );
      return;
    }
    promise = engine.publishArtifact(message.siteId, op, message.args ?? []);
  } else {
// Op/args are handed over to the engine as is: this layer does not look at the op or check the byte (that is the responsibility of the driver). Missing args are normalized to an empty array,
// Let the driver's actual parameter validation reject it out loud instead of quietly coding a default value here.
    promise = engine.worldRead(message.siteId, message.op ?? "read", message.args ?? []);
  }

  promise.then(
    (value) => respond(true, value),
    (cause: unknown) => {
      const error =
        cause instanceof WorkflowError
          ? cause
          : new WorkflowError(
              "DriverError",
              cause instanceof Error ? cause.message : String(cause),
              { cause },
            );
      respond(false, undefined, error);
    },
  );
}

/** WorkflowError → Line form (code/violations/finalText is reserved for structured processing by sandbox script try/catch). */
function toWireError(error: WorkflowError | undefined): ResponseMessage["error"] {
  if (error === undefined) return { name: "Error", message: "unknown error" };
  const wire: NonNullable<ResponseMessage["error"]> = {
    name: error.name,
    message: error.message,
    code: error.code,
  };
  if (error.violations !== undefined) wire.violations = error.violations;
  if (error.finalText !== undefined) wire.finalText = error.finalText;
  return wire;
}

/** Keep the content within limit bytes at the end of the string (stderr truncated). */
function trimTail(value: string, limit: number): string {
  if (Buffer.byteLength(value, "utf8") <= limit) return value;
  return value.slice(-limit);
}

/** When abort reason is `{ superseded: <runId> }`, the successor id is taken out; other shapes return undefined. */
function readSupersededBy(reason: unknown): string | undefined {
  if (typeof reason !== "object" || reason === null) return undefined;
  const value = (reason as { superseded?: unknown }).superseded;
  return typeof value === "string" && value.length > 0 ? value : undefined;
}
