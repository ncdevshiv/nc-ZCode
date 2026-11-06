import {
  AMEND_WORKFLOW_TOOL_NAME,
  CREATE_WORKFLOW_TOOL_NAME,
  SessionEventType,
  type DynamicWorkflowRunProgressPayload,
  type SessionEvent,
} from "@zcode/contracts";
import { createDenyPermissionBroker } from "@zcode/core";
import type { GlobalOptions } from "@zcode/shared-types";
import type { ZCodeAppOptions } from "@zcode/bootstrap";
import { readRuntimeFunction } from "./runtime-event-subscriber.js";

/**
* CreateWorkflow approval bypass under headless (`-p`).
 *
* Why it is needed: headless never constructs permissionBroker, core therefore falls back to
 * `createDenyPermissionBroker()`（`core/src/runtime/agent-runtime.ts`、
* `core/src/tool/executor/impl.ts`), and CreateWorkflow's `alwaysAsk` gate
* Must go through broker in any mode (`yolo` cannot jump, `core/src/permission/service.ts`
*'s alwaysAsk branch precedes the pattern branch). The combined result is that dwf must be rejected immediately under `-p`,
* The error text is "No permission client configured for CreateWorkflow" - not a hang, not a timeout.
 *
* Bypass only releases CreateWorkflow according to the tool name, and the other tools** are delegated to the same deny broker**:
* Reuse rather than copy its rejection semantics, making "other tools maintain today's semantics" a structural fact rather than a coincidence
* (Copy drift is impossible because there is only one copy).
 *
* This is not a permission bypass, just a gate bypass:
* Zero changes to core, PermissionRequest hook still responds before broker (`permission-flow.ts`
*'s `??` short-circuit), permission events are emitted as usual, and actors in the run still inherit the permission profile of the session.
 */
export const createHeadlessPermissionBroker = (): NonNullable<
  ZCodeAppOptions["permissionBroker"]
> => {
  const denyBroker = createDenyPermissionBroker();
  return {
    requestPermission: async (request, options) => {
// AmendWorkflow and CreateWorkflow have the same door and the same exception.
      if (
        request.toolName !== CREATE_WORKFLOW_TOOL_NAME &&
        request.toolName !== AMEND_WORKFLOW_TOOL_NAME
      ) {
        return await denyBroker.requestPermission(request, options);
      }
      return {
        decision: "allow",
        reason: `Headless CLI auto-approves ${request.toolName}: no interactive gate exists in -p mode.`,
        resolvedAt: new Date(),
      };
    },
  };
};

/**
* `type` of dwf progress line in stream-json.
 *
* Deliberately **not** enter `zcodeSessionEventTypeSchema` (`shared/src/zcode-protocol/index.ts`):
* That is a closed set `z.enum` and fed to the discriminated union of `zcodeSessionEventSchema`, the bonus is equal to let
* v3 app-server declares an event on the type face that it never emits. stream-json is a CLI private output format,
* Not subject to protocol strict schema.
 */
const WORKFLOW_RUN_PROGRESS_STREAM_TYPE = "workflow.run.progress";

/**
* One-line stereotyped dwf progress NDJSON. Envelope fields are aligned verbatim with `mapSessionEvent` (same batch of keys, same
* `String()`/`getTime()` normalization), so that line readers do not need to change a set of envelope interpretation rules for this type of line;
* `payload` is the bounded payload of contracts **as is**, without reshaping field names.
 */
interface WorkflowRunProgressStreamLine {
  type: typeof WORKFLOW_RUN_PROGRESS_STREAM_TYPE;
  eventId: string;
  sessionId: string;
  seq: number;
  timestamp: number;
  traceId: string;
  payload: DynamicWorkflowRunProgressPayload;
}

/** Is this session event dwf progress? stream-json and stderr progress share this distinction. */
const isDynamicWorkflowRunProgressEvent = (event: SessionEvent): boolean =>
  event.type === SessionEventType.DynamicWorkflowRunProgress;

const progressPayloadOf = (event: SessionEvent): DynamicWorkflowRunProgressPayload =>
  event.payload as DynamicWorkflowRunProgressPayload;

/**
* dwf progress event → stereotype NDJSON row.
 *
* Replace today's naked leak: `mapSessionEventType`'s default makes this type catch-all
* `session.updated` (`session-mapper.ts`), so the consumer of stream-json receives a row
* A line with the same name as "something changed in the session" but carrying the run internal payload - there's no way to tell, and there's no way to just subscribe to it.
 *
* Note that the dwf event is **out of turn** (`turnId` is empty), so it is not included in the envelope
* `turnId`: Writing a key that is always undefined will only make readers think that it sometimes has a value.
 */
