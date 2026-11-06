/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import { createHash, randomBytes } from "node:crypto";
import {
  auth,
  type FetchLike,
  type OAuthClientInformationMixed,
  type OAuthClientMetadata,
  type OAuthClientProvider,
  type OAuthDiscoveryState,
  type OAuthTokens,
} from "@modelcontextprotocol/client";
import type { Logger, McpOAuthConfig } from "@zcode/contracts";
import {
  createLocalhostOAuthCallbackServer,
  type LocalhostOAuthCallbackServer,
} from "../auth/localhost-callback.js";
import type { SharedZCodeCredentialStore } from "../auth/shared-credentials.js";
import {
  loadCanonicalCredentials,
  publishCanonicalCredentials,
  type CanonicalCredentialSnapshot,
} from "./oauth-credentials.js";
import {
  deletePendingAuthorizationIfOwned,
  loadPendingAuthorization,
  publishPendingAuthorization,
  tryAcquireAuthorizationLease,
} from "./oauth-lease.js";
import {
  loadDiscoveryRecord,
  saveDiscoveryRecord,
  type McpOAuthAuthorizationContext,
} from "./oauth-shared.js";
import { withTimeout } from "./timeout.js";

type McpAuthorizationCodeOAuthConfig = Extract<McpOAuthConfig, { type: "authorization_code" }>;

/** Global lifetime of authorized transactions. It has nothing to do with the caller waiting budget (session 15s), and is closed independently by the caller side. */
export const MCP_OAUTH_AUTHORIZATION_TRANSACTION_TTL_MS = 5 * 60 * 1000;
const FOLLOWER_POLL_INTERVAL_MS = 500;

export type McpInteractiveAuthorizationOutcome =
/** This call completes authorization and the credentials have been issued. */
  | { status: "authorized" }
/** Another transaction has completed authorization (generation has been changed), just go back to Phase 1 and reconnect. */
  | { status: "already-authorized" }
/** The transaction is still in progress (this call is a follower or the transaction TTL has been reached), and the authorization URL is available for display. */
  | { status: "pending"; authorizationUrl?: string }
  | { status: "failed"; error: unknown };

interface McpInteractiveAuthorizationInput {
  adapterInstanceId?: string;
  config: McpAuthorizationCodeOAuthConfig;
  credentialStore: SharedZCodeCredentialStore;
  fetchFn?: FetchLike;
/** 403 step-up: When unionScope is a strict superset of requiredScope, refresh cannot expand rights and must be forced to re-authorize. */
  forceReauthorization?: boolean;
  keyPrefix: string;
  logger?: Logger;
  onAuthorizationRequired?: (context: McpOAuthAuthorizationContext) => Promise<void> | void;
  openAuthorizationUrl?: (context: McpOAuthAuthorizationContext) => Promise<void> | void;
/** The final scope calculated by the orchestration layer (config scope ∪ token.scope ∪ challenge scope). */
  requestedScope?: string;
  resourceMetadataUrl?: URL;
  serverName: string;
  serverUrl: string;
  signal?: AbortSignal;
  transactionTtlMs?: number;
}

/**
* Phase 2: Interactive authorization transaction.
 *
* No MCP transport is created. Directly use the `auth()` driver exported by the SDK discovery → DCR → authorize →
* code exchange, so there is no hidden danger of "Phase 2 transport must be destroyed": after the authorization is successful, the caller directly uses
* Phase 1 pure AuthProvider can re-establish the connection.
 */
export async function runMcpInteractiveAuthorization(
  input: McpInteractiveAuthorizationInput,
): Promise<McpInteractiveAuthorizationOutcome> {
  const transactionTtlMs = input.transactionTtlMs ?? MCP_OAUTH_AUTHORIZATION_TRANSACTION_TTL_MS;
  const baseline = await loadCanonicalCredentials(input.credentialStore, input.keyPrefix);
  const baselineGeneration = baseline?.generation;

  const lease = await tryAcquireAuthorizationLease({
    credentialsFilePath: input.credentialStore.filePath,
    keyPrefix: input.keyPrefix,
  });
  if (!lease) {
    return await followAuthorization(input, baselineGeneration, transactionTtlMs);
  }

  try {
// In-lock reread: Others may have completed authorization while waiting for the lease.
    const current = await loadCanonicalCredentials(input.credentialStore, input.keyPrefix);
    if (hasNewerCredentials(current, baselineGeneration)) {
      return { status: "already-authorized" };
    }
    return await leadAuthorization(input, {
      attemptId: lease.attemptId,
      baselineGeneration,
      transactionTtlMs,
    });
  } finally {
    await deletePendingAuthorizationIfOwned(
      input.credentialStore,
      input.keyPrefix,
      lease.attemptId,
    ).catch(() => undefined);
    await lease.release();
  }
}

