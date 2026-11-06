// ============================================================
// Dynamic Workflow Snippet Service (production implementation of DynamicWorkflowSnippetPort)
// ============================================================
// Execution side of EvalWorkflowSnippet: to scratch facade
// Compile once → same lowering / sandbox / world-read execution surface → memory journal → synchronous settlement.
//
//Three invariants:
// 1. **Same execution surface**. Compile createWorkflowProgram (scratch facade with the same
// FACADE_FILE_NAME injection - the identity of the facade is determined by the declaration file name, changing the name will make the site collection silently empty);
// world read goes to production executeWorldRead (true file system / true git, production caps). snippet's
// The reason for existence is to maintain authenticity, and any "almost" second implementation is what it wants to eliminate.
// 2. **Completely transient**. InMemoryJournalStore, random runId, no dwf_* lines, no background task tracker.
// Disappearing when the process exits is a contract, not a bug.
// 3. **asks are excluded at compile time**. The scratch facade does not have agent(), so the driver’s ask family method
// Unreachable; they throw DriverError just for defense in depth - runtime arrival is a wiring bug and fails loudly.

import { randomUUID } from "node:crypto";
import type {
  DynamicWorkflowSnippetEvalOptions,
  DynamicWorkflowSnippetEvalRequest,
  DynamicWorkflowSnippetEvalResult,
  DynamicWorkflowSnippetPort,
  ExecutionPort,
  FileSystemPort,
  Logger,
} from "@zcode/contracts";
import {
  buildAskSpecs,
  collectDiagnostics,
  collectSites,
  collectWorldRunCommands,
  createWorkflowProgram,
  InMemoryJournalStore,
  lowerWorkflow,
  SNIPPET_FACADE_DTS,
  validate,
  WorkflowError,
  type Caps,
  type JsonSchema,
  type RunEvent,
  type ValidateFn,
  type WorkflowDriver,
} from "@zcode/dynamic-workflow";
import { runWorkflowScript } from "@zcode/dynamic-workflow-runtime";
import { dynamicWorkflowChildSpawn } from "./dynamic-workflow-run-launch.js";
import { resolveWorkflowConcurrencyCeiling } from "./workflow-concurrency-ceiling.js";
import { executeWorldRead, type WorldReadDeps } from "./workflow-world-read.js";

/** The bounds of logs (contract constants are in eval-workflow-snippet.ts of @zcode/contracts; here to avoid reverse dependence on the tool layer). */
const MAX_LOGS = 100;
const MAX_LOG_CHARS = 2_048;
/** Top-level return value serialization upper limit. The harness does not measure the volume of the artifact (the RunSettlement is handed over as is). This is where the boundary lies. */
const MAX_ARTIFACT_BYTES = 256 * 1024;

const validateFn: ValidateFn = (schema, value) => validate(schema as JsonSchema, value);

interface DynamicWorkflowSnippetServiceDeps {
/** The file system port where files.glob / files.read / files.grep falls. */
  fileSystemPort: FileSystemPort;
/** git.* The child process execution port where world-read falls. */
  executionPort: ExecutionPort;
  logger?: Logger;
/** Inject concurrency probe, fixed for testing (floor must be 1, parallelism-2 == 0 on dual-core machines). */
  availableParallelism?: () => number;
}

