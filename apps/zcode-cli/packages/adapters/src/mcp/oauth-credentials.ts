import { createHash, randomBytes } from "node:crypto";
import { isDeepStrictEqual } from "node:util";
import type { OAuthClientInformationMixed, OAuthTokens } from "@modelcontextprotocol/client";
import type { SharedZCodeCredentialStore } from "../auth/shared-credentials.js";

export const MCP_OAUTH_CANONICAL_CREDENTIALS_KEY = "authorization_credentials";
const MCP_OAUTH_LEGACY_CLIENT_KEY = "client_information";
const MCP_OAUTH_LEGACY_TOKENS_KEY = "tokens";
export const MCP_OAUTH_CREDENTIALS_VERSION = 2;
export const MCP_OAUTH_SUPPORTED_CREDENTIAL_VERSIONS = new Set([1, MCP_OAUTH_CREDENTIALS_VERSION]);

/**
 * canonical credential pair。
 *
* `generation`, `obtained_at`, `expires_at`, and `issuer` are all **optional new fields** and do not bump version:
* Adding optional fields is backward compatible with old readers (old readers can ignore unknown fields), but bump version will make unupgraded readers
* CLI/desktop ignores new records as unknown versions, falls back to legacy images, and discards pair guarantees.
 */
export interface McpOAuthCanonicalCredentials {
  client_information: OAuthClientInformationMixed;
/** The absolute expiration point (epoch ms) derived from `expires_in` and `obtained_at`. */
  expires_at?: number;
  /**
* A unique random id for each publication.
   *
* Cannot reuse `published_by`: it is the transaction id derived from OAuth state, multiple refreshes of the same transaction will not change it,
* Unable to assume the follower's responsibility of observing replacement and failed CAS. Random id while eliminating ABA.
   */
  generation?: string;
/** Authorization server issuer. This time only the fields are retained and credential key keying is not involved. */
  issuer?: string;
/** token acquisition time (epoch ms). `OAuthTokens` only has `expires_in`, without which the real expiration point cannot be calculated across processes. */
  obtained_at?: number;
  published_by: string;
  tokens: OAuthTokens;
  version: 1 | typeof MCP_OAUTH_CREDENTIALS_VERSION;
}

export interface CanonicalCredentialSnapshot {
  clientInformation: OAuthClientInformationMixed;
  expiresAt?: number;
  generation: string;
  issuer?: string;
  obtainedAt?: number;
/** Raw JSON for use by compare-and-delete. */
  raw: string;
  tokens: OAuthTokens;
}

export function mcpOAuthCredentialKey(keyPrefix: string, name: string): string {
  return `${keyPrefix}:${name}`;
}

function createCredentialGeneration(): string {
  return randomBytes(16).toString("hex");
}

/**
* Migration period baseline: Old records do not have `generation`, and are replaced by stable hashes of canonical original content.
* Content changes are generation changes, which are enough to support the follower's judgment of "whether to change the generation".
 */
function resolveCredentialGeneration(canonical: McpOAuthCanonicalCredentials, raw: string): string {
  if (typeof canonical.generation === "string" && canonical.generation.length > 0) {
    return canonical.generation;
  }
  return `legacy-${createHash("sha256").update(raw).digest("hex").slice(0, 32)}`;
}

export function isCanonicalCredentials(value: unknown): value is McpOAuthCanonicalCredentials {
  if (
    !isRecord(value) ||
    typeof value.version !== "number" ||
    !MCP_OAUTH_SUPPORTED_CREDENTIAL_VERSIONS.has(value.version)
  ) {
    return false;
  }
  if (typeof value.published_by !== "string" || value.published_by.length === 0) return false;
  if (!isRecord(value.client_information) || !isRecord(value.tokens)) return false;
  return (
    typeof value.client_information.client_id === "string" &&
    typeof value.tokens.access_token === "string" &&
    typeof value.tokens.token_type === "string"
  );
}

/**
* Only read canonical pairs, no legacy compatible derivation is performed.
 *
* The baseline generation of Phase 2 and the refresh of Phase 1 only require canonical; legacy mirroring
* Cross-compatible logic remains within the provider.
 */
export async function loadCanonicalCredentials(
  credentialStore: SharedZCodeCredentialStore,
  keyPrefix: string,
): Promise<CanonicalCredentialSnapshot | undefined> {
  const key = mcpOAuthCredentialKey(keyPrefix, MCP_OAUTH_CANONICAL_CREDENTIALS_KEY);
  const raw = await credentialStore.load(key);
  if (!raw) return undefined;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return undefined;
  }
  if (!isCanonicalCredentials(parsed)) return undefined;
  return {
    clientInformation: parsed.client_information,
    ...(parsed.expires_at === undefined ? {} : { expiresAt: parsed.expires_at }),
    generation: resolveCredentialGeneration(parsed, raw),
    ...(parsed.issuer === undefined ? {} : { issuer: parsed.issuer }),
    ...(parsed.obtained_at === undefined ? {} : { obtainedAt: parsed.obtained_at }),
    raw,
    tokens: parsed.tokens,
  };
}

