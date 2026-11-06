const MAX_ERROR_MESSAGE_CHARS = 2_048;
const MAX_ERROR_CHAIN_DEPTH = 8;
const claimedErrorObjects = new WeakSet<object>();

export interface SanitizedTelemetryError {
  cause?: {
    code?: string;
    message?: string;
    type: string;
  };
  code?: string;
  message?: string;
  type: string;
}

export function sanitizeTelemetryError(error: unknown): SanitizedTelemetryError {
  const record = objectRecord(error);
  const type =
    sanitizeErrorIdentifier(
      stringValue(record.type) ??
        stringValue(record.name) ??
        (error instanceof Error ? error.name : undefined),
    ) ?? "UnknownError";
  const code = sanitizeErrorCode(stringValue(record.code));
  const message = sanitizeErrorMessage(
    error instanceof Error ? error.message : stringValue(record.message),
  );
  const chain = errorObjectChain(error);
// Reason for fix: The chain contains the input itself; the same error cannot be registered as cause repeatedly without independent nested objects.
  const cause = chain.length > 1 ? sanitizeCause(chain[chain.length - 1]!) : undefined;
  return {
    ...(cause ? { cause } : {}),
    ...(code ? { code } : {}),
    ...(message ? { message } : {}),
    type,
  };
}

/**
* The same source exception will bubble up along Attempt -> Call -> Step -> Turn. The error text should only be logged closest to
* Source, the Span that first claimed it; the parent layer continues to record outcome/failure_stage, but does not copy the same text.
 *
* The cause chain of packaging errors also participates in claiming: AdapterError(cause=ProviderError) will not be re-covered in the upper layer
* Provider Attempt The original error logged. WeakSet does not extend the exception object life cycle.
 */
export function claimSanitizedTelemetryError(error: unknown): SanitizedTelemetryError | undefined {
  const objects = errorObjectChain(error);
  if (objects.length === 0) return sanitizeTelemetryError(error);
  const alreadyClaimed = objects.some((candidate) => claimedErrorObjects.has(candidate));
  for (const candidate of objects) claimedErrorObjects.add(candidate);
  return alreadyClaimed ? undefined : sanitizeTelemetryError(error);
}

export function sanitizeErrorMessage(value: string | undefined): string | undefined {
  if (!value) return undefined;
// The error may carry the entire response body; perform bounded truncation first and then regular cleaning to prevent Telemetry from being malicious or abnormal.
// Provider messages bear unbounded CPU/memory cost.
  const sanitized = value
    .slice(0, 4_096)
    .replace(/\bhttps?:\/\/[^\s"'<>]+/giu, sanitizeUrl)
    .replace(
      /(\bauthorization\b["']?\s*[:=])\s*(?:(?:Bearer|Basic)\s+)?[^\s,"'};]+/giu,
      "$1 {redacted}",
    )
    .replace(
      /([?&](?:api[_-]?key|token|access[_-]?token|authorization|password|passwd|secret|cookie|session|x-arms-license-key)=)[^&\s]+/giu,
      "$1{redacted}",
    )
    .replace(
      /(["']?(?:api[_-]?key|token|access[_-]?token|password|passwd|secret|client[_-]?secret|cookie|set-cookie|session|x-arms-license-key)["']?\s*[:=]\s*["']?)(?!\{redacted\})[^\s,"'};]+/giu,
      "$1{redacted}",
    )
    .replace(/\b(Bearer|Basic)\s+[A-Za-z0-9._~+/=-]+/giu, "$1 {redacted}")
    .replace(/\b(?:sk|rk|pk)-[A-Za-z0-9_-]{12,}\b/giu, "{secret}")
    .replace(/\bgh[pousr]_[A-Za-z0-9]{20,}\b/gu, "{secret}")
    .replace(/\bAKIA[A-Z0-9]{16}\b/gu, "{secret}")
    .replace(/\bAIza[0-9A-Za-z_-]{30,}\b/gu, "{secret}")
    .replace(/\b[A-Za-z0-9]{4,32}@[0-9a-f]{12,}\b/giu, "{secret}")
    .replace(/\b[^/@\s]+@[^/@\s]+\.[^/@\s]+\b/gu, "{email}")
    .replace(
      /\/(?:Users|home|root|workspace|workspaces|Volumes)\/[^/\s]+(?:\/[^\s:;,)\]}]+)*/gu,
      "/{path}",
    )
    .replace(/\/(?:private\/)?(?:var\/folders|tmp)\/[^\s:;,)\]}]+/gu, "/{path}")
    .replace(/\b[A-Za-z]:\\[^\\\s]+(?:\\[^\s:;,)\]}]+)*/gu, "{path}")
    .replace(/\p{Cc}+/gu, " ")
    .replace(/\s+/gu, " ")
    .trim();
  return sanitized.slice(0, MAX_ERROR_MESSAGE_CHARS) || undefined;
}

function objectRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

function errorObjectChain(error: unknown): object[] {
  const result: object[] = [];
  const seen = new Set<object>();
  let current = error;
  for (
    let depth = 0;
    depth < MAX_ERROR_CHAIN_DEPTH && current && typeof current === "object";
    depth += 1
  ) {
    if (seen.has(current)) break;
    seen.add(current);
    result.push(current);
    const record = current as Record<string, unknown>;
// Some stream wrappers use adapterError/error; reuse the same claim mechanism along a single priority chain to avoid repeated recording of the parent Span.
    current = [record.cause, record.adapterError, record.error].find(
      (nested) => nested && typeof nested === "object",
    );
  }
  return result;
}

function sanitizeCause(record: object): SanitizedTelemetryError["cause"] | undefined {
  const value = record as Record<string, unknown>;
  const type = sanitizeErrorIdentifier(stringValue(value.type) ?? stringValue(value.name));
  const code = sanitizeErrorCode(stringValue(value.code));
  const message = sanitizeErrorMessage(stringValue(value.message));
  if (!type && !code && !message) return undefined;
  return {
    ...(code ? { code } : {}),
    ...(message ? { message } : {}),
    type: type ?? "UnknownError",
  };
}

function stringValue(value: unknown): string | undefined {
  return typeof value === "string" && value.trim() ? value.trim() : undefined;
}

function sanitizeErrorIdentifier(value: string | undefined): string | undefined {
  if (!value) return undefined;
  const sanitized = value.trim().slice(0, 128);
  return /^[A-Za-z0-9_.:-]+$/u.test(sanitized) ? sanitized : undefined;
}

function sanitizeErrorCode(value: string | undefined): string | undefined {
  if (!value) return undefined;
  const sanitized = value
    .replace(/\p{Cc}+/gu, " ")
    .replace(/\s+/gu, " ")
    .trim()
    .slice(0, 128);
  return sanitized || undefined;
}

function sanitizeUrl(value: string): string {
  try {
    const parsed = new URL(value);
    const route = parsed.pathname
      .split("/")
      .map((segment) => sanitizeRouteSegment(segment))
      .join("/");
    return `${parsed.protocol}//${parsed.host}${route}`;
  } catch {
    return "{url}";
  }
}

function sanitizeRouteSegment(segment: string): string {
  if (!segment) return segment;
  if (
    /@/u.test(segment) ||
    /^\d{7,}$/u.test(segment) ||
    /^[0-9a-f]{16,}$/iu.test(segment) ||
    /^[0-9a-f]{8}-[0-9a-f-]{27,}$/iu.test(segment)
  ) {
    return "{segment}";
  }
  return segment.slice(0, 128);
}
