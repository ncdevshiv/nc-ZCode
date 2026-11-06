import {
  SessionEventType,
  type DynamicWorkflowRunProgressPayload,
  type TraceContext,
} from "../deps.js";
import {
  formatWorkflowEscalationNotification,
  formatWorkflowStallNotification,
} from "../../runtime-task/notification.js";
import type { AgentRuntimeInternal } from "../internal.js";

/**
* Append a workflow run progress event to the **parent session** (run itself has no session).
 *
* This is an **out-of-round append**: run runs in the background, and the parent session may be running a round when the event arrives, or it may be completely idle.
* Both must be implemented, and this link originally supports both - BackgroundTask* uses it (core internal manager
* tool executor's emitEvent). The event source of dwf is in the run service of bootstrap and cannot be obtained at that level.
* AgentRuntimeInternal, so this public method is needed; the homomorphic precedent is {@link recordTargetChanged}.
 *
* Deliberately use `rootTraceContext` instead of the current turn's trace: turnId is therefore empty and the event does not pretend to be any turn.
* The merge of cold recovery will insert the memory event with turnId back to the end of the round according to the "unfinished round to which it belongs", and a run event
* Belongs to the run, not to any round - impersonation causes it to appear at the end of a completed round of dialogue after cold recovery.
 */
export async function recordDynamicWorkflowRunProgress(
  this: AgentRuntimeInternal,
  input: DynamicWorkflowRunProgressPayload & { traceContext?: TraceContext },
): Promise<void> {
  const { traceContext: provided, ...payload } = input;
  const traceContext = provided ?? this.rootTraceContext;
  await this.appendEvent(
    this.createEvent(
      SessionEventType.DynamicWorkflowRunProgress,
      payload satisfies DynamicWorkflowRunProgressPayload,
      traceContext,
    ),
    traceContext,
  );
  notifyEscalationRaised.call(this, payload, traceContext);
  notifyRunStalled.call(this, payload, traceContext);
}

/** Run-level stalled event type (engine's `RunEvent.type`). */
const RUN_STALLED_EVENT_TYPE = "run-stalled";

/**
* `run-stalled` → a model-visible run notification. Same layer and three items as {@link notifyEscalationRaised}
* Discipline: There is exactly one notification for each event (each stall segment is only sent once on the driver side, and the next one will be sent only after successful reloading);
* No prompting; never throw an exception - if the load shape is incorrect, skip it and record a log.
 */
function notifyRunStalled(
  this: AgentRuntimeInternal,
  payload: DynamicWorkflowRunProgressPayload,
  traceContext: TraceContext,
): void {
  if (payload.eventType !== RUN_STALLED_EVENT_TYPE) return;
  const sinceMs = payload.payload.sinceMs;
  if (typeof sinceMs !== "number" || !Number.isFinite(sinceMs) || sinceMs < 0) {
    this.logger?.warn?.("Dynamic workflow stall notification skipped: malformed payload", {
      event: "dynamic_workflow.stall.notification_skipped",
      module: "core.runtime",
      runId: payload.runId,
      sequence: payload.sequence,
    });
    return;
  }
  const runLabel = this.runtimeTaskRegistry.get(payload.runId)?.description ?? payload.runId;
  const reason = stringField(payload.payload, "reason");
  const capValue = payload.payload.cap;
  const cap =
    typeof capValue === "number" && Number.isFinite(capValue) && capValue >= 0
      ? Math.floor(capValue)
      : undefined;
  this.enqueueBackgroundTaskNotification({
    originMeta: {
      backgroundSource: "workflow",
      title: runLabel,
      workId: payload.runId,
      workflowNotification: {
        kind: "stall",
        sinceMs: Math.floor(sinceMs),
        ...(reason === undefined ? {} : { reason: reason.slice(0, 64) }),
        ...(cap === undefined ? {} : { cap }),
      },
    },
    taskId: payload.runId,
    text: formatWorkflowStallNotification({
      runLabel,
      runId: payload.runId,
      sinceMs,
      ...(reason === undefined ? {} : { reason }),
      ...(cap === undefined ? {} : { cap }),
    }),
    traceContext,
  });
}

/** Event type for upgrading Q&A (engine's `RunEvent.type`, arrived via progress payload's `eventType`). */
const ESCALATION_RAISED_EVENT_TYPE = "escalation-raised";

