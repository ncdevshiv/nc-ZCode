/**
* engine.ts reaches the upper limit of oxlint max-lines (400 lines), and changes the release path of `report()` (serialization probe,
* replay deduplication, tag verification, two upper limits, write once) is split into this file; the public side is still exported from engine.ts.
 *
* Free function to read and write engine state via {@link EngineState} seam; WorkflowEngine.report is just a thin delegate.
 */

import { inputHash } from "./hash.js";
import { REPORT_CAPS } from "../facade/report-caps.js";
import { declaredPresetIds, isDeclaredPreset } from "./engine-artifacts.js";
import { hashMismatch } from "./scheduler.js";
import type { EngineState } from "./engine-state.js";
import type { InstanceRef } from "./types.js";
import { refToString, WorkflowError } from "./types.js";

/**
* Publish an intermediate result (Boundary A's `report`). Synchronous, no return value, no driver round trip:
* Drop a line in journal + send an event and end.
 *
* The order of these four things is important:
 *
* 1. **Serialization probe goes first**. Use `JSON.stringify` to get two things at once: whether item can be JSON
* means, and its number of bytes (a measure of the upper limit). Must precede canonicalJson - a ringed item will make
* canonicalJson recursively explodes the stack, which is a crash rather than a readable failure.
* 2. **replay skips when hit**: no event is sent, no record is rewritten. Compare inputHash before skipping, if inconsistent, press
* Purity default makes run fail loudly. This comparison is **defensive** (a report derived from one that the journal has pinned
* value), but it's free, and the offset here means that the entire replay is unreliable - readers of the Results panel never
* That's the kind of thing that should be seen without knowing it.
* 3. **The upper limit precedes the dropout**: If either the number of entries or the number of bytes in a single entry exceeds `ReportCapExceeded`, the entire run will fail.
* run level instead of node level, because `report` returns `void` and there is no way to deny it.
* 4. **Write once**: `completed`, no actor field.
 */
export function publishReport(
  state: EngineState,
  siteId: string,
  item: unknown,
  artifactId?: string,
): void {
  if (state.isRunSettled()) return; // 与 log 同：结算之后不再受理
  const ordinal = state.nextOrdinal(siteId);
  const instance: InstanceRef = { siteId, ordinal };

  const serialized = probeReportItem(state, instance, item);
  if (serialized === undefined) return; // 探针已 failRun
// inputHash deliberately **only covers item**, and labels are not hashed: labels are compile-time literals on the site, and resume
// Scripts are required to be the same byte by byte (script_hash), so tags with the same (siteId, ordinal) cannot change. add it to
// The only effect of hash input is to make **each** unlabeled report in the existing journal appear when resume
// InputHashMismatch - A destructive load deformation with no gain.
  const hash = inputHash(item);

  const recorded = state.journal.getNode(state.runId, siteId, ordinal);
  if (recorded !== undefined) {
    if (recorded.inputHash !== hash) {
      state.failRun(hashMismatch(instance, recorded.inputHash, hash));
      return;
    }
    return; // replay 去重：静默跳过（无事件、不重复 append）
  }

// Tag present: it must have been declared as a **preset** product. Check before the upper limit - a label pointing to a non-existent board
// The script is written incorrectly, not the capacity event. Mixing the two together will make the error code deceptive.
// failRun instead of reject for exactly the same reason as report's two upper bounds: void returns no reject channel.
  if (artifactId !== undefined && !isDeclaredPreset(state, artifactId)) {
    state.failRun(
      new WorkflowError(
        "ArtifactUndeclared",
        `report() tag "${artifactId}" is not a declared preset artifact ` +
          `(at ${refToString(instance)}). Declare it once at the top of the script, e.g. ` +
          `artifact.chart("${artifactId}", …), before tagging reports with it. Declared presets: ` +
          `${declaredPresetIds(state).join(", ") || "(none)"}.`,
      ),
    );
    return;
  }

  if (!reserveReport(state, instance, serialized)) return;
  state.journal.putNode({
    runId: state.runId,
    siteId,
    ordinal,
    kind: "report",
    inputHash: hash,
    status: "completed",
    result: item,
//The labeled report line is the placement point of the invariant "Kanban = projection of journal": each point of a Kanban board
// Just one line kind = report ∧ artifact_id = the id.
    ...(artifactId === undefined ? {} : { artifactId }),
  });
  state.record({
    type: "report",
    instance,
    item,
    ...(artifactId === undefined ? {} : { artifactId }),
  });
}

