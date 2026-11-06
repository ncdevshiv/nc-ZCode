import type { AiSdkProviderKind, ProviderBusinessErrorOptions } from "./model-execution.js";
import { ProviderBusinessError } from "./model-execution.js";
import { getHttpResponseStatus, getResponseHeaders } from "./failure-inspection.js";
import { asRecord, stringProperty } from "./runner-record.js";

const BIGMODEL_BRACKETED_BUSINESS_CODE_PATTERN = /^\[(\d{4})\](?=\[)/;
const PROVIDER_BUSINESS_ERROR_WRAPPER_CODE = "PROVIDER_BUSINESS_ERROR";

interface DetectProviderBusinessFinishErrorOptions {
  providerId: string;
  providerKind?: AiSdkProviderKind | string;
  source: unknown;
}

export function detectProviderBusinessFinishError(
  options: DetectProviderBusinessFinishErrorOptions,
): ProviderBusinessError | undefined {
  const record = asRecord(options.source);
  if (!isFinishLikeRecord(record)) {
    return undefined;
  }

  const rawFinishReason = readRawFinishReason(record);
  const extracted = extractBusinessFailurePayload(record);
  const errorPayload = asRecord(record.error);
  const providerCode =
    extracted.providerCode ??
    normalizeStringish(record.providerCode) ??
    normalizeStringish(errorPayload?.code);

// The error chunk of AI SDK often contains ordinary Error.message (such as rate limited) and has no business code;
// Just relying on providerMessage hits will misjudge retryable current limiting as ProviderBusinessError.
  const hasBusinessSignal =
    providerCode !== undefined ||
    rawFinishReason === "provider_success_false" ||
    record.isProviderBusinessError === true ||
    record.name === "ProviderBusinessError";

  if (!hasBusinessSignal) {
    return undefined;
  }

  const errorOptions: ProviderBusinessErrorOptions = {
    providerId: options.providerId,
    providerKind: normalizeProviderKind(options.providerKind),
    providerCode,
    providerMessage:
      extracted.providerMessage ??
      normalizeStringish(record.providerMessage) ??
      normalizeStringish(record.message) ??
      (rawFinishReason === "provider_success_false"
        ? "Provider returned a business error."
        : undefined),
    providerRequestId: extracted.providerRequestId,
    responseBodySummary: extracted.responseBodySummary,
// The AI ​​SDK's error chunk will repackage the fetch layer business errors;
// Rebuilding ProviderBusinessError without responseHeaders will cause retry-after to be lost before retrying the calculation.
    responseHeaders: extracted.responseHeaders,
    responseStatus: extracted.responseStatus,
    statusCode: extracted.statusCode,
  };

  return new ProviderBusinessError(errorOptions);
}

function normalizeProviderKind(value: AiSdkProviderKind | string | undefined): AiSdkProviderKind {
  return value === "openai" || value === "anthropic" || value === "openai-compatible"
    ? value
    : "openai-compatible";
}

function isFinishLikeRecord(record: Record<string, unknown>): boolean {
  const chunkType = stringProperty(record, "type");
  if (chunkType === "finish" || chunkType === "error") {
    return true;
  }

  if (stringProperty(record, "finishReason") || readRawFinishReason(record)) {
    return true;
  }

  if (record.isProviderBusinessError === true || record.name === "ProviderBusinessError") {
    return true;
  }

  return false;
}

function readRawFinishReason(record: Record<string, unknown>): string | undefined {
  return (
    stringProperty(record, "rawFinishReason") ??
    stringProperty(asRecord(record.providerMetadata), "rawFinishReason")
  );
}

function extractBusinessFailurePayload(record: Record<string, unknown>) {
  const candidates = collectCandidateRecords(record);
  const providerCode = candidates.map(readProviderCode).find(Boolean);
  const providerMessage = candidates.map(readProviderMessage).find(Boolean);
  const providerRequestId = candidates.map(readProviderRequestId).find(Boolean);
  const responseHeaders = candidates.map(getResponseHeaders).find(Boolean);
  const responseBodySummary = candidates.find(hasBusinessSignal);
  const responseStatus = candidates.map(getHttpResponseStatus).find((value) => value !== undefined);
  const statusCode = candidates.map(readStatusCode).find((value) => value !== undefined);

  return {
    providerCode,
    providerMessage,
    providerRequestId,
    responseHeaders,
    responseBodySummary,
    responseStatus,
    statusCode,
  };
}

function collectCandidateRecords(record: Record<string, unknown>): Record<string, unknown>[] {
  const result: Record<string, unknown>[] = [];
  const seen = new WeakSet<object>();
  const queue: unknown[] = [record];

  while (queue.length > 0) {
    const current = queue.shift();
    if (!current || typeof current !== "object") {
      continue;
    }
    if (seen.has(current)) {
      continue;
    }
    seen.add(current);

    if (Array.isArray(current)) {
      for (const item of current) {
        queue.push(item);
      }
      continue;
    }

    const currentRecord = current as Record<string, unknown>;
    result.push(currentRecord);

    queue.push(
      currentRecord.providerMetadata,
      currentRecord.response,
      asRecord(currentRecord.response).body,
      currentRecord.body,
      currentRecord.error,
      currentRecord.data,
      currentRecord.choices,
    );

// Business errors such as zcode-plan may only appear in the deep JSON (such as response.body) of the AI ​​SDK finish chunk.
// Scanning only along the fixed field chain will miss 3007, and eventually core will be misjudged as suspicious empty.
    for (const nested of Object.values(currentRecord)) {
      if (nested && typeof nested === "object") {
        queue.push(nested);
      }
    }
  }

  return result;
}

function readProviderCode(record: Record<string, unknown>): string | undefined {
  const errorRecord = asRecord(record.error);
  const contextRecord = asRecord(record.context);
  const value =
    normalizeProviderCode(record.providerCode) ??
    normalizeProviderCode(errorRecord?.providerCode) ??
    normalizeProviderCode(contextRecord?.providerCode) ??
    normalizeProviderCode(record.error_code) ??
    normalizeProviderCode(errorRecord?.error_code) ??
// When ProviderBusinessError enters the AI ​​SDK chunk for the second time, the outer code is the packaging type;
// The real upstream code is in providerCode/nested body, and the packaging code cannot be truncated and scanned in advance.
    normalizeProviderCode(record.code) ??
    normalizeProviderCode(errorRecord?.code) ??
    normalizeProviderCode(contextRecord?.code) ??
// BigModel/Z.AI's SSE error chunk sometimes only has `[1302][...][request_id]` message,
// There is no structured code; only this strong format prefix is ​​parsed to avoid misjudgment of ordinary rate limit copywriting as business code.
    readBigModelBracketedBusinessCode(record.message) ??
    readBigModelBracketedBusinessCode(record.providerMessage) ??
    readBigModelBracketedBusinessCode(errorRecord?.message) ??
    readBigModelBracketedBusinessCode(errorRecord?.providerMessage) ??
    readBigModelBracketedBusinessCode(contextRecord?.message) ??
    readBigModelBracketedBusinessCode(contextRecord?.providerMessage);
  return value;
}

function readProviderMessage(record: Record<string, unknown>): string | undefined {
  const errorRecord = asRecord(record.error);
  const contextRecord = asRecord(record.context);
  return (
    normalizeStringish(record.msg) ??
    normalizeStringish(record.providerMessage) ??
    normalizeStringish(record.message) ??
    normalizeStringish(errorRecord?.msg) ??
    normalizeStringish(errorRecord?.providerMessage) ??
    normalizeStringish(errorRecord?.message) ??
    normalizeStringish(contextRecord?.providerMessage) ??
    normalizeStringish(contextRecord?.msg) ??
    normalizeStringish(contextRecord?.message)
  );
}

function readProviderRequestId(record: Record<string, unknown>): string | undefined {
  const errorRecord = asRecord(record.error);
  return (
    normalizeStringish(record.request_id) ??
    normalizeStringish(record.requestId) ??
    normalizeStringish(record.id) ??
    normalizeStringish(errorRecord.request_id) ??
    normalizeStringish(errorRecord.requestId) ??
    normalizeStringish(errorRecord.id)
  );
}

function readStatusCode(record: Record<string, unknown>): number | undefined {
  return toFiniteNumber(record.statusCode) ?? toFiniteNumber(record.status);
}

function hasBusinessSignal(record: Record<string, unknown>): boolean {
  return Boolean(
    readProviderCode(record) ||
    readProviderMessage(record) ||
    (Array.isArray(record.allowed_models) && record.allowed_models.length > 0),
  );
}

function normalizeStringish(value: unknown): string | undefined {
  if (typeof value === "number" && Number.isFinite(value)) {
    return String(Math.trunc(value));
  }
  if (typeof value !== "string") {
    return undefined;
  }

  const normalized = value.trim();
  return normalized.length > 0 ? normalized : undefined;
}

function normalizeProviderCode(value: unknown): string | undefined {
  const normalized = normalizeStringish(value);
  return normalized?.toUpperCase() === PROVIDER_BUSINESS_ERROR_WRAPPER_CODE
    ? undefined
    : normalized;
}

function readBigModelBracketedBusinessCode(value: unknown): string | undefined {
  const message = normalizeStringish(value);
  const match = message?.match(BIGMODEL_BRACKETED_BUSINESS_CODE_PATTERN);
  return match?.[1];
}

function toFiniteNumber(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}
