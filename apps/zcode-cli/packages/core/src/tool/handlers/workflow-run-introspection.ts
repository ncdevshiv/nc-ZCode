// ============================================================
// Common aspects of Workflow run introspection tools (ListWorkflowRuns / GetWorkflowRun)
// ============================================================
//
// The three things must be shared verbatim, so they are written here instead of in each handler:
//
// 1. **Business failure in the absence of capabilities**. "Port is absent" (journal is not available → run service is not constructed at all) and
// "Port present but method absent" (journal without introspection query) is the same thing for models: this session does not have this capability.
// Two tools, two criteria, and a piece of copywriting.
// 2. **Failure code**. The type of `ToolHandlerFailure.errorCode` is number (executor side
// `isToolHandlerFailure` is closed by number), so there are two stable discrimination keys
// (`workflow_introspection_unavailable` / `run_not_found`) falls on the prefix of message——
// It is the only discriminant bit that the model and log can read.
// 3. **Asynchronous boot copy**. Both tool descriptions must clearly state that "the run of this session will automatically send a notification of the product."
// Copywriting bifurcation is equivalent to giving the model two sets of default behaviors.

import { escapeXml } from "../../runtime-task/notification.js";
import type { ToolHandlerFailure } from "../types.js";

/**
* Business failure code. The value itself does not enter the model (the executor projects it as `code: "1"`), and the discriminant key is in the message prefix.
* The two codes must be different: ability absence and run unknown are two things that the model needs to handle separately.
 */
const WORKFLOW_RUN_INTROSPECTION_ERROR_CODE = {
  INTROSPECTION_UNAVAILABLE: 1,
  RUN_NOT_FOUND: 2,
} as const;

/**
* "This session does not have workflow introspection capabilities". **Never** silently return an empty list: that will cause the model to say "This item has not been run"
* workflow" and "workflow cannot be read in this session" are mixed into the same conclusion (the same reason for the visible downgrade of CreateWorkflow).
 */
export function workflowIntrospectionUnavailableFailure(): ToolHandlerFailure {
  return {
    result: false,
    errorCode: WORKFLOW_RUN_INTROSPECTION_ERROR_CODE.INTROSPECTION_UNAVAILABLE,
    message:
      "workflow_introspection_unavailable: this session cannot read workflow runs — workflow execution is not available here, so no run history is reachable. This is a capability gap, not an empty project.",
  };
}

export function workflowRunNotFoundFailure(runId: string): ToolHandlerFailure {
  return {
    result: false,
    errorCode: WORKFLOW_RUN_INTROSPECTION_ERROR_CODE.RUN_NOT_FOUND,
    message: `run_not_found: no workflow run with ID ${runId} exists for this project. Use ListWorkflowRuns to see the runs that do.`,
  };
}

/**
* Both tools describe common asynchronous boot.
 *
* Do not disable polling - it is still the right action when the user explicitly asks to watch on the fly; this is just the default.
 */
export const WORKFLOW_RUN_INTROSPECTION_STEERING = [
  "Runs this session starts settle on their own: you receive a completion notification carrying the final output. Do NOT poll this tool while waiting for one — continue with other work.",
  "Reach for it when: (a) the user asks how a workflow is going, (b) you want to review this project's earlier runs, including ones other sessions started, (c) a completion notification was truncated and you need the run's full record by ID.",
].join("\n");

/**
* epoch ms → ISO 8601 (UTC). The timestamp in the structured output is epoch ms (the original fact of the journal), and the model side gives
* ISO: That's how it reads "how long ago" directly without having to do mental arithmetic on a string of 13 digits.
 *
* Bad values ​​(NaN/out of bounds) fall back to the original number instead of throwing an error - a timestamp should not cause the entire tool to fail.
 */
export function formatWorkflowRunTimestamp(epochMs: number): string {
  if (!Number.isFinite(epochMs)) return String(epochMs);
  try {
    return new Date(epochMs).toISOString();
  } catch {
    return String(epochMs);
  }
}