const mapWorkflowRunProgressStreamLine = (event: SessionEvent): WorkflowRunProgressStreamLine => ({
  type: WORKFLOW_RUN_PROGRESS_STREAM_TYPE,
  eventId: String(event.id),
  sessionId: String(event.sessionId),
  seq: event.sequenceNumber,
  timestamp: event.timestamp.getTime(),
  traceId: String(event.traceId),
  payload: progressPayloadOf(event),
});

const asRecord = (value: unknown): Record<string, unknown> =>
  value && typeof value === "object" ? (value as Record<string, unknown>) : {};

const instanceLabel = (payload: Record<string, unknown>): string | undefined => {
  const instance = asRecord(payload.instance);
  const siteId = instance.siteId;
  if (typeof siteId !== "string") return undefined;
  const ordinal = instance.ordinal;
  return typeof ordinal === "number" ? `${siteId}@${ordinal}` : siteId;
};

/**
* The **state transition** aspects of engine event types (`text` mode stderr only talks about these + `log`).
 *
* Glossary from contract annotation of `toProtocolEvent` (`bootstrap/src/app/dynamic-workflow-run-launch.ts`):
 * run-started / actor-created / node-queued / node-dispatched / node-repairing /
 * node-nudged / node-settled / usage-updated / log / run-settled。
* `usage-updated` and `actor-created` are deliberately not printed: the former is a pure count, the latter is not a migration.
 */
const describeProgress = (payload: DynamicWorkflowRunProgressPayload): string | undefined => {
  const inner = asRecord(payload.payload);
  switch (payload.eventType) {
    case "run-started":
      return "started";
    case "run-settled": {
// Final state vocabulary completed / errored / stopped;
// stopped with reason: `stopped/provider: …`.
      const rawStatus = typeof inner.status === "string" ? inner.status : "settled";
      const status =
        typeof inner.stopReason === "string" ? `${rawStatus}/${inner.stopReason}` : rawStatus;
      const error = asRecord(inner.error).message;
      return typeof error === "string" ? `${status}: ${error}` : status;
    }
    case "log":
      return typeof inner.message === "string" ? `log: ${inner.message}` : undefined;
    case "node-queued":
    case "node-dispatched":
    case "node-repairing":
    case "node-nudged": {
      const label = instanceLabel(inner);
      return label ? `${payload.eventType.slice("node-".length)} ${label}` : undefined;
    }
    case "node-settled": {
      const label = instanceLabel(inner);
      const outcome = typeof inner.outcome === "string" ? inner.outcome : "settled";
      return label ? `settled ${label} (${outcome})` : undefined;
    }
    default:
      return undefined;
  }
};

/**
* Whether migration is a **required** aspect. The start and settlement of run are the two endpoints of this progress stream, and any throttling must not eat them
* - Otherwise `text` mode might not type a word during the entire waiting period, and then suddenly print the final answer.
* `log` is explicitly issued by the script author (`log()` facade) and is also not throttled: its frequency is determined by the script,
* And the author wrote it just to be seen.
 */
const isUnthrottledProgress = (eventType: string): boolean =>
  eventType === "run-started" || eventType === "run-settled" || eventType === "log";

interface WorkflowProgressReporterInput {
/** The progress is only written to stderr - stdout belongs to the result (`text` mode writes short progress in stderr). */
  write: (line: string) => void;
/** Inject the clock so that throttling can be exhausted by sleepless single testing. */
  now?: () => number;
/** Minimum interval for node phase migration; `log` and run endpoints are not bound by this. */
  throttleMs?: number;
}

/** Default throttling interval for node phase migration. A 100-node run has about 5×100 events. */
const DEFAULT_WORKFLOW_PROGRESS_THROTTLE_MS = 400;

/**
* stderr progress printer under `--output-format text`.
* Only dwf progress events are recognized, and other session events are ignored directly (no side effects after return).
 */
const createWorkflowProgressReporter = (
  input: WorkflowProgressReporterInput,
): ((event: SessionEvent) => void) => {
  const now = input.now ?? Date.now;
  const throttleMs = input.throttleMs ?? DEFAULT_WORKFLOW_PROGRESS_THROTTLE_MS;
  let lastThrottledAt: number | undefined;
  return (event) => {
    if (!isDynamicWorkflowRunProgressEvent(event)) return;
    const payload = progressPayloadOf(event);
    const description = describeProgress(payload);
    if (description === undefined) return;
    if (!isUnthrottledProgress(payload.eventType)) {
      const at = now();
      if (lastThrottledAt !== undefined && at - lastThrottledAt < throttleMs) return;
      lastThrottledAt = at;
    }
    input.write(`workflow ${payload.runId}: ${description}\n`);
  };
};