async function leadAuthorization(
  input: McpInteractiveAuthorizationInput,
  context: {
    attemptId: string;
    baselineGeneration?: string;
    transactionTtlMs: number;
  },
): Promise<McpInteractiveAuthorizationOutcome> {
  const state = randomBytes(24).toString("base64url");
  const callbackPath = normalizeCallbackPath(input.config.redirectPath, input.serverName);
//Re-listen(0) for each authorization. Completely abandon port reuse: the listener survives for a long time during the entire connection period, and reuse will inevitably cause collision;
// fresh DCR will write the URL of the currently surviving listener into redirect_uris, and port changes will no longer cause mismatches.
  let callbackServer: LocalhostOAuthCallbackServer;
  try {
    callbackServer = await createLocalhostOAuthCallbackServer({ callbackPath, state });
  } catch (error) {
// EACCES/EMFILE/ENFILE/EADDRNOTAVAIL, etc. will all be handled as leader failure, and EADDRINUSE will not be treated specially.
    input.logger?.warn("MCP OAuth callback listener failed", {
      event: "mcp.oauth.callback_listener.failed",
      ...logContext(input, state),
      error: error instanceof Error ? error.message : String(error),
      status: "failed",
    });
    return { status: "failed", error };
  }

  const provider = new InteractiveAuthorizationProvider({
    attemptId: context.attemptId,
    baselineGeneration: context.baselineGeneration,
    callbackServer,
    config: input.config,
    credentialStore: input.credentialStore,
    keyPrefix: input.keyPrefix,
    logger: input.logger,
    onAuthorizationRequired: input.onAuthorizationRequired,
    openAuthorizationUrl: input.openAuthorizationUrl,
    requestedScope: input.requestedScope,
    serverName: input.serverName,
    state,
    transactionTtlMs: context.transactionTtlMs,
  });

  try {
    const redirected = await auth(provider, {
      serverUrl: input.serverUrl,
      ...(input.requestedScope ? { scope: input.requestedScope } : {}),
      ...(input.resourceMetadataUrl ? { resourceMetadataUrl: input.resourceMetadataUrl } : {}),
      ...(input.fetchFn ? { fetchFn: input.fetchFn } : {}),
      ...(input.forceReauthorization ? { forceReauthorization: true } : {}),
    });
    if (redirected === "AUTHORIZED") {
// The client is statically configured and the server may release it directly without going through the browser.
      return { status: "authorized" };
    }

// Only the section "Waiting for authorization" has a timeout. Code exchange always waits until settled and does not race against timeouts:
// Otherwise, the lock may be released when the token response has been returned and saveTokens is still in progress, and fencing will be missed.
    const callback = await withTimeout(
      callbackServer.waitForCallback(),
      context.transactionTtlMs,
      `MCP server ${input.serverName} OAuth authorization timed out`,
      input.signal,
    );
    const callbackParams = new URL(callback.url).searchParams;
    const authorizationCode = callbackParams.get("code") ?? callback.code;
    const issuerParam = callbackParams.get("iss");
    await auth(provider, {
      serverUrl: input.serverUrl,
      authorizationCode,
      ...(issuerParam ? { iss: issuerParam } : {}),
      ...(input.requestedScope ? { scope: input.requestedScope } : {}),
      ...(input.resourceMetadataUrl ? { resourceMetadataUrl: input.resourceMetadataUrl } : {}),
      ...(input.fetchFn ? { fetchFn: input.fetchFn } : {}),
    });
    input.logger?.info("MCP OAuth authorization completed", {
      event: "mcp.oauth.authorization.completed",
      ...logContext(input, state),
      status: "completed",
    });
    return { status: "authorized" };
  } catch (error) {
// When the transaction TTL reaches the point, authorization may still be performed in the browser, but the leader has given up: turn off the listener.
// Let the next connection become the leader again instead of leaving a callback port that will not be consumed.
    const published = await loadCanonicalCredentials(input.credentialStore, input.keyPrefix);
    if (hasNewerCredentials(published, context.baselineGeneration)) {
      return { status: "already-authorized" };
    }
    input.logger?.warn("MCP OAuth authorization failed", {
      event: "mcp.oauth.authorization.failed",
      ...logContext(input, state),
      error: error instanceof Error ? error.message : String(error),
      status: "failed",
    });
    return { status: "failed", error };
  } finally {
    await callbackServer.close().catch(() => undefined);
  }
}

