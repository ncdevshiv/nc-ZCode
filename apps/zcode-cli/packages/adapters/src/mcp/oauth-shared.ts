import type { OAuthDiscoveryState } from "@modelcontextprotocol/client";
import type { SharedZCodeCredentialStore } from "../auth/shared-credentials.js";
import { isRecord, mcpOAuthCredentialKey } from "./oauth-credentials.js";

const MCP_OAUTH_DISCOVERY_STATE_KEY = "discovery_state";
const MCP_OAUTH_DISCOVERY_FETCHED_AT_KEY = "discovery_state_fetched_at";

/** discovery metadata cache lifetime. Rediscover after expiration to avoid long-term use of old AS metadata with changed endpoints. */
const MCP_OAUTH_DISCOVERY_TTL_MS = 24 * 60 * 60 * 1000;

export interface McpOAuthAuthorizationContext {
  authorizationUrl: string;
  redirectUrl: string;
  serverName: string;
}

/**
* The timestamp of the discovery record is stored in a separate key and does not wrap `discovery_state` itself.
 *
* The value form of `discovery_state` must continue to be bare `OAuthDiscoveryState`: CLI and desktop can be upgraded independently,
* Share the same credentials file, changing it to `{fetched_at, state}` wrapper structure will allow unupgraded readers to read a
* Objects without `authorizationServerUrl` silently lose discovery cache. Appending an optional key is
* Backward compatibility practices.
 */
export async function saveDiscoveryRecord(
  credentialStore: SharedZCodeCredentialStore,
  keyPrefix: string,
  state: OAuthDiscoveryState,
  now = Date.now(),
): Promise<void> {
  await credentialStore.saveMany({
    [mcpOAuthCredentialKey(keyPrefix, MCP_OAUTH_DISCOVERY_STATE_KEY)]: JSON.stringify(state),
    [mcpOAuthCredentialKey(keyPrefix, MCP_OAUTH_DISCOVERY_FETCHED_AT_KEY)]: String(now),
  });
}

/**
* Read unexpired discovery records.
 *
* If the timestamp is missing (written in the old version) or expired, `undefined` will be returned, allowing the caller to rediscover it; it will be filled in the next time it is saved.
* Timestamp, self-healing.
 */
export async function loadDiscoveryRecord(
  credentialStore: SharedZCodeCredentialStore,
  keyPrefix: string,
  options: { expectedIssuer?: string; now?: number; ttlMs?: number } = {},
): Promise<OAuthDiscoveryState | undefined> {
  const now = options.now ?? Date.now();
  const ttlMs = options.ttlMs ?? MCP_OAUTH_DISCOVERY_TTL_MS;
  const stateKey = mcpOAuthCredentialKey(keyPrefix, MCP_OAUTH_DISCOVERY_STATE_KEY);
  const fetchedAtKey = mcpOAuthCredentialKey(keyPrefix, MCP_OAUTH_DISCOVERY_FETCHED_AT_KEY);
  const values = await credentialStore.loadMany([stateKey, fetchedAtKey]);
  const raw = values[stateKey];
  if (!raw) return undefined;

  const fetchedAt = Number(values[fetchedAtKey]);
  if (!Number.isFinite(fetchedAt) || now - fetchedAt >= ttlMs) return undefined;

  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return undefined;
  }
  if (!isRecord(parsed) || typeof parsed.authorizationServerUrl !== "string") return undefined;

  const state = parsed as unknown as OAuthDiscoveryState;
  if (options.expectedIssuer && !issuersMatch(state, options.expectedIssuer)) {
// The issuer recorded by canonical is inconsistent with the cache: the authorization server has changed, and the cache must be invalidated.
    return undefined;
  }
  return state;
}

function issuersMatch(state: OAuthDiscoveryState, expectedIssuer: string): boolean {
  const cachedIssuer = state.authorizationServerMetadata?.issuer ?? state.authorizationServerUrl;
  if (!cachedIssuer) return false;
  return normalizeIssuer(cachedIssuer) === normalizeIssuer(expectedIssuer);
}

function normalizeIssuer(value: string): string {
  return value.endsWith("/") ? value.slice(0, -1) : value;
}