interface HeadlessWriteStream {
  write: (chunk: string) => unknown;
}

interface HeadlessSessionObserverInput {
  options: Pick<GlobalOptions, "json" | "outputFormat">;
  stderr: HeadlessWriteStream;
  stdout: HeadlessWriteStream;
/** Only required by `--output-format stream-json`; NDJSON is not written in its absence. */
  mapSessionEvent?: (event: SessionEvent) => unknown;
}

interface HeadlessSessionObserver {
/** The only consumer of session events. Installed on **one** sink - writing once to both sinks is a duplicate row. */
  observe: (event: SessionEvent) => void;
  /**
* Have you observed dwf activity? This is the **narrow trigger** criterion for "waiting for settlement": no dwf activity running
* Do not wait once, the behavior is the same byte by byte as before the change.
   */
  hasWorkflowActivity: () => boolean;
  /**
* Start recording round text. Must be called **synchronously** after `submitPrompt` returns: that moment until the first await
* No event can jump in the queue, so the first event of the notification driver round will not be missed.
* `excludeTurnId` is the id of the first turn - its text is given by the return value of `submitPrompt`,
* In case its `turn_complete` is late, rely on this id to prevent repeated counting.
   */
  beginWaitPhase: (excludeTurnId?: string) => void;
/** The text of each completed round during the waiting period, in order of arrival. */
  waitPhaseTurnResponses: () => readonly string[];
}

/**
* Headless session event observer: combine "what to do according to the output format" and "what to wait for" in one place.
 *
* Division of labor in three formats:
* stream-json → One line of NDJSON for each event to stdout, dwf progress follows the stereotyped line; stderr is not repeated.
* text → only dwf progress, and only to stderr (stdout belongs to the result).
* json → does not write any events (the contract is "exactly one object").
 *
* All three formats need to observe events - even if json does not type a word, waiting still depends on the dwf trigger criterion here.
* Record with turn text. So this function always returns an observer and no longer returns undefined.
 */
export const createHeadlessSessionObserver = (
  input: HeadlessSessionObserverInput,
): HeadlessSessionObserver => {
  const { mapSessionEvent, options, stderr, stdout } = input;
  const writeStreamEvent = mapSessionEvent
    ? (event: SessionEvent) => {
        // One event per line, written as it happens. Deliberately not formatJson:
        // that pretty-prints with an indent, which would spread a single event
        // over several lines and break every line-oriented reader downstream.
        //
// The progress of dwf goes through the stereotype line: the default of mapSessionEvent will make it catch-all
// `session.updated` (session-mapper.ts), readers can neither distinguish nor
// Just subscribe to it.
        const line = isDynamicWorkflowRunProgressEvent(event)
          ? mapWorkflowRunProgressStreamLine(event)
          : mapSessionEvent(event);
        stdout.write(`${JSON.stringify(line)}\n`);
      }
    : undefined;
  const reportProgress =
    mapSessionEvent === undefined && wantsWorkflowProgress(options)
      ? createWorkflowProgressReporter({ write: (line) => void stderr.write(line) })
      : undefined;

  let workflowActivity = false;
  let waiting = false;
  let excludedTurnId: string | undefined;
  const turnResponses: string[] = [];

  return {
    hasWorkflowActivity: () => workflowActivity,
    beginWaitPhase: (excludeTurnId) => {
      waiting = true;
      excludedTurnId = excludeTurnId;
    },
    waitPhaseTurnResponses: () => turnResponses,
    observe: (event) => {
      writeStreamEvent?.(event);
      reportProgress?.(event);
      if (isWorkflowActivityEvent(event)) workflowActivity = true;
      if (!waiting || event.type !== SessionEventType.TurnComplete) return;
// The text of the first turn comes from the return value of submitPrompt; its turn_complete will be counted twice if it is late.
      if (event.turnId !== undefined && String(event.turnId) === excludedTurnId) return;
      const response = (event.payload as { response?: unknown }).response;
      if (typeof response === "string" && response.trim().length > 0) turnResponses.push(response);
    },
  };
};