interface PublishCanonicalCredentialsInput {
  clientInformation: OAuthClientInformationMixed;
  issuer?: string;
  obtainedAt?: number;
  publishedBy: string;
  tokens: OAuthTokens;
}

interface PublishedCanonicalCredentials {
  canonical: McpOAuthCanonicalCredentials;
  generation: string;
  legacyClientRaw: string;
  legacyTokensRaw: string;
  raw: string;
}

/**
* Atomic release of canonical pair and legacy images.
 *
* The client and refresh token must come from the same authorization, so the three keys must be in the same shared credential store
* Write once in the cross-process read-modify-write critical section; separate coverage will make the last written client and token come from
* Different transactions. The legacy image will continue to be maintained within the compatibility window so that the CLI/Desktop that has not been upgraded can still read it.
 */
export async function publishCanonicalCredentials(
  credentialStore: SharedZCodeCredentialStore,
  keyPrefix: string,
  input: PublishCanonicalCredentialsInput,
): Promise<PublishedCanonicalCredentials> {
  const obtainedAt = input.obtainedAt ?? Date.now();
  const expiresAt =
    typeof input.tokens.expires_in === "number" && Number.isFinite(input.tokens.expires_in)
      ? obtainedAt + input.tokens.expires_in * 1000
      : undefined;
  const generation = createCredentialGeneration();
  const canonical: McpOAuthCanonicalCredentials = {
    client_information: input.clientInformation,
    ...(expiresAt === undefined ? {} : { expires_at: expiresAt }),
    generation,
    ...(input.issuer === undefined ? {} : { issuer: input.issuer }),
    obtained_at: obtainedAt,
    published_by: input.publishedBy,
    tokens: input.tokens,
    version: MCP_OAUTH_CREDENTIALS_VERSION,
  };
  const raw = JSON.stringify(canonical);
  const legacyClientRaw = JSON.stringify(input.clientInformation);
  const legacyTokensRaw = JSON.stringify(input.tokens);
  await credentialStore.saveMany({
    [mcpOAuthCredentialKey(keyPrefix, MCP_OAUTH_CANONICAL_CREDENTIALS_KEY)]: raw,
    [mcpOAuthCredentialKey(keyPrefix, MCP_OAUTH_LEGACY_CLIENT_KEY)]: legacyClientRaw,
    [mcpOAuthCredentialKey(keyPrefix, MCP_OAUTH_LEGACY_TOKENS_KEY)]: legacyTokensRaw,
  });
  return { canonical, generation, legacyClientRaw, legacyTokensRaw, raw };
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** The security margin for the access token’s expiration date. */
const MCP_OAUTH_EXPIRY_SKEW_MS = 30_000;

/**
* Whether it is necessary to refresh the token before giving it to the request header.
 *
* No `expires_at` (old record, or the server did not return `expires_in`) will be regarded as expired: `OAuthTokens` without acquisition
* Time, it is impossible to calculate the true expiration point. It is better to enter the lock and try to refresh it than to send out a token that may have expired.
 */
export function isCanonicalTokenNearExpiry(
  snapshot: Pick<CanonicalCredentialSnapshot, "expiresAt">,
  now = Date.now(),
  skewMs = MCP_OAUTH_EXPIRY_SKEW_MS,
): boolean {
  if (snapshot.expiresAt === undefined) return true;
  return now >= snapshot.expiresAt - skewMs;
}

type CanonicalInvalidationScope = "tokens" | "all";

/**
* Conditions based on canonical snapshots are invalid.
 *
* Only deleted if the canonical current value is still equal to `expectedRaw`, so another transaction has published a new pair this time
* If it fails, it will be discarded as a whole and the winner will not be deleted by mistake. `tokens` is reserved for legacy clients as seeds for reauthorization; `all` is used for
* `invalid_client`, client and token must be discarded as a pair.
 */
export async function invalidateCanonicalCredentials(
  credentialStore: SharedZCodeCredentialStore,
  keyPrefix: string,
  expectedRaw: string,
  scope: CanonicalInvalidationScope,
): Promise<boolean> {
  const canonicalKey = mcpOAuthCredentialKey(keyPrefix, MCP_OAUTH_CANONICAL_CREDENTIALS_KEY);
  const keysToDelete = [
    canonicalKey,
    mcpOAuthCredentialKey(keyPrefix, MCP_OAUTH_LEGACY_TOKENS_KEY),
  ];
  if (scope === "all") {
    keysToDelete.push(mcpOAuthCredentialKey(keyPrefix, MCP_OAUTH_LEGACY_CLIENT_KEY));
  }
  return await credentialStore.deleteManyIfValue(canonicalKey, expectedRaw, keysToDelete);
}

export interface CredentialPairSnapshot {
  clientInformation?: OAuthClientInformationMixed;
  expiresAt?: number;
/** When canonical is available, it is its generation; when it is only legacy, it is derived based on content and can still be used to observe generation changes. */
  generation?: string;
  issuer?: string;
  obtainedAt?: number;
/** canonical original JSON; undefined only when legacy is used (canonical CAS cannot be done). */
  raw?: string;
  source: "canonical" | "legacy";
  tokens?: OAuthTokens;
}

/**
* Read canonical and legacy images and derive a usable pair according to compatibility rules.
 */
export async function loadCredentialPair(
  credentialStore: SharedZCodeCredentialStore,
  keyPrefix: string,
): Promise<CredentialPairSnapshot | undefined> {
  const canonicalKey = mcpOAuthCredentialKey(keyPrefix, MCP_OAUTH_CANONICAL_CREDENTIALS_KEY);
  const legacyClientKey = mcpOAuthCredentialKey(keyPrefix, MCP_OAUTH_LEGACY_CLIENT_KEY);
  const legacyTokensKey = mcpOAuthCredentialKey(keyPrefix, MCP_OAUTH_LEGACY_TOKENS_KEY);
  const values = await credentialStore.loadMany([canonicalKey, legacyClientKey, legacyTokensKey]);
  return deriveCredentialPair({
    canonicalRaw: values[canonicalKey] ?? undefined,
    legacyClientRaw: values[legacyClientKey] ?? undefined,
    legacyTokensRaw: values[legacyTokensKey] ?? undefined,
  });
}

/**
* Compatible with pair derivation rules within the window (pure function, no I/O).
 *
* The purpose of extracting pure functions is to allow "callers who already hold the original snapshot" (who need the original value to do compare-and-delete
* provider) reuses the same rule instead of reading the credential file again - writing it in both places will inevitably diverge.
 *
* Rule background: CLI and desktop in the compatibility window may be different versions. The old process only writes the legacy key, and the new process writes
* canonical + mirror. What is judged here is "in the old format without generation, whether the changes in legacy can be proven to belong to
*Same authorization".
 */
export function deriveCredentialPair(input: {
  canonicalRaw?: string;
  legacyClientRaw?: string;
  legacyTokensRaw?: string;
}): CredentialPairSnapshot | undefined {
  const canonicalParsed = parseJson<unknown>(input.canonicalRaw);
  const canonical =
    canonicalParsed !== undefined && isCanonicalCredentials(canonicalParsed)
      ? canonicalParsed
      : undefined;
  const legacyClient = parseJson<OAuthClientInformationMixed>(input.legacyClientRaw);
  const legacyTokens = parseJson<OAuthTokens>(input.legacyTokensRaw);

  if (canonical && input.canonicalRaw) {
    const canonicalSnapshot: CredentialPairSnapshot = {
      clientInformation: canonical.client_information,
      ...(canonical.expires_at === undefined ? {} : { expiresAt: canonical.expires_at }),
      generation: resolveCredentialGeneration(canonical, input.canonicalRaw),
      ...(canonical.issuer === undefined ? {} : { issuer: canonical.issuer }),
      ...(canonical.obtained_at === undefined ? {} : { obtainedAt: canonical.obtained_at }),
      raw: input.canonicalRaw,
      source: "canonical",
      tokens: canonical.tokens,
    };
    if (canonical.version === 1) {
      const canAdoptLegacyTokens =
        legacyTokens !== undefined &&
        (!legacyClient || isDeepStrictEqual(legacyClient, canonical.client_information));
      if (canAdoptLegacyTokens) {
        return {
          clientInformation: legacyClient ?? canonical.client_information,
          source: "legacy",
          tokens: legacyTokens,
        };
      }
// After v1 is released, the legacy image will be deleted, so no image is a normal stable state; if it is applied to v2
// Image invalidation rules will discard canonical tokens that are still valid and force all upgrade users to re-authorize.
      return canonicalSnapshot;
    }
    if (!legacyTokens) {
// The old provider must remain invalid after invalidate tokens, and old tokens cannot be revived from canonical.
      return { clientInformation: legacyClient, source: "legacy" };
    }
    if (isDeepStrictEqual(legacyTokens, canonical.tokens)) {
      return legacyClient ? canonicalSnapshot : { source: "legacy", tokens: legacyTokens };
    }
    if (legacyClient && isDeepStrictEqual(legacyClient, canonical.client_information)) {
// When the client has not changed, you can confirm that the legacy token is the refresh result of the old provider with the same identity.
      return { clientInformation: legacyClient, source: "legacy", tokens: legacyTokens };
    }
// When both client and token change and there is no generation, it cannot be proven that they are from the same transaction: keep the client for re-authorization,
// Never speculatively splice authentication assets.
    return { clientInformation: legacyClient, source: "legacy" };
  }

  if (!legacyClient && !legacyTokens) return undefined;
  return {
    clientInformation: legacyClient,
    source: "legacy",
    ...(legacyTokens ? { tokens: legacyTokens } : {}),
  };
}

function parseJson<T>(raw: string | undefined): T | undefined {
  if (!raw) return undefined;
  try {
    return JSON.parse(raw) as T;
  } catch {
    return undefined;
  }
}
