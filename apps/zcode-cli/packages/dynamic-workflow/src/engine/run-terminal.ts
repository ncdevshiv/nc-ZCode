// ============================================================
// Structured details of run final state
// ============================================================
// Extracted from types.ts (that file is worth 400 lines of lint gate): `ProviderStop` error code details and run-level stop observation
//The payload of the event. Vocabulary (RunStatus/RunStopReason/WorkflowErrorCode) is still in types.ts, here only
// "Details" - they do not participate in union types, and their removal does not affect the exhaustive consumer.

/**
* Structured details of `ProviderStop`: notifications and
* GetWorkflowRun selects copy based on this and never parses it backwards from message. `reason` is for contracts
* `ModelFailureReason` value (`auth_failed` / `invalid_request` / `rate_limited` ...), pure package does not import
* contracts are string; `kind` is the judgment key of the policy table (authentication / not configured / model unavailable / invalid request /
* Quota/Guide), use it instead of reason when reading the copy sheet.
 */
export interface ProviderStopDetails {
  kind: "auth" | "not_configured" | "model_unavailable" | "invalid_request" | "quota" | "other";
  reason: string;
  providerId?: string;
  providerLabel?: string;
  modelId?: string;
  providerCode?: string;
/** The subagent that triggered the stop (`refToString(instance)`) and its name / birth stage (if any). */
  subagent?: string;
  subagentName?: string;
  phase?: string;
/** Original text of provider, verbatim (bounded, truncated by driver). */
  rawMessage?: string;
/** Quota class: The reset time (epoch ms) given by the provider is present only if it can be parsed. */
  resetAt?: number;
}

/**
* Run-level stall: The driver observes that this run continues
* `sinceMs` milliseconds without any successful model request, during which at least one retry was scheduled. Pure observation events, the engine only
* `record()`; core sends a run notification according to it. `reason` is the majority retry reason during the period (`ModelRetryReason`
* value, open string), `cap` is the cap of the provider bucket at this moment.
 */
export interface RunStallInfo {
  sinceMs: number;
  reason?: string;
  cap?: number;
}