async function followAuthorization(
  input: McpInteractiveAuthorizationInput,
  baselineGeneration: string | undefined,
  transactionTtlMs: number,
): Promise<McpInteractiveAuthorizationOutcome> {
  const deadline = Date.now() + transactionTtlMs;
  let projectedUrl: string | undefined;
  input.logger?.info("MCP OAuth authorization is already in progress elsewhere", {
    event: "mcp.oauth.authorization.following",
    ...logContext(input),
    status: "waiting",
  });

  while (Date.now() < deadline && !input.signal?.aborted) {
    const current = await loadCanonicalCredentials(input.credentialStore, input.keyPrefix);
    if (hasNewerCredentials(current, baselineGeneration)) return { status: "already-authorized" };

    const pending = await loadPendingAuthorization(input.credentialStore, input.keyPrefix);
    if (pending && pending.authorizationUrl !== projectedUrl) {
// The setting page and session are independent leases, and the leader's onAuthorizationRequired callback is for the follower.
// Invisible; the follower must project the same authorization URL from the shared pending key to its own state.
      projectedUrl = pending.authorizationUrl;
      await input.onAuthorizationRequired?.({
        authorizationUrl: pending.authorizationUrl,
        redirectUrl: "",
        serverName: input.serverName,
      });
    }
    await sleep(FOLLOWER_POLL_INTERVAL_MS, input.signal);
  }

  return { status: "pending", ...(projectedUrl ? { authorizationUrl: projectedUrl } : {}) };
}

/**
* Phase 2 dedicated OAuthClientProvider.
 *
* In contrast to Phase 1's pure AuthProvider, here a full `OAuthClientProvider` is required to drive `auth()`;
* But it only survives within authorized transactions, and:
* - `clientInformation()` only recognizes statically configured clientId, and the rest will return undefined, forcing fresh DCR;
* - `saveClientInformation()` only writes to transaction memory and never writes to disk;
* - `tokens()` is always undefined and never triggers refresh (refresh is the only responsibility of Phase 1);
* - PKCE verifier only stores memory: the entire transaction is completed in the same process and within the same lease, and there is no provider reconstruction.
 */
class InteractiveAuthorizationProvider implements OAuthClientProvider {
  private readonly attemptId: string;
  private readonly baselineGeneration?: string;
  private readonly callbackServer: LocalhostOAuthCallbackServer;
  private readonly config: McpAuthorizationCodeOAuthConfig;
  private readonly credentialStore: SharedZCodeCredentialStore;
  private readonly keyPrefix: string;
  private readonly logger?: Logger;
  private readonly onAuthorizationRequired?: (
    context: McpOAuthAuthorizationContext,
  ) => Promise<void> | void;
  private readonly openAuthorizationUrl?: (
    context: McpOAuthAuthorizationContext,
  ) => Promise<void> | void;
/** The final scope calculated by the orchestration layer (union in step-up); DCR and authorize requests must use the same value. */
  private readonly requestedScope?: string;
  private readonly serverName: string;
  private readonly stateValue: string;
  private readonly transactionId: string;
  private readonly transactionTtlMs: number;
  private issuer?: string;
  private memoryCodeVerifier?: string;
  private memoryDiscoveryState?: OAuthDiscoveryState;
  private transactionClientInformation?: OAuthClientInformationMixed;

