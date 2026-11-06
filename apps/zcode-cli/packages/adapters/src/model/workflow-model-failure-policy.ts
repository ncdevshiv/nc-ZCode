/**
* Model-side error policy table within workflow.
 *
* The classifier (failure-classifier.ts) answers "Does the main conversation need to be automatically retried?"; this table answers
* "Would the workflow stop and find someone?" The two tables have opposite answers on 3008/3009/3010 (concurrency limit), which is exactly
* The reason for the existence of this module: workflow sub-agent and tool-side requests hold an unlimited retry budget (`modelRetryBudget`),
* On them, only the **deterministic** errors that require people to solve are worth stopping the run; everything else (including classifier judgment
*Unknown service codes, TLS, 5xx that cannot be retried) will all be retried, and stall notifications will be used as escape hatches.
 *
* runner (retry gate of attempt loop) and bootstrap driver (classify reject as stopRun / askFailed)
* Share the **same** function: the failure of the runner to judge retry will never be reported to the driver as stop; the failure of the runner to judge stop
* The policy table must determine stop when reaching the driver (same function, same input).
 */

import { ModelErrorCode, ModelFailureReason } from "@zcode/contracts";
import type { ClassifiedModelFailure } from "./failure-classifier.js";
import { isRetryableFailure } from "./failure-classifier.js";
import type { ModelRetryBudget } from "@zcode/contracts";
import { isUnboundedRetryBudget } from "./retry-budget.js";

/** The judgment key of `ProviderStop`: informs the copy table to select sentences by it (not by reason). */
export type WorkflowProviderStopKind =
  | "auth"
  | "not_configured"
  | "model_unavailable"
  | "invalid_request"
  | "quota"
  | "other";

/**
* Quota business code (one row in the Stop set). with failure-provider-business-codes.ts
* TERMINAL_RATE_LIMIT_MAPPING is intentionally **maintained separately**: which table determines whether the main conversation will automatically retry, this table determines
* The workflow stops and waits for quota reset/recharge. 1005 is invalid_request in the classifier. It must be judged by code first.
* Please refer to reason for quota.
 */
export const WORKFLOW_QUOTA_PROVIDER_CODES: ReadonlySet<string> = new Set([
  "1005",
  "1308",
  "1310",
  "1313",
  "1316",
  "1317",
  "1318",
  "1319",
  "1320",
  "1321",
  "2056",
  "20097",
  "insufficient_quota",
  "credit_balance_exhausted",
  "organization_spend_limit_exceeded",
  "project_spend_limit_exceeded",
  "organization_usage_limit_exceeded",
  "exceeded_current_quota_error",
]);

/** ModelErrorCode of the "provider is not configured / invalid selection" family (reason is not necessarily provider_not_configured). */
const NOT_CONFIGURED_ERROR_CODES: ReadonlySet<string> = new Set([
  ModelErrorCode.ProviderNotFound,
  ModelErrorCode.ProviderNotConfigured,
  ModelErrorCode.ModelConfigMissing,
  ModelErrorCode.InvalidModelSelection,
  ModelErrorCode.ModelRequestAuthMissing,
]);

export type WorkflowModelFailurePolicy =
  | { decision: "retry" }
  | { decision: "stop"; kind: WorkflowProviderStopKind }
/** This is where core compression fails: the node fails with `ContextLimit`, the script can be caught, otherwise it will be errored. */
  | { decision: "context_exceeded" }
  | { decision: "cancelled" };

/**
* Strategy table ontology. The input is the result of the classifier (read-only code / reason / retryable) and provider business code.
* The order is meaningful: cancel first (it is not an error); quotas are judged by **code**, before reason (the reason of 1005 is
*invalid_request); the rest is by reason/code; everything outside the table = retry.
 */
export function resolveWorkflowModelFailurePolicy(
  failure: Pick<ClassifiedModelFailure, "code" | "reason" | "retryable">,
  providerCode: string | undefined,
): WorkflowModelFailurePolicy {
  if (failure.reason === ModelFailureReason.Cancelled) return { decision: "cancelled" };
  if (providerCode !== undefined && WORKFLOW_QUOTA_PROVIDER_CODES.has(providerCode)) {
    return { decision: "stop", kind: "quota" };
  }
  if (failure.reason === ModelFailureReason.AuthFailed) return { decision: "stop", kind: "auth" };
  if (
    failure.reason === ModelFailureReason.ProviderNotConfigured ||
    NOT_CONFIGURED_ERROR_CODES.has(failure.code)
  ) {
    return { decision: "stop", kind: "not_configured" };
  }
  if (failure.code === ModelErrorCode.ModelNotFound) {
    return { decision: "stop", kind: "model_unavailable" };
  }
  if (failure.reason === ModelFailureReason.ContextExceeded)
    return { decision: "context_exceeded" };
// "Invalid request" only recognizes the provider's rejection of **request** (3001, HTTP 400/422). `invalid_model_response`
// Also marked as invalid_request by the classifier, but that's a **response** parsing failure - it's probably better to ask again, return to retry.
  if (
    failure.reason === ModelFailureReason.InvalidRequest &&
    failure.code !== ModelErrorCode.InvalidModelResponse
  ) {
    return { decision: "stop", kind: "invalid_request" };
  }
  return { decision: "retry" };
}

