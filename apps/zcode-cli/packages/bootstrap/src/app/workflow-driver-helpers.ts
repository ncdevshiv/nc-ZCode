// ============================================================
//AgentRuntime-backed WorkflowDriver: purely auxiliary
// ============================================================
// workflow-driver.ts reaches the upper limit of oxlint max-lines (400 lines), and combines pure functions that do not touch the driver state with
// constants (deferred, actor session id casting, upgrade budget and qid fragment, nudge/schema endnote, verdict and statistics
// Mapping, turn failure and normalization) are split into this file; the public side (mintActorSessionId) is still exported from workflow-driver.ts.

import {
  CoreErrorType,
  createSessionId,
  type SessionId,
  type SubmitVerdict as ContractsSubmitVerdict,
  type SubmitViolation,
} from "@zcode/contracts";
import type { TurnResult } from "@zcode/core";
import {
  refToString,
  WorkflowError,
  type ActorRef,
  type AskStats,
  type InstanceRef,
  type PersonaSpec,
  type Violation,
  type WorkflowReportSink,
} from "@zcode/dynamic-workflow";
import type { ActorToolCounts } from "./workflow-driver-tool-activity.js";
import type { Deferred, SessionState } from "./workflow-driver-types.js";

export function defer<T>(): Deferred<T> {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

/**
* actor session id: run scope + charset safe.
 *
* The old solution `createSessionId("wf-actor-" + refToString(actor))` has two defects: it does not contain runId, so
* Two concurrent runs of the same site×ordinal actors collide with the same ID; and with `#`/`@` (the form of refToString is
* `actor#1@1`), and the session id will be entered into the URL, file path and log.
 */
/**
* actor session id: run scoped and charset safe.
 *
* **Export is intentional**: progress projections should bring out the session id on the `actor-created` event (Boundary C does not bring it),
* And "Calculate session id by (runId, actorRef)" must have only one implementation - if two places are calculated once, there will be one day when they are not equal.
* The performance is that the details page opens a session that does not exist. This function is called by run service (the test pins both sides to be equal).
 */
export function mintActorSessionId(runId: string, actor: ActorRef): SessionId {
  return createSessionId(
    `dwf-${sanitizeIdSegment(runId)}-${sanitizeIdSegment(refToString(actor))}`,
  );
}

/**
* Fold any string into `[A-Za-z0-9.\-_]`, where `_` only appears as escaped output.
 *
* **Two steps, the order is contract**: first escape the literal `_` into `__`, and then map characters other than `[A-Za-z0-9.-]` into a single `_`.
* If the order is reversed, the escaping will be invalid (the `_` produced in the second step will be translated again by the first step). The `_` produced in the first step falls into the second step
* The mapping is identical, so the two steps can be safely concatenated.
 *
* Why that step is escaped: without it, both `my_actor#1` and `my#actor#1` collapse into `my_actor_1` - a true collision.
* A simple whitelist is collision-free only if "the site id does not have `_` in the vocabulary, and that vocabulary is owned by the analyzer.
* Not owned by this document. Escaping changes "relying on someone else's vocabulary to maintain a certain shape" into a locally verifiable property.
 *
* Residual (smaller) assumption: Two **different** special characters still map to `_`, so hypothetical `a#b` and `a@b` would collide.
* Today and in predictable site IDs, special characters appear in fixed positions (`actor#N@M`, site-specific `kind#N/M`),
* There is no such isomorphic pair; if we really want to eliminate it, we must give each special character an independent encoding.
 */
function sanitizeIdSegment(value: string): string {
  return value.replace(/_/g, "__").replace(/[^A-Za-z0-9.-]/g, "_");
}

/**
* The maximum number of upgrades in one ask (same family as nudge budget).
 *
*Beyond the upper limit** is not a mistake but a discipline**: the fourth call gets the ordinary result of "advance with your own best judgment".
* Upgrades do not write dwf_node lines (wait is not work), so this count is the only bound.
 */
export const MAX_ESCALATIONS_PER_ASK = 3;

/** The tool result text returned to the model when the budget is exhausted (normal results, not errors). */
export const ESCALATION_BUDGET_EXHAUSTED =
  `Escalation budget exhausted: at most ${MAX_ESCALATIONS_PER_ASK} escalations per task. ` +
  "Do not call escalate again. Proceed on your best judgement with the information you have, " +
  "and state in your final result the assumptions you relied on and the doubts that remain.";

/** In addition to the actor session id fragment, a candidate sequence of qid fragments (from short to long, the last one is the complete runId). */
export function questionIdFragments(runId: string): string[] {
// The `dwfrun-` prefix is ​​the same for every run. Leaving it in will only make the qid longer without increasing the recognition.
  const body = runId.startsWith("dwfrun-") ? runId.slice("dwfrun-".length) : runId;
  const full = sanitizeIdSegment(body);
  const candidates = [full.slice(0, 8), full.slice(0, 16), full];
// The three candidates on the short runId will degenerate into the same string; deduplication is only to avoid unnecessary repeated table lookups.
  return [...new Set(candidates)];
}

/**
* The **effective name** of the actor: directly take `persona.name`, the absence or empty string means anonymous.
 *
* No need to rerun normalization here - the persona received by the driver is already a product of the engine `normalizePersona`
* (scheduler passes `actor.persona` unchanged into `createActorSession`), so `spec.name` is
* The valid name recognized by the engine: the name on the `actor-created` event, the cached identity key of amend-resume,
* DuplicateActorName is the key to duplicate checking.
 *
* **Intentionally not trim**, contrary to {@link normalizeEscalationContext}: the engine's anonymity criterion is
* `name !== undefined && name !== ""` (createActor of engine.ts), an actor named `" "`
* Is **named** to the engine and occupies the cache identity key. If trim is set to anonymous here, the same actor will appear in "With or without a name"
* There are two answers to this question - and that's what the upgrade log is used to identify the questioner. Follow the engine.
 */
export function effectiveActorName(persona: PersonaSpec): string | undefined {
  const name = persona.name;
  return name === undefined || name === "" ? undefined : name;
}

/** Optional context for `escalate`: blank is equivalent to absence (models often pass empty strings, and falling into events is just noise). */
export function normalizeEscalationContext(context: string | undefined): string | undefined {
  const trimmed = context?.trim();
  return trimmed === undefined || trimmed.length === 0 ? undefined : trimmed;
}

/** turn ends the nudge prompt to promote model submission when it is not submitted. */
export const NUDGE_PROMPT =
  "You ended your turn without submitting a result. Call the submit_result tool now with a payload conforming to the required schema.";

/**
* Schema endnote of typed ask: appended to the instruction body, telling the model to use submit_result to submit results that conform to the schema.
* Format (this comment is the contract): separator line + one sentence request to call the tool + indented 2-space JSON Schema + one sentence result constraint.
* The wording does not contain "exactly once" - the model will legally call submit_result multiple times during the repair round.
 */
export function schemaEpilogue(schema: unknown): string {
  const rendered = schema === undefined ? "(any JSON value)" : JSON.stringify(schema, null, 2);
  return [
    "",
    "",
    "---",
    "When you have finished, call the `submit_result` tool to submit your final result. Its `result` argument must be a JSON value conforming to this JSON Schema:",
    "",
    rendered,
    "",
    "Pass the conforming JSON as the `result` argument — do not wrap it or add commentary.",
  ].join("\n");
}

/**
* Endnote of typed ask for mono sub-agent: The schema is already in the tool declaration, and there is only one sentence left here: "Adjust the tool when finished." with {@link schemaEpilogue}
* The same format skeleton (two blank lines + separator line), GUI endnote collapse by border index instead of text, is not affected.
 */
export const TYPED_TOOL_EPILOGUE = [
  "",
  "",
  "---",
  "When you have finished, call the `submit_result` tool to submit your final result. Its `result` argument must match the tool's declared schema — pass the conforming JSON directly, do not wrap it or add commentary.",
].join("\n");

/** Engine Violation → contracts SubmitViolation (structural isomorphism, 1:1). */
export function mapViolations(violations: readonly Violation[]): SubmitViolation[] {
  return violations.map((v) => ({ path: v.path, expected: v.expected, got: v.got }));
}

/** Synthesizes a binary rejection (for driver local interception, without going through the engine). */
export function rejectWith(message: string): ContractsSubmitVerdict {
  return {
    accept: false,
    violations: [{ path: "$", expected: message, got: "submit_result call" }],
  };
}

/**
* AskStats distilled from observations of the TurnResult + tool active side. tokens is the engine’s only strongly dependent field (budget deduction); turns are related to two
* The tool count is for journal recording, where `toolCalls === 0` makes this ask a **pure** ask that can still be hit after the import cache is closed, `worldToolCalls`
* It is the accounting of "it has seen or touched the outside world" - the protocol tool (`submit_result` / `escalate`) is not counted, otherwise each typed ask
* will no longer be pure because of the results.
 *
* Count `ToolCallStarted` from `result.events`, and TurnResult's events array
* Excludes tool events - 2097 ask lines in the production journal have `toolCalls` all 0, including subagents that clearly wrote the file. Ruochun ask
The judgment of * is based on this 0. After closing the door, each cache entry will be regarded as a pure hit as usual, which happens to let go of the type that the door is closed to prevent. Count therefore
* Instead, the driver's tool activity side takes the data from the **session event stream** (that's where the tool calls actually appear), and adds them after pressing ask.
 */
function statsFromTurn(result: TurnResult, toolCounts: ActorToolCounts): AskStats {
  const usage = result.usage;
  return {
    tokens: usage?.totalTokens ?? 0,
    toolCalls: toolCounts.toolCalls,
    turns: usage?.modelRequestCount ?? 1,
    worldToolCalls: toolCounts.worldToolCalls,
  };
}

/** Determine whether the reject of executeTurn is "user/engine cancellation" (normal end, not counting driver failure). */
export function isTurnCancelled(error: unknown): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    (error as { type?: unknown }).type === CoreErrorType.TurnCancelled
  );
}

