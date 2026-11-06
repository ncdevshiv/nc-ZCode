import type { IOAuthService } from "./oauth.js";
import type { OAuthCachedSessionRestoreResult, OAuthCallbackResult, OAuthProviderId, OAuthProviderMeta, OAuthStartResponse, UserInfo } from "@zcode/shared";
import type { ICredentialService } from "../credential/credential.js";

interface OAuthServiceDependencies {
  adapters?: any[];
  apiClient?: any;
  now?: () => number;
  env?: NodeJS.ProcessEnv;
  onProviderLogout?: (provider: OAuthProviderId, accountIdentity?: string | null) => Promise<void>;
}

export class OAuthService implements IOAuthService {
  constructor(_credentialService: ICredentialService, _dependencies: OAuthServiceDependencies = {}) {}

  async getProviders(): Promise<OAuthProviderMeta[]> {
    return [];
  }

  async getActiveProvider(): Promise<OAuthProviderId | null> {
    return null;
  }

  async restoreCachedSession(): Promise<UserInfo | null> {
    return null;
  }

  async restoreCachedSessionState(): Promise<OAuthCachedSessionRestoreResult> {
    return { status: "signed-out" };
  }

  async restoreSession(): Promise<UserInfo | null> {
    return null;
  }

  async startOAuth(_provider: OAuthProviderId): Promise<OAuthStartResponse> {
    throw new Error("OAuth disabled");
  }

  async startOAuthWithPolling(_provider: OAuthProviderId): Promise<OAuthStartResponse> {
    throw new Error("OAuth disabled");
  }

  async pollPendingOAuth(): Promise<OAuthCallbackResult | null> {
    return null;
  }

  async handleCallback(_url: string): Promise<OAuthCallbackResult | null> {
    return null;
  }

  async refreshToken(_provider?: OAuthProviderId): Promise<void> {
    throw new Error("OAuth disabled");
  }

  async logout(_provider?: OAuthProviderId): Promise<void> {
    return;
  }

  async logoutAll(): Promise<void> {
    return;
  }

  async cancelPending(_provider?: OAuthProviderId): Promise<void> {
    return;
  }

  logoutIfCurrentCredentialRequest(_input: string | URL, _headers: Headers): Promise<boolean> {
    return Promise.resolve(false);
  }
}

export function createOAuthService(
  credentialService: ICredentialService,
  dependencies: Omit<OAuthServiceDependencies, "adapters"> = {},
): OAuthService {
  return new OAuthService(credentialService, dependencies);
}