/**
* Replacement point for runner retry gate: bounded budget or read classifier's `retryable` (the main dialogue remains unchanged); no upper limit
* Budget (workflow traffic) change reading policy table - `retry` can be used to retry, `stop` / `context_exceeded` can not be retried.
* Cancellation is individually short-circuited by the caller before this happens (this is already the case for both runners).
 */
export function retryAllowedByFailurePolicy(
  failure: ClassifiedModelFailure,
  retryBudget: ModelRetryBudget | undefined,
  providerCode: string | undefined,
): boolean {
  if (!isUnboundedRetryBudget(retryBudget)) return isRetryableFailure(failure);
  return resolveWorkflowModelFailurePolicy(failure, providerCode).decision === "retry";
}

/** Driver side reads the fact from adapter error: policy ruling + spells `ProviderStopDetails` to the required field. */
export interface WorkflowModelFailureInspection {
  policy: WorkflowModelFailurePolicy;
/** The value of contracts `ModelFailureReason`. */
  reason: string;
  providerCode?: string;
  providerId?: string;
  modelId?: string;
/** Adapter error message (under the mapped business code is the original text of the provider). */
  rawMessage?: string;
/** The reset time (epoch ms) when the quota class is given Retry-After by the provider. */
  resetAt?: number;
}

/**
* Read adapter error by **shape** (`AiSdkModelAdapterError`: `name` + `context.reason` ...), directly or
* Within one layer of `cause`; there may be two class definitions (dist boundaries) between bootstrap and adapters, and the shape will not drift.
* Returns undefined if it is not a model layer error (the driver is still classified as DriverError).
 */
export function inspectWorkflowModelFailure(
  error: unknown,
): WorkflowModelFailureInspection | undefined {
  const direct = readAdapterError(error);
  const found =
    direct ?? readAdapterError((error as { cause?: unknown } | undefined)?.cause ?? undefined);
  if (found === undefined) return undefined;
  const { context, code, message } = found;
  const reason = stringValue(context.reason);
  if (reason === undefined) return undefined;
  const providerCode = codeValue(context.providerCode);
  const policy = resolveWorkflowModelFailurePolicy(
    {
      code: code as ClassifiedModelFailure["code"],
      reason: reason as ClassifiedModelFailure["reason"],
      retryable: context.retryable === true,
    },
    providerCode,
  );
  const retryAfterMs = context.retryAfterMs;
  const inspection: WorkflowModelFailureInspection = { policy, reason };
  if (providerCode !== undefined) inspection.providerCode = providerCode;
  const providerId = stringValue(context.providerId);
  if (providerId !== undefined) inspection.providerId = providerId;
  const modelId = stringValue(context.modelId);
  if (modelId !== undefined) inspection.modelId = modelId;
  if (message.length > 0) inspection.rawMessage = message;
  if (
    policy.decision === "stop" &&
    policy.kind === "quota" &&
    typeof retryAfterMs === "number" &&
    Number.isFinite(retryAfterMs) &&
    retryAfterMs > 0
  ) {
    inspection.resetAt = Date.now() + retryAfterMs;
  }
  return inspection;
}

function readAdapterError(
  error: unknown,
): { context: Record<string, unknown>; code: string; message: string } | undefined {
  if (typeof error !== "object" || error === null) return undefined;
  const candidate = error as {
    name?: unknown;
    code?: unknown;
    context?: unknown;
    message?: unknown;
  };
  if (candidate.name !== "AiSdkModelAdapterError") return undefined;
  const context =
    typeof candidate.context === "object" && candidate.context !== null
      ? (candidate.context as Record<string, unknown>)
      : {};
  return {
    context,
    code: typeof candidate.code === "string" ? candidate.code : "",
    message: typeof candidate.message === "string" ? candidate.message : "",
  };
}

function stringValue(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

/** The provider business code may be a number (response body as it is) or a string; unified into a string key. */
function codeValue(value: unknown): string | undefined {
  if (typeof value === "number" && Number.isFinite(value)) return String(Math.trunc(value));
  return stringValue(value);
}
