import { dirname, join } from "node:path";
import {
  discoverOAuthServerInfo,
  OAuthError,
  OAuthErrorCode,
  refreshAuthorization,
  selectResourceURL,
  type AuthorizationServerMetadata,
  type FetchLike,
  type OAuthDiscoveryState,
} from "@modelcontextprotocol/client";
import type { Logger } from "@zcode/contracts";
import { isZCodeFileLockTimeoutError } from "@zcode/shared";
import { withFileLock } from "@zcode/shared/node";
import type { SharedZCodeCredentialStore } from "../auth/shared-credentials.js";
import {
  invalidateCanonicalCredentials,
  loadCredentialPair,
  publishCanonicalCredentials,
  type CredentialPairSnapshot,
} from "./oauth-credentials.js";
import {
  createInteractiveAuthorizationRequiredError,
  createTemporaryRefreshFailureError,
} from "./oauth-errors.js";
import { sanitizeKeyPrefix } from "./oauth-lease.js";
import { loadDiscoveryRecord, saveDiscoveryRecord } from "./oauth-shared.js";

/**
* Refresh lock acquisition budget.
 *
* The network budget within the lock must be covered (one discovery + one token request), otherwise the waiter will wait before the winner
* Timed out when publishing results. The default value of `withFileLock` is only 8 seconds (privateFilePersistence.ts:9), which is not enough.
 */
const REFRESH_LOCK_MAX_WAIT_MS = 45_000;

interface RefreshMcpOAuthTokensInput {
  credentialStore: SharedZCodeCredentialStore;
  fetchFn?: FetchLike;
  keyPrefix: string;
  logger?: Logger;
  /**
* reactive = triggered by 401 `onUnauthorized`.
   *
* Determine the semantics when the network fails: proactive (proactive refresh at the expiration date) can fail-soft return the current value, because the token has not yet been
* Rejected by the resource server; reactive cannot - it has just been rejected, returning it will inevitably generate a second 401 and throw
* `ClientHttpAuthentication` will also misjudge temporary AS failures as requiring interactive authorization.
   */
  reactive: boolean;
  serverName: string;
  serverUrl: string;
/** Static configured clientId. When present, `invalid_client` cannot heal itself by interactive authorization. */
  staticClientId?: string;
}

interface ResolvedAsMetadata {
  authorizationServerUrl: string;
  metadata?: AuthorizationServerMetadata;
  resource?: URL;
}

/**
* Refresh the access token in the cross-process solo lock and return the available access token.
 *
* Concurrent refresh problem: multiple CLI processes share the same credential file, `withFileLock` only covers credential reading and writing, not the network
* token exchange. When the access token expires, each process concurrently refreshes with the same refresh token, which affects the authorization server.
* rotation reuse-detection, the entire token family is revoked and eventually degrades to reauthorization.
 */
export async function refreshMcpOAuthTokensUnderLock(
  input: RefreshMcpOAuthTokensInput,
): Promise<string> {
  const observedGeneration = (await loadCredentialPair(input.credentialStore, input.keyPrefix))
    ?.generation;
  const lockPath = resolveRefreshLockPath(input.credentialStore.filePath, input.keyPrefix);

  try {
    return await withFileLock(
      lockPath,
      async () => await refreshLocked(input, observedGeneration),
      {
        lockMaxWaitMs: REFRESH_LOCK_MAX_WAIT_MS,
      },
    );
  } catch (error) {
    if (!isZCodeFileLockTimeoutError(error)) throw error;
// Waiting for the lock timeout does not mean that the refresh failed: the winner may have already published the results. Reread it first, confirm the replacement and use it directly if you have the token.
    const current = await loadCredentialPair(input.credentialStore, input.keyPrefix);
    if (current?.tokens && current.generation !== observedGeneration) {
      return current.tokens.access_token;
    }
    throw createTemporaryRefreshFailureError({ cause: error, serverName: input.serverName });
  }
}

async function refreshLocked(
  input: RefreshMcpOAuthTokensInput,
  observedGeneration: string | undefined,
): Promise<string> {
  const current = await loadCredentialPair(input.credentialStore, input.keyPrefix);

// Merge: Reuse directly after others have refreshed during the lock period, zero second request. This is the key to rotation reuse-detection.
  if (current?.tokens && current.generation !== observedGeneration) {
    return current.tokens.access_token;
  }
  if (!current?.tokens) {
// The generation changed but the winner did not leave a token (for example, it received an invalid_grant): it cannot be regarded as "no credentials"
// Return silently and must switch to interactive authorization.
    throw createInteractiveAuthorizationRequiredError({
      reason: "no_credentials",
      serverName: input.serverName,
    });
  }
  const refreshToken = current.tokens.refresh_token;
  const clientInformation = current.clientInformation;
  if (!refreshToken || !clientInformation) {
    throw createInteractiveAuthorizationRequiredError({
      reason: "no_refresh_token",
      serverName: input.serverName,
    });
  }

  let resolved: ResolvedAsMetadata;
  try {
    resolved = await resolveAsMetadata(input, current);
  } catch (error) {
// Discovery failure must not be treated as grant failure.
    return failSoft(input, current, error);
  }

  try {
    const next = await refreshAuthorization(resolved.authorizationServerUrl, {
      clientInformation,
      refreshToken,
      ...(resolved.metadata ? { metadata: resolved.metadata } : {}),
      ...(resolved.resource ? { resource: resolved.resource } : {}),
      ...(input.fetchFn ? { fetchFn: input.fetchFn } : {}),
    });
    const published = await publishCanonicalCredentials(input.credentialStore, input.keyPrefix, {
      clientInformation,
      ...(current.issuer ? { issuer: current.issuer } : {}),
      publishedBy: `refresh:${input.keyPrefix}`,
      tokens: next,
    });
    input.logger?.info("MCP OAuth access token refreshed", {
      event: "mcp.oauth.refresh.completed",
      credentialKeyPrefix: input.keyPrefix,
      mcpServerName: input.serverName,
      processId: process.pid,
      publishedGeneration: published.generation.slice(0, 12),
      reactive: input.reactive,
      refreshTokenRotated: next.refresh_token !== refreshToken,
      status: "completed",
    });
    return next.access_token;
  } catch (error) {
    return await handleRefreshFailure(input, current, error);
  }
}