  constructor(input: {
    attemptId: string;
    baselineGeneration?: string;
    callbackServer: LocalhostOAuthCallbackServer;
    config: McpAuthorizationCodeOAuthConfig;
    credentialStore: SharedZCodeCredentialStore;
    keyPrefix: string;
    logger?: Logger;
    onAuthorizationRequired?: (context: McpOAuthAuthorizationContext) => Promise<void> | void;
    openAuthorizationUrl?: (context: McpOAuthAuthorizationContext) => Promise<void> | void;
    requestedScope?: string;
    serverName: string;
    state: string;
    transactionTtlMs: number;
  }) {
    this.attemptId = input.attemptId;
    this.baselineGeneration = input.baselineGeneration;
    this.callbackServer = input.callbackServer;
    this.config = input.config;
    this.credentialStore = input.credentialStore;
    this.keyPrefix = input.keyPrefix;
    this.logger = input.logger;
    this.onAuthorizationRequired = input.onAuthorizationRequired;
    this.openAuthorizationUrl = input.openAuthorizationUrl;
    this.requestedScope = input.requestedScope;
    this.serverName = input.serverName;
    this.stateValue = input.state;
    this.transactionId = createHash("sha256").update(input.state).digest("hex");
    this.transactionTtlMs = input.transactionTtlMs;
  }

  get redirectUrl(): string {
    return this.callbackServer.callbackUrl;
  }

  get clientMetadata(): OAuthClientMetadata {
    return {
      client_name: this.config.clientName ?? `ZCode ${this.serverName}`,
      grant_types: ["authorization_code", "refresh_token"],
      redirect_uris: [this.redirectUrl],
      response_types: ["code"],
      ...(this.config.clientSecret ? { token_endpoint_auth_method: "client_secret_basic" } : {}),
// The scope registered by DCR and the scope requested by authorize must be the same union result.
// If DCR only writes config scope, the registered client is inconsistent with subsequent authorization requests initiated by union.
// Strict authorization servers will refuse or silently converge by registered values.
      ...((this.requestedScope ?? this.config.scope)
        ? { scope: this.requestedScope ?? this.config.scope }
        : {}),
    };
  }

  state(): string {
    return this.stateValue;
  }

  clientInformation(): OAuthClientInformationMixed | undefined {
    if (this.config.clientId) {
      return {
        client_id: this.config.clientId,
        ...(this.config.clientSecret ? { client_secret: this.config.clientSecret } : {}),
      };
    }
// In the past, the persistent DCR client was returned first, and its redirect_uris was locked in
//Register the random port at that time. The authorization request then carries "old client_id + new redirect_uri", and the authorization server presses
// RFC 6749 §4.1.2.1 prohibits jumping back and rendering the error page in place, the callback never arrives and retries never heal.
// Return undefined to let the SDK re-register with the URL of the currently surviving listener, and the mismatch is eliminated from the root.
    return this.transactionClientInformation;
  }

  saveClientInformation(clientInformation: OAuthClientInformationMixed): void {
// Write transactional memory only. DCR client is only meaningful if it is paired with the token exchanged for this authorization; it is placed separately
// It will also pollute the canonical pairs of other transactions.
    this.transactionClientInformation = clientInformation;
  }

  tokens(): undefined {
    return undefined;
  }

  async saveTokens(tokens: OAuthTokens): Promise<void> {
    const clientInformation = this.clientInformation();
    if (!clientInformation) {
      throw new Error(`Missing MCP OAuth client information for ${this.serverName}`);
    }
    const published = await publishCanonicalCredentials(this.credentialStore, this.keyPrefix, {
      clientInformation,
      ...(this.issuer ? { issuer: this.issuer } : {}),
      publishedBy: this.transactionId,
      tokens,
    });
    this.logger?.info("MCP OAuth credentials published", {
      event: "mcp.oauth.credentials.published",
      ...this.logContext(),
      clientIdHash: hashIdentifier(clientInformation.client_id),
      grantKind: "authorization_code",
      hasRefreshToken: Boolean(tokens.refresh_token),
      publishedGeneration: published.generation.slice(0, 12),
      status: "completed",
      tokenExpiresInSeconds: tokens.expires_in,
    });
  }