/**
* Does this event prove that this process has dwf activity?
 *
* Two criteria instead of one: the progress event is the most direct evidence, but its append is asynchronous and may theoretically be later than
* Arrive at the sink at the end of the round. `BackgroundTaskStarted` is issued synchronously when the tool executor registers a background task.
* **Necessarily** falls within the turn that initiated it - so it's earlier, harder evidence. Collect both, only one of them is needed to trigger.
 */
const isWorkflowActivityEvent = (event: SessionEvent): boolean => {
  if (isDynamicWorkflowRunProgressEvent(event)) return true;
  return (
    event.type === SessionEventType.BackgroundTaskStarted &&
    (event.payload as { taskKind?: unknown }).taskKind === "workflow"
  );
};

/** Two busy authoritative facts for the runtime. Narrow interface instead of the entire AgentRuntime - wait read-only for these two booleans. */
interface HeadlessWorkflowRuntimeFacts {
  hasActiveOrQueuedTurnWork: () => boolean;
  hasRunningBackgroundTasks: () => boolean;
}

/** Polling interval. The waiting period is on the order of minutes. The overhead of this granularity is negligible, and it determines the responsiveness of the exit. */
const HEADLESS_WORKFLOW_POLL_INTERVAL_MS = 100;

/**
* Waiting for workflow run settlement on the fly ** + Completion notification driven round run completed **.
 *
* Why polling these two Boolean is enough (no competing windows, this is the key argument of this mechanism): background task transfers to final state
* (`updateRuntimeBackgroundTask` of `background-tasks.ts`) is enqueued with notification commands
* (`runtime-command-queue.ts`, via `maybeEnqueueBackgroundTaskNotification` →
* There is no await** between `enqueueBackgroundTaskNotification` → `enqueueRuntimeCommand`),
* And `drainRuntimeCommandQueue` also sets `runtimeCommandDrainActive` to true before the first await.
* So "task is no longer running" and "round work is pending" are flipped within the same synchronization block, and the poller cannot fall in the middle.
 *
* Predicates deliberately **wider than dwf**: concurrent background Bash/subagent tasks will also be waited for. Their notification rounds and workflows
* Intertwined on the same queue, there is no point in waiting separately. The narrow half is **trigger**(`hasWorkflowActivity`),
* So running without dwf activity is completely unaffected.
 *
* No timeout, no env escape hatch: the control methods are Cancel and Ctrl-C, the latter is converged by existing orphans
* Remember run as `stopped(interrupted)` (failure code `Interrupted`, can be resumed). Once the signal is aborted, it returns immediately and never swallows the signal.
 */
export const waitForHeadlessWorkflowSettle = async (input: {
  intervalMs?: number;
  runtime: HeadlessWorkflowRuntimeFacts;
  signal: AbortSignal;
  sleep?: (ms: number) => Promise<void>;
}): Promise<void> => {
  const intervalMs = input.intervalMs ?? HEADLESS_WORKFLOW_POLL_INTERVAL_MS;
  const sleep =
    input.sleep ?? ((ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms)));
  while (!input.signal.aborted) {
    if (!input.runtime.hasRunningBackgroundTasks() && !input.runtime.hasActiveOrQueuedTurnWork()) {
      return;
    }
    await sleep(intervalMs);
  }
};

/**
* Can this app's runtime provide busy facts?
 *
* For the reason of dynamic reading, see `readRuntimeFunction` of `runtime-event-subscriber.ts` (the same
* `RunDependencies.createZCodeApp` injection point boundaries).
 *
* **No silent downgrade**: Don’t wait until you get the busy fact - an honest answer to "this host does not have this ability",
* Instead of pretending to have waited.
 */
export const readHeadlessRuntimeFacts = (
  runtime: unknown,
): HeadlessWorkflowRuntimeFacts | undefined => {
  const turnWork = readRuntimeFunction(runtime, "hasActiveOrQueuedTurnWork");
  const backgroundTasks = readRuntimeFunction(runtime, "hasRunningBackgroundTasks");
  if (!turnWork || !backgroundTasks) return undefined;
  return {
    hasActiveOrQueuedTurnWork: () => turnWork.call(runtime) === true,
    hasRunningBackgroundTasks: () => backgroundTasks.call(runtime) === true,
  };
};

/**
* Should I check the workflow progress on stderr for this run?
 *
* Only `text` has this location: the contract of `json` is exactly one object, and `stream-json` has each progress
* Shape to one line on stdout. The default (without --output-format, without --json) is text, so it is also typed.
 */
const wantsWorkflowProgress = (options: Pick<GlobalOptions, "json" | "outputFormat">): boolean =>
  options.outputFormat === undefined ? !options.json : options.outputFormat === "text";