/** Convert any (non-model layer) turn failure to node-level WorkflowError (DriverError), retaining the original cause. */
export function toWorkflowError(error: unknown): WorkflowError {
  if (error instanceof WorkflowError) return error;
  const message = error instanceof Error ? error.message : String(error);
  return new WorkflowError("DriverError", `Subagent turn failed: ${message}`, { cause: error });
}

// ——————————— Model-side failed containment: constant vs. purely auxiliary ———————————

/** The upper bound of the original provider text in ProviderStop details (both notifications and journals carry it, it cannot be unbounded). */
export const PROVIDER_STOP_RAW_MESSAGE_MAX_CHARS = 2000;

/** Resume prompt for transient redrive (same mechanism as nudge: a new turn on the same persistent runtime). */
export const TRANSIENT_CONTINUE_PROMPT =
  "The previous model request failed transiently and was abandoned; continue from where you left off.";

const TRANSIENT_BACKOFF_BASE_MS = 2_000;
const TRANSIENT_BACKOFF_MAX_MS = 60_000;

/** The same curve for runner: doubles from 2s to 60s, multiplied by the jitter of [0.5, 1]. */
export function transientBackoffMs(attempt: number, random: () => number = Math.random): number {
  const raw = Math.min(TRANSIENT_BACKOFF_MAX_MS, TRANSIENT_BACKOFF_BASE_MS * 2 ** (attempt - 1));
  return Math.round(raw * (0.5 + 0.5 * random()));
}

