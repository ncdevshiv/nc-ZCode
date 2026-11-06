import { createHash } from "node:crypto";
import type { McpOAuthConfig } from "@zcode/contracts";
import { type SharedZCodeCredentialStore } from "../auth/shared-credentials.js";
import { type McpOAuthAuthorizationContext } from "./oauth-shared.js";
type McpAuthorizationCodeOAuthConfig = Extract<McpOAuthConfig, { type: "authorization_code" }>;

export type { McpOAuthAuthorizationContext };

export interface McpOAuthRuntimeOptions {
  authorizationTimeoutMs?: number;
  credentialStore?: SharedZCodeCredentialStore;
  onAuthorizationRequired?: (context: McpOAuthAuthorizationContext) => Promise<void> | void;
  openAuthorizationUrl?: (context: McpOAuthAuthorizationContext) => Promise<void> | void;
}

export function createCredentialKeyPrefix(
  serverName: string,
  serverUrl: string,
  config: McpAuthorizationCodeOAuthConfig,
): string {
// Both OAuth token and dynamic client registration rely on authorization semantics, and must be re-authorized when scope/client/redirect changes.
  const hash = createHash("sha256")
    .update(
      [
        serverName,
        serverUrl,
        config.clientId ?? "",
        config.scope ?? "",
        config.redirectPath ?? "",
      ].join("\n"),
    )
    .digest("hex")
    .slice(0, 24);
  return `mcp:oauth:${hash}`;
}