  async redirectToAuthorization(authorizationUrl: URL): Promise<void> {
    const context: McpOAuthAuthorizationContext = {
      authorizationUrl: authorizationUrl.toString(),
      redirectUrl: this.redirectUrl,
      serverName: this.serverName,
    };
// Pending is bound to attempt: delete according to attempt CAS, the old leader's finally will not erase the new leader's
//pending. TTL is only used to display expiration judgment and does not assume lock ownership semantics.
    await publishPendingAuthorization(this.credentialStore, this.keyPrefix, {
      attemptId: this.attemptId,
      authorizationUrl: context.authorizationUrl,
      ...(this.baselineGeneration ? { baselineGeneration: this.baselineGeneration } : {}),
      expiresAt: Date.now() + this.transactionTtlMs,
      state: this.stateValue,
    });
    this.logger?.info("MCP OAuth authorization required", {
      event: "mcp.oauth.authorization.required",
      ...this.logContext(),
      callbackPort: Number(new URL(this.redirectUrl).port),
      status: "waiting",
    });
    await this.onAuthorizationRequired?.(context);
// Only the URL is exposed by default, waiting for the user to click authorization on the settings page; automatically launching the browser will interrupt the current operation.
    await this.openAuthorizationUrl?.(context);
  }

  saveCodeVerifier(codeVerifier: string): void {
    this.memoryCodeVerifier = codeVerifier;
  }

  codeVerifier(): string {
    if (!this.memoryCodeVerifier) {
      throw new Error(`Missing MCP OAuth PKCE verifier for ${this.serverName}`);
    }
    return this.memoryCodeVerifier;
  }

  saveAuthorizationServerUrl(authorizationServerUrl: string): void {
    this.issuer = authorizationServerUrl;
  }

  authorizationServerUrl(): string | undefined {
    return this.issuer;
  }

  async saveDiscoveryState(state: OAuthDiscoveryState): Promise<void> {
// Also leave a copy of the memory in the transaction: the code exchange leg needs to be able to read back the authorize leg records
// issuer, otherwise the SDK throws AuthorizationServerMismatchError. Shared records may be overwritten by other processes or expire.
// The memory copy ensures that the issuer binding within the same transaction is stable.
    this.memoryDiscoveryState = state;
    await saveDiscoveryRecord(this.credentialStore, this.keyPrefix, state);
  }

  async discoveryState(): Promise<OAuthDiscoveryState | undefined> {
    if (this.memoryDiscoveryState) return this.memoryDiscoveryState;
    return await loadDiscoveryRecord(this.credentialStore, this.keyPrefix);
  }

  private logContext(): Record<string, unknown> {
    return {
      credentialKeyPrefix: this.keyPrefix,
      mcpServerName: this.serverName,
      oauthAttemptId: this.attemptId.slice(0, 12),
      oauthStateId: this.transactionId.slice(0, 16),
      processId: process.pid,
    };
  }
}

function hasNewerCredentials(
  current: CanonicalCredentialSnapshot | undefined,
  baselineGeneration: string | undefined,
): boolean {
  return Boolean(current?.tokens && current.generation !== baselineGeneration);
}

function normalizeCallbackPath(value: string | undefined, serverName: string): string {
  const fallback = `/oauth/callback/mcp/${encodeURIComponent(serverName)}`;
  if (!value) return fallback;
  return value.startsWith("/") ? value : `/${value}`;
}

function logContext(
  input: McpInteractiveAuthorizationInput,
  state?: string,
): Record<string, unknown> {
  return {
    adapterInstanceId: input.adapterInstanceId,
    credentialKeyPrefix: input.keyPrefix,
    mcpServerName: input.serverName,
    ...(state
      ? { oauthStateId: createHash("sha256").update(state).digest("hex").slice(0, 16) }
      : {}),
    processId: process.pid,
  };
}

function hashIdentifier(value: string | undefined): string | undefined {
  if (!value) return undefined;
  return createHash("sha256").update(value).digest("hex").slice(0, 12);
}

function sleep(durationMs: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    }, durationMs);
    const onAbort = () => {
      clearTimeout(timer);
      resolve();
    };
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}
