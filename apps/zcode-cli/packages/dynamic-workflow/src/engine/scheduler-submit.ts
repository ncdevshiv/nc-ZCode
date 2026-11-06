/**
* scheduler.ts reaches the upper limit of oxlint max-lines (400 lines), and returns the two lines of Boundary B upwards
* (submit_result arrives, turn ends) and its repair / nudge budget and submit are normalized and split into this file;
* The public side is still exported from scheduler.ts.
 *
* The free function obtains the scheduler’s live node table and two settlement entries through {@link SubmitSeam}; on AskScheduler
* submitAttempted / turnEnded are just thin delegates.
 */

import type { AskNode, SchedulerHost } from "./scheduler-types.js";
import type { InstanceRef, Violation } from "./types.js";
import { REPAIR_ATTEMPTS, WorkflowError } from "./types.js";

/** The minimum seam the scheduler exposes to report processing: check the live node and settle based on the result. */
export interface SubmitSeam {
  readonly host: SchedulerHost;
/** Check live nodes by instance (undefined is returned if liveNodes has left). */
  liveNode(instance: InstanceRef): AskNode | undefined;
  settleOk(node: AskNode, artifact: unknown): void;
  settleFailed(node: AskNode, error: WorkflowError): void;
}

export function handleSubmitAttempted(
  seam: SubmitSeam,
  instance: InstanceRef,
  payload: unknown,
): void {
  const node = seam.liveNode(instance);
  if (node === undefined || node.settled) return;
// untyped ask does not provide submit_result; even if it is received, it will not be settled according to it.
  if (!node.spec.typed) return;

  const { value, violations } = normalizeSubmit(seam.host, node.spec.schema, payload);
  if (violations.length === 0) {
    seam.host.driver.respondToSubmit(instance, { kind: "accept" });
    seam.settleOk(node, value);
    return;
  }
  if (node.repairsRemaining > 0) {
    node.repairsRemaining--;
    const attempt = REPAIR_ATTEMPTS - node.repairsRemaining;
    seam.host.driver.respondToSubmit(instance, { kind: "reject", violations });
    seam.host.record({ type: "node-repairing", instance, attempt, violations });
    return;
  }
  seam.host.driver.cancelAsk(instance);
  seam.settleFailed(
    node,
    new WorkflowError(
      "ValidationFailed",
      "submit_result failed schema validation repeatedly and the repair budget is exhausted.",
      { violations },
    ),
  );
}

/**
* Loosely normalized submit payload: first verify according to the original value; only when the original value does not pass and the original value is a string, do a JSON.parse
* Verify again. If successful, the parsed value shall prevail (both will be returned to the verdict and will be returned to the journal).
 *
* Real disk discovery (GLM-5.3 via Anthropic compatible endpoint): Real models often serialize the `result` parameter of submit_result into
* JSON string (such as `"{\"title\":...}"`) instead of JSON object, causing the validator to correctly report "expected object, got string",
* The run failed after repairing 3 times. legacy script-workflow's parseStructuredResponse already tolerates this with relaxed JSON parsing
* case; add equal tolerance here, but schema-aware:
* 1. The original value is passed directly → the original value is used and never parsed (the legal string results are preserved, such as `string | null` schema, `"42"` must be left `"42"`).
* 2. The original value is nothing more than string → Single JSON.parse (no recursion, no double parsing, follow the legacy single extraction precedent) and then verify, and then take the parsed value.
* 3. Otherwise → return to original value + original violation, submit model for repair.
* The engine is a pure package, JSON.parse is purely deterministic and has no I/O, so it is safe to put it here.
 */
function normalizeSubmit(
  host: SchedulerHost,
  schema: unknown,
  payload: unknown,
): { value: unknown; violations: Violation[] } {
  const direct = host.validate(schema, payload);
  if (direct.length === 0) return { value: payload, violations: [] };
  if (typeof payload === "string") {
    try {
      const parsed = JSON.parse(payload);
      const after = host.validate(schema, parsed);
      if (after.length === 0) return { value: parsed, violations: [] };
// Bug: The parsing is successful but the parsed value is still not out of date. The value before parsing was reported
// `direct` violation ("$: expected object, got string"), the model is actually facing a decoded object, and there is no way to fix it.
// Can only be repeatedly requoted/double-encoded/degenerated into "{}", exhausting the repair budget. Repair violations must describe the values ​​that the model can modify,
// Therefore, `after` (path-level violation on the object after parsing) is returned here, and the single parsing contract remains unchanged.
      return { value: parsed, violations: after };
    } catch {
//Non-JSON string: JSON.parse throws, falling back to the original value is a violation, and the model is repaired.
    }
  }
  return { value: payload, violations: direct };
}

export function handleTurnEnded(seam: SubmitSeam, instance: InstanceRef, finalText: string): void {
  const node = seam.liveNode(instance);
  if (node === undefined || node.settled) return;
  if (!node.spec.typed) {
    seam.settleOk(node, finalText);
    return;
  }
  if (node.nudgesRemaining > 0) {
    node.nudgesRemaining--;
    seam.host.driver.respondToSubmit(instance, { kind: "nudge" });
    seam.host.record({ type: "node-nudged", instance });
    return;
  }
  seam.host.driver.cancelAsk(instance);
  seam.settleFailed(
    node,
    new WorkflowError(
      "ResultNotSubmitted",
      "The typed ask ended without a submit_result call, so there is no result.",
      { finalText },
    ),
  );
}