/** Duration ladder. Definition in one place, summary and formatter are shared - writing one set in two places will cause bifurcation during a certain parameter adjustment. */
const DURATION_MS = {
  second: 1_000,
  minute: 60_000,
  hour: 3_600_000,
  day: 86_400_000,
} as const;

function padTwo(value: number): string {
  return value < 10 ? `0${value}` : String(value);
}

/**
* milliseconds → the duration of human reading (`40s` / `5m 10s` / `2h 15m` / `3d 2h`).
 *
* Only two levels are retained: a reader wants magnitude rather than precision, and "1h 02m 03s" will make him count digits. Minutes and seconds filled with zeros
* (Both are base-60 subunits. When zeros are not added, `1m 5s` and `1m 50s` are indistinguishable at a glance). The sunset hour is not added.
* Non-finite values ​​and negative values ​​(clock rollback) are folded to 0, never let a timestamp turn the entire tool result into NaN.
 */
export function formatWorkflowRunDuration(ms: number): string {
  const total = Number.isFinite(ms) && ms > 0 ? ms : 0;
  if (total < DURATION_MS.minute) return `${Math.floor(total / DURATION_MS.second)}s`;
  if (total < DURATION_MS.hour) {
    const minutes = Math.floor(total / DURATION_MS.minute);
    const seconds = Math.floor((total % DURATION_MS.minute) / DURATION_MS.second);
    return `${minutes}m ${padTwo(seconds)}s`;
  }
  if (total < DURATION_MS.day) {
    const hours = Math.floor(total / DURATION_MS.hour);
    const minutes = Math.floor((total % DURATION_MS.hour) / DURATION_MS.minute);
    return `${hours}h ${padTwo(minutes)}m`;
  }
  const days = Math.floor(total / DURATION_MS.day);
  const hours = Math.floor((total % DURATION_MS.day) / DURATION_MS.hour);
  return `${days}d ${hours}h`;
}

/**
* "How long ago". `at` returns `undefined` when absent or not a finite number - read accordingly **entire paragraph omitted** this age,
* Instead of rendering a 0 or "unknown": it is a fact that old journals without timestamps cannot tell their age.
 */
export function formatRelativeAge(now: number, at: number | undefined): string | undefined {
  if (at === undefined || !Number.isFinite(at) || !Number.isFinite(now)) return undefined;
  return `${formatWorkflowRunDuration(now - at)} ago`;
}

/** Format ISO time and relative age; only display ISO time when age is unknown. */
export function formatWorkflowRunInstant(now: number, at: number): string {
  const age = formatRelativeAge(now, at);
  const iso = formatWorkflowRunTimestamp(at);
  return age === undefined ? iso : `${iso} (${age})`;
}

/**
*Thousandths. The result of `toLocaleString` depends on the host's ICU data, and every word in the model must be tested
* Pinned verbatim, so insert your own comma here.
 */
export function formatWorkflowRunCount(value: number): string {
  if (!Number.isFinite(value)) return String(value);
  const sign = value < 0 ? "-" : "";
  const digits = String(Math.trunc(Math.abs(value)));
  return sign + digits.replace(/\B(?=(\d{3})+(?!\d))/gu, ",");
}

/**
* XML-ish attributes. The value first folds the white space into a single space and then escapes: the newline in the attribute will destroy the layout of "one run, one line".
* And label is free text (name given by the user or the first line of the script).
 */
export function workflowRunAttribute(name: string, value: string | number | boolean): string {
  const text = typeof value === "string" ? value.replace(/\s+/gu, " ").trim() : String(value);
  return `${name}="${escapeXml(text)}"`;
}

/**
* Escape **Reuse** Which one is in the notification projection of runtime-task (notification.ts): both are of the same class
* XML-ish model plane projection, and a second copy of the escape table will only fork at a certain adjustment.
 */
export { escapeXml as escapeWorkflowRunText };
