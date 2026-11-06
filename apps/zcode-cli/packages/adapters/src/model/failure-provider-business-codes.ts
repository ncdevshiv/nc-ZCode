import {
  ModelErrorCode,
  ModelFailureReason as ModelFailureReasonValue,
  ModelRetryReason as ModelRetryReasonValue,
  type ModelErrorCode as ModelErrorCodeType,
  type ModelFailureReason,
  type ModelRetryReason,
} from "@zcode/contracts";
import { isNetworkFailure, isTimeoutFailure } from "./failure-inspection.js";
import type { ProviderBusinessError } from "./model-execution.js";

interface ProviderBusinessCodeMapping {
  code: ModelErrorCodeType;
  message?: string;
  reason: ModelFailureReason;
  retryReason: ModelRetryReason;
  retryable: boolean;
}

// Some OpenAI-compatible providers will package recoverable upstream network failures into business codes.
const PROVIDER_NETWORK_BUSINESS_CODES = new Set(["1234"]);
const PROVIDER_INTERNAL_NETWORK_MESSAGES = new Set([
  "500 internal network error",
  "internal network error",
  "internal network failure",
]);

const TERMINAL_RATE_LIMIT_MAPPING: ProviderBusinessCodeMapping = {
  code: ModelErrorCode.ModelRateLimited,
  reason: ModelFailureReasonValue.RateLimited,
  retryReason: ModelRetryReasonValue.RateLimited,
  retryable: false,
};
const TERMINAL_BUSINESS_MAPPING: ProviderBusinessCodeMapping = {
  code: ModelErrorCode.ModelRequestFailed,
  reason: ModelFailureReasonValue.Unknown,
  retryReason: ModelRetryReasonValue.NetworkError,
  retryable: false,
};
const RETRYABLE_RATE_LIMIT_MAPPING: ProviderBusinessCodeMapping = {
  code: ModelErrorCode.ModelRateLimited,
  reason: ModelFailureReasonValue.RateLimited,
  retryReason: ModelRetryReasonValue.RateLimited,
  retryable: true,
};
const RETRYABLE_OVERLOAD_MAPPING: ProviderBusinessCodeMapping = {
  code: ModelErrorCode.ModelRequestFailed,
  reason: ModelFailureReasonValue.ProviderOverloaded,
  retryReason: ModelRetryReasonValue.ProviderOverloaded,
  retryable: true,
};

