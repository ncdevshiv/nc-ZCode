import type { FetchLike, AuthProvider } from "@modelcontextprotocol/client";
import type { Logger, McpOAuthConfig } from "@zcode/contracts";
import type { SharedZCodeCredentialStore } from "../auth/shared-credentials.js";
import { isCanonicalTokenNearExpiry, loadCredentialPair } from "./oauth-credentials.js";
import { createInteractiveAuthorizationRequiredError } from "./oauth-errors.js";
import { refreshMcpOAuthTokensUnderLock } from "./oauth-refresh.js";

type McpAuthorizationCodeOAuthConfig = Extract<McpOAuthConfig, { type: "authorization_code" }>;

interface CreateMcpOAuthTokenProviderInput {
  config: McpAuthorizationCodeOAuthConfig;
  credentialStore: SharedZCodeCredentialStore;
  fetchFn?: FetchLike;
  keyPrefix: string;
  logger?: Logger;
  serverName: string;
  serverUrl: string;
}

/**
* Phase 1 runtime AuthProvider.
 *
* Only implements `token()` and `onUnauthorized()`, **not** `OAuthClientProvider`: transport
* The assertion of `isOAuthClientProvider()` is therefore false and `_oauthProvider` remains empty
* (client@2.0.0 `index.mjs:4977-4980`), then:
 *
* - No discovery, no DCR, no callback listener enabled when establishing a connection;
* - 401 only calls our `onUnauthorized()` and automatically retries once, the SDK's `auth()` does not participate at all,
* All refreshes are forced to import our cross-process solo lock.
 *
* Never pass `OAuthClientProvider` to the runtime transport: it will be wrapped by `adaptOAuthProvider`,
* 401 uses SDK's `handleOAuthUnauthorized()` → `auth()` to bypass the refresh lock, and the concurrent refresh problem recurs immediately.
 */
export function createMcpOAuthTokenProvider(input: CreateMcpOAuthTokenProviderInput): AuthProvider {
  const refreshInput = {
    credentialStore: input.credentialStore,
    ...(input.fetchFn ? { fetchFn: input.fetchFn } : {}),
    keyPrefix: input.keyPrefix,
    ...(input.logger ? { logger: input.logger } : {}),
    serverName: input.serverName,
    serverUrl: input.serverUrl,
    ...(input.config.clientId ? { staticClientId: input.config.clientId } : {}),
  };

  return {
    async token(): Promise<string | undefined> {
      const pair = await loadCredentialPair(input.credentialStore, input.keyPrefix);
// When there is no token, undefined is returned: the request is sent as usual, 401 is obtained, and then classified uniformly by onUnauthorized.
      if (!pair?.tokens) return undefined;
      if (!isCanonicalTokenNearExpiry(pair)) return pair.tokens.access_token;
//Proactively refresh on deadline. If there is no refresh token, just use the current value to support 401 without imaginary refresh.
      if (!pair.tokens.refresh_token) return pair.tokens.access_token;
      return await refreshMcpOAuthTokensUnderLock({ ...refreshInput, reactive: false });
    },

    async onUnauthorized(): Promise<void> {
      const pair = await loadCredentialPair(input.credentialStore, input.keyPrefix);
      if (!pair?.tokens?.refresh_token) {
        throw createInteractiveAuthorizationRequiredError({
          reason: pair?.tokens ? "no_refresh_token" : "no_credentials",
          serverName: input.serverName,
        });
      }
// The contract is "let token() return the available token next time", the return value itself is ignored by the SDK; the refresh result has been published to
// canonical, token() will be re-read next time.
      await refreshMcpOAuthTokensUnderLock({ ...refreshInput, reactive: true });
    },
  };
}