/** Retry-After on adapter error (`context.retryAfterMs`, within one layer of cause), read by shape. */
export function readRetryAfterMs(error: unknown): number | undefined {
  for (const candidate of [error, (error as { cause?: unknown } | undefined)?.cause]) {
    const context = (candidate as { context?: { retryAfterMs?: unknown } } | undefined)?.context;
    const value = context?.retryAfterMs;
    if (typeof value === "number" && Number.isFinite(value) && value > 0) return value;
  }
  return undefined;
}

/** The name of the sub-agent in the notification copy: use the first name if there is a name, otherwise `site@ordinal`. */
export function subagentLabel(state: Pick<SessionState, "actor" | "actorName">): string {
  return state.actorName ?? refToString(state.actor);
}

export const defaultSchedule = (callback: () => void, delayMs: number): (() => void) => {
  const timer = setTimeout(callback, delayMs);
  if (typeof timer === "object" && timer !== null && "unref" in timer) timer.unref();
  return () => clearTimeout(timer);
};

/**
* One turn parses two facts reported to the engine, and the order is load-bearing:
* 1. `askProgress` → `node-progress`: Which round this ask has run to, how many times the tool has been used, and where it has been recently;
* 2. `askStats` → `usage-updated`: How many tokens were spent in this round (reported once regardless of accept / text / nudge).
* Progress comes first, so anyone who reads the new amount must have already read the progress that earned it. The two are combined into one function precisely for the purpose of
* Give this sequence a place to point to, rather than two lines scattered across the call site.
 */
export function reportTurnObservations(
  sink: WorkflowReportSink,
  state: SessionState,
  instance: InstanceRef,
  result: TurnResult,
): void {
  sink.askProgress(instance, state.modelActivity.noteTurnResolved());
  sink.askStats(instance, statsFromTurn(result, state.modelActivity.toolCounts()));
}