/**
* Runtime JSON guardrails for * report (serializable diagnostics at compile time are suspenders, here belt).
* Returns the serialized text of item; failRun and returns undefined if not representable.
 *
* Why use `JSON.stringify` instead of `canonicalJson`: canonicalJson is **full function** (put
* undefined/The function is silently folded into `"null"`, and the stack is recursively exploded when it encounters a loop). This is exactly what it should look like as a hash input.
* But as a guardrail, it will quietly drop a damaged item into the journal. `JSON.stringify` opposite: ring vs. bigint
* Throw error, undefined/function/symbol return undefined - both cases can be caught as a loud failure.
 *
* Select `DriverError` instead of `ReportCapExceeded` for the error code: this is not an upper limit (the upper limit is the number), but
* The contract "item is not JSON" is broken, and the main gate of that contract is compile-time diagnosis. Coming here means there is something
* bypasses the main gate (typically a ring created by `any`), so it is a contract break rather than a capacity event -
* Let `ReportCapExceeded` only represent the upper limit so readers can act accordingly.
 */
function probeReportItem(
  state: EngineState,
  instance: InstanceRef,
  item: unknown,
): string | undefined {
  let serialized: string | undefined;
  try {
    serialized = JSON.stringify(item);
  } catch (cause) {
    state.failRun(
      new WorkflowError(
        "DriverError",
        `report() item cannot be serialized to JSON (a cycle or a bigint) at ` +
          `${refToString(instance)}. Report a plain JSON value.`,
        { cause },
      ),
    );
    return undefined;
  }
  if (serialized === undefined) {
// JSON.stringify returns undefined for undefined / function / symbol.
    state.failRun(
      new WorkflowError(
        "DriverError",
        `report() item is not a JSON value (undefined, a function or a symbol) at ` +
          `${refToString(instance)}. Report a plain JSON value.`,
      ),
    );
    return undefined;
  }
  return serialized;
}

/** Two upper limits for report (number of entries, number of bytes in a single entry). Any exceedance failsRun and returns false. */
function reserveReport(state: EngineState, instance: InstanceRef, serialized: string): boolean {
  const bytes = utf8ByteLength(serialized);
  if (bytes > REPORT_CAPS.maxItemSerializedBytes) {
    state.failRun(
      new WorkflowError(
        "ReportCapExceeded",
        `report() item at ${refToString(instance)} is ${bytes} bytes, over the ` +
          `${REPORT_CAPS.maxItemSerializedBytes}-byte limit. Report a summary instead.`,
      ),
    );
    return false;
  }
  if (state.reportCount() >= REPORT_CAPS.maxItemsPerRun) {
    state.failRun(
      new WorkflowError(
        "ReportCapExceeded",
        `This run already reported ${REPORT_CAPS.maxItemsPerRun} items, the maximum. ` +
          `Report findings, not chatter.`,
      ),
    );
    return false;
  }
  state.countReport();
  return true;
}

/**
* The number of UTF-8 bytes in a string (a measure of the report's single upper limit). Use `TextEncoder` (ECMAScript/WHATWG
* standard global) instead of `Buffer.byteLength`: this package maintains zero node built-in dependencies. The upper limit is about bytes, not
* Characters - The number of characters in a Chinese finding is only one-third of the number of bytes, which makes the upper limit useless in terms of characters.
 */
function utf8ByteLength(value: string): number {
  return new TextEncoder().encode(value).length;
}
