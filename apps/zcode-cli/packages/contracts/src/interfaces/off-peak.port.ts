// ============================================================
// Off-Peak Port - idle-time task creation boundary
// ============================================================
// Alongside its AutomationPort sibling. create returns a discriminant union instead of throwing an error: failure classification
// (Quota 3103 / Qualification 3101 / Network, etc.) must be authenticated to the handler across the CLI↔host protocol,
// For the model to receive stable and actionable error prompts, downgrading to message string judgment is prohibited.

import type { OffPeakCreateInput, OffPeakTaskSummary } from "../tools/off-peak.js";

export type OffPeakCreateFailureStage = "client_validation" | "ticket_request" | "local_persist";

export type OffPeakCreateErrorCategory =
  | "client_validation"
  | "eligibility_3101"
  | "quota_3103"
  | "network"
  | "invalid_response"
  | "local_persist"
  | "unknown";

export type OffPeakCreateOutcome =
  | { ok: true; task: OffPeakTaskSummary }
  | {
      ok: false;
      failureStage: OffPeakCreateFailureStage;
      errorCategory: OffPeakCreateErrorCategory;
      errorCode: string;
    };

export function isOffPeakQuotaFailure(
  outcome: OffPeakCreateOutcome,
): outcome is Extract<OffPeakCreateOutcome, { ok: false }> {
  return !outcome.ok && outcome.errorCategory === "quota_3103";
}

export function isOffPeakEligibilityFailure(
  outcome: OffPeakCreateOutcome,
): outcome is Extract<OffPeakCreateOutcome, { ok: false }> {
  return !outcome.ok && outcome.errorCategory === "eligibility_3101";
}

export interface OffPeakCreateContext {
/** The session where the current tool is called; as the binding session for idle tasks (the first run resumes this session, aligning with CronCreate targetTaskId). */
  sessionId?: string;
}

export interface OffPeakPort {
  create(input: OffPeakCreateInput, context?: OffPeakCreateContext): Promise<OffPeakCreateOutcome>;
  list(): Promise<OffPeakTaskSummary[]>;
}
