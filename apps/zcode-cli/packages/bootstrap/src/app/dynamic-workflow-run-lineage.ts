// ============================================================
// Lineage reading surface of Dynamic Workflow Run (a small piece of the observation surface)
// ============================================================
// Pointers at both ends of the revision chain: `resumedFrom` points to the predecessor,
// `supersededBy` points to the successor that stopped itself. The three sections of snapshot, list, and details must carry these two keys, and the rules only need to be written once.
// Detached from dynamic-workflow-run-observation.ts because that file has reached the 400-line limit of oxlint.

import type { RunStatus } from "@zcode/dynamic-workflow";

/** Only see the final view of the two keys required for lineage: the `terminal` and journal lines of the registry entry satisfy it. */
interface SupersedableSettlement {
  status: RunStatus;
  supersededBy?: string;
}

/**
* Success pointer: memory final state first, followed by journal line; only meaningful for stopped(superseded) - even in other states
* Payloads with this key (should not happen) are also read as absent, lest a completed run be drawn as "replaced".
 */
export function supersededByOf(
  entry: { terminal?: SupersedableSettlement } | undefined,
  record: { status?: RunStatus; supersededBy?: string } | undefined,
): string | undefined {
  if (entry?.terminal !== undefined) {
    return entry.terminal.status === "stopped" ? entry.terminal.supersededBy : undefined;
  }
  return record?.status === "stopped" ? record.supersededBy : undefined;
}

/** Appears only if two pointers are present (absent is read as "none of this end", while an `undefined` value will cause each line to have a noise key). */
export function lineageFields(
  resumedFrom: string | undefined,
  supersededBy: string | undefined,
): { resumedFrom?: string; supersededBy?: string } {
  return {
    ...(resumedFrom === undefined ? {} : { resumedFrom }),
    ...(supersededBy === undefined ? {} : { supersededBy }),
  };
}