const PROVIDER_BUSINESS_CODE_MAPPINGS = new Map<string, ProviderBusinessCodeMapping>([
  [
    "500",
    {
      code: ModelErrorCode.ModelRequestFailed,
      reason: ModelFailureReasonValue.ServerError,
      retryReason: ModelRetryReasonValue.ServerError,
      retryable: true,
    },
  ],
  [
    "1006",
    {
      code: ModelErrorCode.ProviderNotConfigured,
      reason: ModelFailureReasonValue.AuthFailed,
      retryReason: ModelRetryReasonValue.AuthRefresh,
      retryable: false,
    },
  ],
  [
    "1005",
    {
      code: ModelErrorCode.ModelRequestFailed,
      reason: ModelFailureReasonValue.InvalidRequest,
      retryReason: ModelRetryReasonValue.NetworkError,
      retryable: false,
    },
  ],
  [
    "3006",
    {
      code: ModelErrorCode.ModelNotFound,
      reason: ModelFailureReasonValue.InvalidRequest,
      retryReason: ModelRetryReasonValue.NetworkError,
      retryable: false,
    },
  ],
  [
    "3001",
    {
      code: ModelErrorCode.InvalidModelRequest,
      reason: ModelFailureReasonValue.InvalidRequest,
      retryReason: ModelRetryReasonValue.NetworkError,
      retryable: false,
    },
  ],
  [
    "3007",
    {
      code: ModelErrorCode.InvalidModelRequest,
      reason: ModelFailureReasonValue.AuthFailed,
      retryReason: ModelRetryReasonValue.AuthRefresh,
      retryable: false,
    },
  ],
// 3008/3009/3010: Concurrency upper limit, similar to quota exhaustion 1005 but does not use refresh-quota, but uses upgrade banner.
  [
    "3008",
    {
      code: ModelErrorCode.ModelRateLimited,
      reason: ModelFailureReasonValue.RateLimited,
      retryReason: ModelRetryReasonValue.RateLimited,
      retryable: false,
    },
  ],
  [
    "3009",
    {
      code: ModelErrorCode.ModelRateLimited,
      reason: ModelFailureReasonValue.RateLimited,
      retryReason: ModelRetryReasonValue.RateLimited,
      retryable: false,
    },
  ],
// 3010: Current model concurrency upper limit. Reserved as a non-automatic retry rate limited, the UI guides model switching or upgrades.
  [
    "3010",
    {
      code: ModelErrorCode.ModelRateLimited,
      reason: ModelFailureReasonValue.RateLimited,
      retryReason: ModelRetryReasonValue.RateLimited,
      retryable: false,
    },
  ],
// Recovery errors in the BigModel document need to be explicitly entered into the table; at the same time, mark 1261 as a super window.
// Long-term quotas, package permissions, fair usage restrictions and provider explicit termination business codes are explicitly terminated to avoid generic 429 false retries.
  [
    "1120",
    {
      code: ModelErrorCode.ModelRequestFailed,
      reason: ModelFailureReasonValue.ServerError,
      retryReason: ModelRetryReasonValue.ServerError,
      retryable: true,
    },
  ],
  [
    "1230",
    {
      code: ModelErrorCode.ModelRequestFailed,
      reason: ModelFailureReasonValue.ServerError,
      retryReason: ModelRetryReasonValue.ServerError,
      retryable: true,
    },
  ],
  [
    "1234",
    {
      code: ModelErrorCode.ModelRequestFailed,
      reason: ModelFailureReasonValue.NetworkError,
      retryReason: ModelRetryReasonValue.NetworkError,
      retryable: true,
    },
  ],
  [
    "1261",
    {
      code: ModelErrorCode.ModelContextExceeded,
      reason: ModelFailureReasonValue.ContextExceeded,
      retryReason: ModelRetryReasonValue.NetworkError,
      retryable: false,
    },
  ],
  [
    "1113",
    {
      code: ModelErrorCode.ModelRequestFailed,
      reason: ModelFailureReasonValue.Unknown,
      retryReason: ModelRetryReasonValue.NetworkError,
      retryable: false,
    },
  ],
  [
    "1302",
    {
      code: ModelErrorCode.ModelRateLimited,
      reason: ModelFailureReasonValue.RateLimited,
      retryReason: ModelRetryReasonValue.RateLimited,
      retryable: true,
    },
  ],
  [
    "1303",
    {
      code: ModelErrorCode.ModelRateLimited,
      reason: ModelFailureReasonValue.RateLimited,
      retryReason: ModelRetryReasonValue.RateLimited,
      retryable: true,
    },
  ],
  [
    "1305",
    {
      code: ModelErrorCode.ModelRateLimited,
      reason: ModelFailureReasonValue.RateLimited,
      retryReason: ModelRetryReasonValue.RateLimited,
      retryable: true,
    },
  ],
  [
    "1304",
    {
      code: ModelErrorCode.ModelRateLimited,
      reason: ModelFailureReasonValue.RateLimited,
      retryReason: ModelRetryReasonValue.RateLimited,
      retryable: false,
    },
  ],
  [
    "1308",
    {
      code: ModelErrorCode.ModelRateLimited,
      reason: ModelFailureReasonValue.RateLimited,
      retryReason: ModelRetryReasonValue.RateLimited,
      retryable: false,
    },
  ],
  [
    "1309",
    {
      code: ModelErrorCode.ModelRequestFailed,
      reason: ModelFailureReasonValue.Unknown,
      retryReason: ModelRetryReasonValue.NetworkError,
      retryable: false,
    },
  ],
  [
    "1310",
    {
      code: ModelErrorCode.ModelRateLimited,
      reason: ModelFailureReasonValue.RateLimited,
      retryReason: ModelRetryReasonValue.RateLimited,
      retryable: false,
    },
  ],
  [
    "1311",
    {
      code: ModelErrorCode.ModelRequestFailed,
      reason: ModelFailureReasonValue.Unknown,
      retryReason: ModelRetryReasonValue.NetworkError,
      retryable: false,
    },
  ],
  [
    "1312",
    {
      code: ModelErrorCode.ModelRequestFailed,
      reason: ModelFailureReasonValue.ProviderOverloaded,
      retryReason: ModelRetryReasonValue.ProviderOverloaded,
      retryable: true,
    },
  ],
  [
    "1313",
    {
      code: ModelErrorCode.ModelRateLimited,
      reason: ModelFailureReasonValue.RateLimited,
      retryReason: ModelRetryReasonValue.RateLimited,
      retryable: false,
    },
  ],
  [
    "3002",
    {
      code: ModelErrorCode.ModelRateLimited,
      reason: ModelFailureReasonValue.RateLimited,
      retryReason: ModelRetryReasonValue.RateLimited,
      retryable: true,
    },
  ],
  [
    "2007",
    {
      code: ModelErrorCode.ModelRequestFailed,
      reason: ModelFailureReasonValue.ServerError,
      retryReason: ModelRetryReasonValue.ServerError,
      retryable: true,
    },
  ],
]);