/**
* `escalation-raised` → a model-visible in-run notification.
 *
* Why not open a new port at this layer: the progress sink has sent every RunEvent here, this is the **only** one
* Where the run event and AgentRuntime are visible at the same time. Adding another port just makes the same fact go two ways.
 *
*Three disciplines:
* - **Exactly one notification for each raised**, no resending or prompting. Notifications were discarded (stale branch/shutdown)
* The bottom line is the query - pendingQuestions of GetWorkflowRun, not retry.
* - **`escalation-resolved` does not send notification**: the responder is the main agent itself, and the receipt is already the result of that tool call.
* - **Never throw an exception**: This is the observation surface, and the truth about run is in the journal. If the load shape is wrong, skip it and record a log——
* It crosses the port boundary and is bounded and clipped. Defensive reading is the treatment it deserves.
 */
function notifyEscalationRaised(
  this: AgentRuntimeInternal,
  payload: DynamicWorkflowRunProgressPayload,
  traceContext: TraceContext,
): void {
  if (payload.eventType !== ESCALATION_RAISED_EVENT_TYPE) return;

  const qid = stringField(payload.payload, "qid");
  const question = stringField(payload.payload, "question");
  if (qid === undefined || question === undefined) {
    this.logger?.warn?.("Dynamic workflow escalation notification skipped: malformed payload", {
      event: "dynamic_workflow.escalation.notification_skipped",
      module: "core.runtime",
      runId: payload.runId,
      sequence: payload.sequence,
    });
    return;
  }

// The backend link of the display name has the same origin as the final state notification: the description of the registry entry (the name of CreateWorkflow/
// Derived from the first line of the script) → runId. Deliberately not checking the port - notifications are generated synchronously, and this path does not do I/O.
  const runLabel = this.runtimeTaskRegistry.get(payload.runId)?.description ?? payload.runId;
  const context = stringField(payload.payload, "context");
// Anonymous actor does not have actorName (the engine deliberately does not synthesize hidden labels, see the comments of RunEvent): Here comes the
// Structured ref `site@ordinal`, which is uniquely positioned within run and can be read as a sentence. Notification text and manifest
// The payloads share the same pocket chain - if the two places are in separate pockets, the same actor will display different names in two places.
  const actor = stringField(payload.payload, "actorName") ?? actorRefLabel(payload.payload);
// askedAt is epoch ms and may be absent (stringField cannot read numbers and can only read them defensively).
  const askedAt = payload.payload.askedAt;

  this.enqueueBackgroundTaskNotification({
    originMeta: {
      backgroundSource: "workflow",
      title: runLabel,
// workId ≡ runId: The same display anchor point as the final notification of the run, so the two notifications fall on the same background work.
      workId: payload.runId,
// manifest payload (escalation decision branch): GUI collapsed into "Subagent X has a question",
// Expand to display the full text of the question. Launch-side casting, bounded (question/context ≤4000, synchronous shared schema).
      workflowNotification: {
        kind: "escalation",
        qid,
        actor,
        question: question.slice(0, WORKFLOW_ESCALATION_TEXT_MAX_CHARS),
        ...(context === undefined
          ? {}
          : { context: context.slice(0, WORKFLOW_ESCALATION_TEXT_MAX_CHARS) }),
        ...(typeof askedAt === "number" && Number.isFinite(askedAt) ? { askedAt } : {}),
      },
    },
// taskId allows the notification to inherit the branchGeneration of the run (fencing of late notifications); the bottom line is snapshot query.
    taskId: payload.runId,
    text: formatWorkflowEscalationNotification({
      runLabel,
      runId: payload.runId,
      qid,
      actor,
      question,
      ...(context === undefined ? {} : { context }),
    }),
    traceContext,
  });
}

/** The bounds of question / context in the manifest payload (shared schema: ≤4000 characters). */
const WORKFLOW_ESCALATION_TEXT_MAX_CHARS = 4_000;

/** `{siteId, ordinal}` → `site@ordinal` (the shape of the engine `refToString`); if it cannot be read, it will say "unknown actor". */
function actorRefLabel(payload: Record<string, unknown>): string {
  const actor = payload.actor;
  if (typeof actor === "string") return actor;
  if (typeof actor !== "object" || actor === null) return "unknown actor";
  const siteId = stringField(actor as Record<string, unknown>, "siteId");
  const ordinal = (actor as Record<string, unknown>).ordinal;
  if (siteId === undefined || typeof ordinal !== "number") return "unknown actor";
  return `${siteId}@${ordinal}`;
}

function stringField(payload: Record<string, unknown>, key: string): string | undefined {
  const value = payload[key];
  return typeof value === "string" && value.length > 0 ? value : undefined;
}