async function handleRefreshFailure(
  input: RefreshMcpOAuthTokensInput,
  current: CredentialPairSnapshot,
  error: unknown,
): Promise<never | string> {
  const oauthErrorCode = error instanceof OAuthError ? error.code : undefined;
  input.logger?.warn("MCP OAuth access token refresh failed", {
    event: "mcp.oauth.refresh.failed",
    credentialKeyPrefix: input.keyPrefix,
    credentialSource: current.source,
    mcpServerName: input.serverName,
    oauthErrorCode,
    processId: process.pid,
    reactive: input.reactive,
    status: "failed",
  });

  if (oauthErrorCode === OAuthErrorCode.InvalidGrant) {
// Deterministic invalidation: refresh token has been revoked or expired. CAS clears tokens and retains the client as the re-authorization seed.
    if (current.raw) {
      await invalidateCanonicalCredentials(
        input.credentialStore,
        input.keyPrefix,
        current.raw,
        "tokens",
      );
    }
    throw createInteractiveAuthorizationRequiredError({
      cause: error,
      reason: "invalid_grant",
      serverName: input.serverName,
    });
  }

  if (
    oauthErrorCode === OAuthErrorCode.InvalidClient ||
    oauthErrorCode === OAuthErrorCode.UnauthorizedClient
  ) {
    if (input.staticClientId) {
// The invalid_client of the statically configured client cannot heal itself by interactive authorization: Phase 2 will still use the same client.
// Sub-authorization will only create a cycle. Report configuration errors.
      throw new Error(
        `MCP server ${input.serverName} OAuth client was rejected by the authorization server (invalid_client). ` +
          `The configured clientId is not usable; fix the MCP oauth configuration.`,
        { cause: error },
      );
    }
    if (current.raw) {
      await invalidateCanonicalCredentials(
        input.credentialStore,
        input.keyPrefix,
        current.raw,
        "all",
      );
    }
    throw createInteractiveAuthorizationRequiredError({
      cause: error,
      reason: "invalid_client",
      serverName: input.serverName,
    });
  }

  return failSoft(input, current, error);
}

/**
* Non-deterministic failure (network, 5xx, `server_error`).
 *
* Proactive returns the current value: the token has not been rejected yet, let the request continue, and the 401 path retains the final decision.
* reactive must throw a temporary error: the present value has just been rejected, return it must be 401 again.
 */
function failSoft(
  input: RefreshMcpOAuthTokensInput,
  current: CredentialPairSnapshot,
  error: unknown,
): string {
  if (input.reactive || !current.tokens) {
    throw createTemporaryRefreshFailureError({ cause: error, serverName: input.serverName });
  }
  return current.tokens.access_token;
}

/**
* Parse authorization server metadata and verified resources.
 *
* discovery records have TTL and are shared with Phase 2; `resource` must be retrieved and brought into refresh requests (RFC 8707),
* The SDK's own refresh will pass it, and we must make it up ourselves after bypassing `auth()`, otherwise the audience binding will be lost.
 */
async function resolveAsMetadata(
  input: Pick<
    RefreshMcpOAuthTokensInput,
    "credentialStore" | "fetchFn" | "keyPrefix" | "serverUrl"
  >,
  current: Pick<CredentialPairSnapshot, "issuer">,
): Promise<ResolvedAsMetadata> {
  const cached = await loadDiscoveryRecord(input.credentialStore, input.keyPrefix, {
    ...(current.issuer ? { expectedIssuer: current.issuer } : {}),
  });
  const discovered: OAuthDiscoveryState =
    cached ??
    (await discoverOAuthServerInfo(input.serverUrl, {
      ...(input.fetchFn ? { fetchFn: input.fetchFn } : {}),
    }));
  if (!cached) {
    await saveDiscoveryRecord(input.credentialStore, input.keyPrefix, discovered);
  }

  const resource = await selectResourceURL(
    input.serverUrl,
// selectResourceURL reads only provider's validateResourceURL; there is no provider here, just pass an empty shell.
    {} as never,
    discovered.resourceMetadata,
  );
  return {
    authorizationServerUrl: discovered.authorizationServerUrl,
    ...(discovered.authorizationServerMetadata
      ? { metadata: discovered.authorizationServerMetadata }
      : {}),
    ...(resource ? { resource } : {}),
  };
}

/**
* refresh lock file path.
 *
* Must be independent of the credentials file: `publishCanonicalCredentials` will be called within the lock, which itself
* Credentials.json is locked; the same path will enter a deadlock by itself. basename contains only hash and hyphen (Windows filename
* no colons allowed).
 */
function resolveRefreshLockPath(credentialsFilePath: string, keyPrefix: string): string {
  return join(dirname(credentialsFilePath), `${sanitizeKeyPrefix(keyPrefix)}.refresh`);
}
