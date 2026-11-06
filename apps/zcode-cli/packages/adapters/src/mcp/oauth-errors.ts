import {
  InsufficientScopeError,
  SdkErrorCode,
  UnauthorizedError,
} from "@modelcontextprotocol/client";

/**
* Stable identification of interactive authorization requirements.
 *
* Use `Symbol.for` to register to the global symbol registry: dual installation of bundler or version skew results in two copies of the same process
* When adapter code is used, both copies resolve to the same symbol, which `instanceof` cannot do. SDK in transport
* Use the same technique on auth seam (`Symbol.for("mcp.authSeamEscape")`), and `markAuthSeamEscape()`
* is identity-preserving, so our brand can bubble up through the SDK to the orchestration layer unchanged.
 */
const INTERACTIVE_REQUIRED_BRAND = Symbol.for("zcode.mcp.oauth.interactiveAuthorizationRequired");
const TEMPORARY_REFRESH_FAILURE_BRAND = Symbol.for("zcode.mcp.oauth.temporaryRefreshFailure");

const MCP_OAUTH_INTERACTIVE_REQUIRED_ERROR_CODE = "MCP_OAUTH_INTERACTIVE_REQUIRED";
const MCP_OAUTH_TEMPORARY_REFRESH_FAILURE_ERROR_CODE = "MCP_OAUTH_TEMPORARY_REFRESH_FAILURE";

export type McpOAuthInteractiveRequiredReason =
  | "no_credentials"
  | "no_refresh_token"
  | "invalid_grant"
  | "invalid_client"
  | "insufficient_scope"
  | "unauthorized"
  | "legacy_provider_seam";

interface McpOAuthInteractiveRequiredError extends Error {
  code: typeof MCP_OAUTH_INTERACTIVE_REQUIRED_ERROR_CODE;
  reason: McpOAuthInteractiveRequiredReason;
  requiredScope?: string;
  resourceMetadataUrl?: string;
}

interface McpOAuthTemporaryRefreshFailureError extends Error {
  code: typeof MCP_OAUTH_TEMPORARY_REFRESH_FAILURE_ERROR_CODE;
}

export function createInteractiveAuthorizationRequiredError(input: {
  cause?: unknown;
  reason: McpOAuthInteractiveRequiredReason;
  requiredScope?: string;
  resourceMetadataUrl?: string;
  serverName: string;
}): McpOAuthInteractiveRequiredError {
  const error = new Error(
    `MCP server ${input.serverName} requires interactive OAuth authorization (${input.reason})`,
    input.cause === undefined ? undefined : { cause: input.cause },
  ) as McpOAuthInteractiveRequiredError;
  Object.defineProperty(error, INTERACTIVE_REQUIRED_BRAND, {
    configurable: true,
    value: true,
  });
  error.code = MCP_OAUTH_INTERACTIVE_REQUIRED_ERROR_CODE;
  error.reason = input.reason;
  if (input.requiredScope) error.requiredScope = input.requiredScope;
  if (input.resourceMetadataUrl) error.resourceMetadataUrl = input.resourceMetadataUrl;
  return error;
}

/**
* Network/5xx failure for reactive refresh.
 *
* Must be distinguished from `interactiveRequired`: temporary AS failure does not mean that the grant has expired, and mistakenly converting it to interactive authorization will
* Needless to interrupt the user; returning an old token that has been rejected by the resource server will inevitably generate a second 401.
 */
export function createTemporaryRefreshFailureError(input: {
  cause?: unknown;
  serverName: string;
}): McpOAuthTemporaryRefreshFailureError {
  const error = new Error(
    `MCP server ${input.serverName} OAuth token refresh failed temporarily`,
    input.cause === undefined ? undefined : { cause: input.cause },
  ) as McpOAuthTemporaryRefreshFailureError;
  Object.defineProperty(error, TEMPORARY_REFRESH_FAILURE_BRAND, {
    configurable: true,
    value: true,
  });
  error.code = MCP_OAUTH_TEMPORARY_REFRESH_FAILURE_ERROR_CODE;
  return error;
}

function isInteractiveAuthorizationRequiredError(
  error: unknown,
): error is McpOAuthInteractiveRequiredError {
  return hasBrand(error, INTERACTIVE_REQUIRED_BRAND);
}

function isTemporaryRefreshFailureError(
  error: unknown,
): error is McpOAuthTemporaryRefreshFailureError {
  return hasBrand(error, TEMPORARY_REFRESH_FAILURE_BRAND);
}

export interface InteractiveAuthorizationTrigger {
  reason: McpOAuthInteractiveRequiredReason;
  requiredScope?: string;
  resourceMetadataUrl?: string;
}

/**
* Classify connection/runtime errors as "requires interactive authorization".
 *
* The SDK under pure AuthProvider may only give these types of deterministic authentication errors:
* - `interactiveRequired` thrown by ourselves (missing token, invalid_grant, invalid_client);
* - `SdkHttpError(ClientHttpAuthentication)` still 401 after retrying;
* - `UnauthorizedError` without `onUnauthorized` (defense path);
* - `InsufficientScopeError` for Streamable HTTP 403 (with requiredScope).
 *
* `temporaryRefreshFailure` is explicitly not listed here: it must preserve credentials and fail unchanged.
 */
export function classifyInteractiveAuthorizationTrigger(
  error: unknown,
): InteractiveAuthorizationTrigger | undefined {
  if (isTemporaryRefreshFailureError(error)) return undefined;
  if (isInteractiveAuthorizationRequiredError(error)) {
    return {
      reason: error.reason,
      ...(error.requiredScope ? { requiredScope: error.requiredScope } : {}),
      ...(error.resourceMetadataUrl ? { resourceMetadataUrl: error.resourceMetadataUrl } : {}),
    };
  }
  if (error instanceof InsufficientScopeError) {
    return {
      reason: "insufficient_scope",
      ...(error.requiredScope ? { requiredScope: error.requiredScope } : {}),
      ...(error.resourceMetadataUrl
        ? { resourceMetadataUrl: String(error.resourceMetadataUrl) }
        : {}),
    };
  }
  if (error instanceof UnauthorizedError) return { reason: "unauthorized" };
  if (isSdkAuthenticationHttpError(error)) return { reason: "unauthorized" };
// cause chain: SDK wraps errors on several seams, classification cannot only look at the outermost layer.
  const cause = (error as { cause?: unknown } | undefined)?.cause;
  if (cause !== undefined && cause !== error) {
    return classifyInteractiveAuthorizationTrigger(cause);
  }
  return undefined;
}

/** Still 401 after retrying: `SdkHttpError(SdkErrorCode.ClientHttpAuthentication)`. Compare by code and do not rely on instanceof. */
function isSdkAuthenticationHttpError(error: unknown): boolean {
  if (typeof error !== "object" || error === null) return false;
  const code = (error as { code?: unknown }).code;
  return code === SdkErrorCode.ClientHttpAuthentication;
}

function hasBrand(error: unknown, brand: symbol): boolean {
  return (typeof error === "object" && error !== null) || typeof error === "function"
    ? (error as Record<symbol, unknown>)[brand] === true
    : false;
}