/** Create snippet service. Returns an implementation of {@link DynamicWorkflowSnippetPort}. */
export function createDynamicWorkflowSnippetService(
  deps: DynamicWorkflowSnippetServiceDeps,
): DynamicWorkflowSnippetPort {
  const caps = (): Caps => {
    return {
// The concurrency upper bound is the same as the run service / manager implementation. Snippet does not have ask and does not connect to the manager.
      maxConcurrency: resolveWorkflowConcurrencyCeiling(deps.availableParallelism),
    };
  };

  return {
    async evalSnippet(
      request: DynamicWorkflowSnippetEvalRequest,
      options?: DynamicWorkflowSnippetEvalOptions,
    ): Promise<DynamicWorkflowSnippetEvalResult> {
// Compile once: a ts.Program simultaneously feeds diagnostics, site tables and lowering (run service invariant 2 of
// snippet version). Diagnostics with TS1184/index-access rewrite - readability of self-learning loops from the same source as CreateWorkflow.
      const workflow = createWorkflowProgram(request.code, { facadeDts: SNIPPET_FACADE_DTS });
      const diagnostics = collectDiagnostics(workflow.program);
      if (diagnostics.length > 0) {
        return { kind: "diagnostics", diagnostics };
      }

      const table = collectSites(workflow);
// world.run: snippet is the workbench for these calls (run through the gate logic for true commands before submitting).
// Non-literal cmd has the same diagnosis as production (authorization plane is closed at compile time); the command set is handed over to the driver for review.
      const worldRun = collectWorldRunCommands(workflow, table);
      if (worldRun.diagnostics.length > 0) {
        return { kind: "diagnostics", diagnostics: worldRun.diagnostics };
      }
      const lowered = lowerWorkflow(workflow, table);
// There is no agent() in the scratch facade, and there cannot be an ask site in the site table; an empty schema record will
// buildAskSpecs Hands over empty askSpecs (the engine's protection against hard failures of absent specs is naturally true for snippets).
      const askSpecs = buildAskSpecs(table, {});

      const logs: string[] = [];
      let logsTruncated = false;
      const captureLog = (event: RunEvent): void => {
        if (event.type !== "log") return;
        if (logs.length >= MAX_LOGS) {
          logsTruncated = true;
          return;
        }
        const message = event.message;
        if (message.length > MAX_LOG_CHARS) {
          logs.push(message.slice(0, MAX_LOG_CHARS));
          logsTruncated = true;
          return;
        }
        logs.push(message);
      };

      const worldReadDeps: WorldReadDeps = {
        fileSystemPort: deps.fileSystemPort,
        executionPort: deps.executionPort,
        cwd: request.cwd,
        declaredRunCommands: new Set(worldRun.commands),
      };
      const runId = `dwfeval-${randomUUID()}`;
      const childSpawn = dynamicWorkflowChildSpawn();

      const settlement = await runWorkflowScript({
        askSpecs,
        caps: caps(),
        ...(childSpawn === undefined ? {} : { childSpawn }),
        cwd: request.cwd,
        lowered: lowered.code,
        makeDriver: () => createSnippetDriver(worldReadDeps, captureLog),
// Same as run-launch: leave a warn when the entry file falls back to the OS temporary directory.
        onWarning: (warning) => {
          deps.logger?.warn?.("Dynamic workflow entry file fell back to the OS temp dir", {
            event: "dynamic_workflow.entry_file.fallback",
            module: "bootstrap.app",
            runId,
            ...warning,
          });
        },
        runId,
        ...(options?.signal === undefined ? {} : { signal: options.signal }),
        timeoutMs: request.timeoutMs,
        validate: validateFn,
      });

      deps.logger?.debug?.("Dynamic workflow snippet settled", {
        event: "dynamic_workflow.snippet.settled",
        module: "bootstrap.app",
        runId,
// The `status` key of the log context is a common task vocabulary, and the three final states of run start with another key.
        runStatus: settlement.status,
        traceId: request.trace.traceId,
      });

      if (settlement.status === "completed") {
        const oversize = artifactOversize(settlement.artifact);
        if (oversize !== undefined) {
          return {
            kind: "failed",
            error: {
              code: "ArtifactTooLarge",
              message:
                `The snippet's return value serializes to ${oversize} bytes, over the cap of ` +
                `${MAX_ARTIFACT_BYTES} bytes. Return a summary (a count, the first few items) and ` +
                `leave the full data to log() or to the real run.`,
            },
            logs,
            logsTruncated,
          };
        }
        return {
          kind: "completed",
          ...(settlement.artifact === undefined ? {} : { artifact: settlement.artifact }),
          logs,
          logsTruncated,
        };
      }

      if (settlement.status === "errored") {
        return {
          kind: "failed",
          error: { code: settlement.error.code, message: settlement.error.message },
          logs,
          logsTruncated,
        };
      }

// stopped: The tool call was canceled (abort signal) or the sandbox failed. Normalizing callers into structured failures - snippet
// There is no resume semantics. The next action of "stopped experiment" and "failed experiment" are the same (change it and run it again).
// Stop with failure (sandbox crash/timeout) to reveal its code, otherwise give Canceled.
      return {
        kind: "failed",
        error:
          settlement.error === undefined
            ? { code: "Cancelled", message: "snippet evaluation was cancelled" }
            : { code: settlement.error.code, message: settlement.error.message },
        logs,
        logsTruncated,
      };
    },
  };
}

/** The number of bytes is given when the serialization of the return value exceeds the limit, otherwise it is undefined. The value has passed the NDJSON boundary and must be JSON-safe. */
function artifactOversize(artifact: unknown): number | undefined {
  if (artifact === undefined) return undefined;
  const bytes = Buffer.byteLength(JSON.stringify(artifact), "utf8");
  return bytes > MAX_ARTIFACT_BYTES ? bytes : undefined;
}

/**
* Snippet's sessionless driver: world read takes the production execution side, journal is implemented in memory, and the ask family is not reachable.
* (scratch facade without agent()) - Wiring bug on arrival, failing loudly by throwing structured DriverError.
 */
function createSnippetDriver(
  worldReadDeps: WorldReadDeps,
  onEvent: (event: RunEvent) => void,
): WorkflowDriver {
  const askUnreachable = (member: string): WorkflowError =>
    new WorkflowError(
      "DriverError",
      `Snippet driver received ${member}, but the scratch facade has no agent(), so the ask ` +
        `path must be unreachable. This is a wiring bug.`,
    );
  return {
    createActorSession: () => Promise.reject(askUnreachable("createActorSession")),
    startAsk: () => {
      throw askUnreachable("startAsk");
    },
    respondToSubmit: () => {
      throw askUnreachable("respondToSubmit");
    },
    cancelAsk: () => {
// Cancel the idempotent sweep on the ending path (the engine cancels the in-flight ask broadcast); the snippet has no in-flight ask and has nothing to do.
    },
    executeWorldRead: (op, args) => executeWorldRead(worldReadDeps, op, args),
    journal: new InMemoryJournalStore(),
    emit: onEvent,
  };
}