// These codes are all from the official documentation of the provider, and the semantics require the user to recharge, adjust the package, or wait for the long-term quota to be reset.
// Only consume AI SDK / existing ProviderBusinessError exposed code, do not parse the manufacturer's original response field here.
// insufficient_quota once fell into the generic 429 retry; its termination semantics can no longer rely on the Retry-After duration.
for (const code of [
  "insufficient_quota",
  "credit_balance_exhausted",
  "organization_spend_limit_exceeded",
  "project_spend_limit_exceeded",
  "organization_usage_limit_exceeded",
  "exceeded_current_quota_error",
  "2056",
  "20097",
  "1316",
  "1317",
  "1318",
  "1319",
  "1320",
  "1321",
]) {
  PROVIDER_BUSINESS_CODE_MAPPINGS.set(code, TERMINAL_RATE_LIMIT_MAPPING);
}

for (const code of ["1008", "1314", "1315"]) {
  PROVIDER_BUSINESS_CODE_MAPPINGS.set(code, TERMINAL_BUSINESS_MAPPING);
}

for (const code of ["rate_limit_reached_error", "rate_limit_error"]) {
  PROVIDER_BUSINESS_CODE_MAPPINGS.set(code, RETRYABLE_RATE_LIMIT_MAPPING);
}

for (const code of ["engine_overloaded_error", "overloaded_error"]) {
  PROVIDER_BUSINESS_CODE_MAPPINGS.set(code, RETRYABLE_OVERLOAD_MAPPING);
}

export function getProviderBusinessCodeMapping(
  providerCode: string,
): ProviderBusinessCodeMapping | undefined {
  return PROVIDER_BUSINESS_CODE_MAPPINGS.get(providerCode);
}

export function isRetryableProviderBusinessNetworkFailure(
  error: ProviderBusinessError,
  providerCode: string | undefined,
): boolean {
  if (providerCode) {
// The SSE error chunk of AI SDK will wrap the underlying ECONNRESET into ProviderBusinessError.providerCode.
// This is essentially a transport layer disconnection, and network error retry semantics must be used instead of unknown.
    if (isNetworkFailure(providerCode)) {
      return true;
    }
    if (PROVIDER_NETWORK_BUSINESS_CODES.has(providerCode)) {
      return true;
    }

    const normalizedCode = providerCode.toLowerCase();
    if (normalizedCode === "network_error" || normalizedCode === "network_error_retryable") {
      return true;
    }
  }

  return isProviderBusinessInternalNetworkFailure(error);
}

export function isRetryableProviderBusinessTimeoutFailure(
  error: ProviderBusinessError,
  providerCode: string | undefined,
  statusCode?: number,
): boolean {
// AI SDK/SSE error chunk may wrap underlying headers/body timeout into ProviderBusinessError.providerCode.
// This type of error has no real provider business semantics and must retain the retryable semantics of ordinary timeout.
  return isTimeoutFailure(
    error,
    providerCode,
    statusCode ?? error.statusCode ?? error.responseStatus,
  );
}

function isProviderBusinessInternalNetworkFailure(error: ProviderBusinessError): boolean {
  if (readResponseBodyErrorType(error.responseBodySummary) !== "api_error") {
    return false;
  }

  const normalizedMessage =
    normalizeProviderMessage(error.providerMessage) ??
    normalizeProviderMessage(error.message) ??
    normalizeProviderMessage(readResponseBodyErrorMessage(error.responseBodySummary));
  return normalizedMessage ? PROVIDER_INTERNAL_NETWORK_MESSAGES.has(normalizedMessage) : false;
}

function readResponseBodyErrorType(summary: unknown): string | undefined {
  return normalizeProviderMessage(asRecord(asRecord(summary).error).type);
}

function readResponseBodyErrorMessage(summary: unknown): string | undefined {
  return stringValue(asRecord(asRecord(summary).error).message);
}

function normalizeProviderMessage(value: unknown): string | undefined {
  const text = stringValue(value)?.trim().toLowerCase();
  return text && text.length > 0 ? text : undefined;
}

function stringValue(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

function asRecord(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === "object" ? (value as Record<string, unknown>) : {};
}
