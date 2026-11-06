import { isDeepStrictEqual } from "node:util";
import type {
  AccountProviderUnavailableReason,
  AccountProviderConnectionResolver,
  AccountProviderConnectionResult,
  ProviderConfigSnapshot,
  ProviderSource,
} from "@zcode/provider";
import { AccountProviderService, createAccountProviderConfigResolver } from "@zcode/provider";
import type {
  ApiClient,
  ProviderFamilyConnectionSelectionSettings,
  ProviderFamilyDomain,
  ZCodeAccountAccess,
  ZCodeProviderAccountAccess,
} from "@zcode/shared";
import type {
  CodingPlanAvailabilityProvider,
  CodingPlanAvailabilityResult,
  CodingPlanUnavailableReason,
} from "#src/model-provider/codingPlanProviderAvailability.js";
import {
  validateBigModelAccountProviderAvailability,
  validateZaiAccountProviderAvailability,
} from "#src/model-provider/codingPlanProviderAvailability.js";

export interface AccountProviderConnectionSettings {
  readonly providerFamilyDomain: ProviderFamilyDomain | null;
  readonly selections: ProviderFamilyConnectionSelectionSettings;
  readonly unresolvedFamilies?: readonly ProviderFamilyDomain[];
}

export interface AccountProviderFamilyAvailabilityInput {
  readonly family: ProviderFamilyDomain;
  readonly providers: readonly CodingPlanAvailabilityProvider[];
  readonly selections: ProviderFamilyConnectionSelectionSettings;
}

export type AccountProviderFamilyAvailabilityResolver = (
  input: AccountProviderFamilyAvailabilityInput,
) => Promise<Partial<Record<string, CodingPlanAvailabilityResult>>>;

export interface AccountProviderConnectionResolverOptions {
  readonly readSettings: () => Promise<AccountProviderConnectionSettings>;
  readonly loadCodingPlanApiKey: (
    providerId: string,
    family: ProviderFamilyDomain,
    accountIdentity: string,
    forceRefresh: boolean,
  ) => Promise<string | null>;
  readonly loadAccountIdentity: (family: ProviderFamilyDomain) => Promise<string | null>;
  readonly resolveFamilyAvailability: AccountProviderFamilyAvailabilityResolver;
}

export interface CodingPlanFamilyAvailabilityResolverOptions {
  readonly apiClient: ApiClient;
  readonly credentialService?: {
    load(key: string): Promise<string | null>;
  };
}

export interface AccountProviderConfigSourceOptions extends AccountProviderConnectionResolverOptions {
  readonly configSource: ProviderSource<ProviderConfigSnapshot>;
}

export function createAccountProviderConnectionResolver(
  _options: AccountProviderConnectionResolverOptions,
): AccountProviderConnectionResolver {
  return async ({ configuredProviders }) => {
    const connections: AccountProviderConnectionResult[] = [];
    for (const [providerId, config] of configuredProviders.entries()) {
      const access = config.access;
      if (access?.type === "zhipu-account") {
        connections.push({ providerId, status: "unavailable" });
        continue;
      }
    }
    return Object.freeze(connections);
  };
}

function isCredentialRefreshReason(reason: string): boolean {
  return (
    reason.includes("oauth-callback") || reason.split(":").at(-1) === "oauth-login-entitlement"
  );
}

function resolveAccountUnavailableReason(
  reason: CodingPlanUnavailableReason,
): AccountProviderUnavailableReason {
  switch (reason) {
    case "coding_plan_not_authenticated":
      return "not-authenticated";
    case "coding_plan_not_connected":
      return "not-connected";
    case "coding_plan_auth_failed":
      return "credential-failed";
    case "coding_plan_not_entitled":
      return "not-entitled";
  }
}

export function createAccountProviderConfigSource(
  options: AccountProviderConfigSourceOptions,
): AccountProviderService {
  return new AccountProviderService({
    configSource: options.configSource,
    resolve: createAccountProviderConfigResolver(createAccountProviderConnectionResolver(options)),
  });
}

export function createCodingPlanFamilyAvailabilityResolver(
  options: CodingPlanFamilyAvailabilityResolverOptions,
): AccountProviderFamilyAvailabilityResolver {
  return ({ family, providers, selections }) => {
    const context = {
      apiClient: options.apiClient,
      credentialService: options.credentialService,
      providerFamilyConnectionSelections: selections,
    };
    return family === "zai"
      ? validateZaiAccountProviderAvailability(providers, context)
      : validateBigModelAccountProviderAvailability(providers, context);
  };
}

export async function resolveCurrentAccountAccess(input: {
  readonly access: ZCodeProviderAccountAccess;
  readonly readSettings: () => Promise<AccountProviderConnectionSettings>;
  readonly loadAccountIdentity: (family: ProviderFamilyDomain) => Promise<string | null>;
}): Promise<ZCodeAccountAccess | null> {
  const settings = await input.readSettings();
  const { accountType, mode } = input.access;
  if (settings.providerFamilyDomain !== accountType) return null;
  if (mode === "start-plan") {
    if (!(await input.loadAccountIdentity(accountType))?.trim()) return null;
    return { type: "zhipu-account", family: accountType, planKind: "start-plan" };
  }
  const selection = settings.selections[accountType];
  if (!selection) return null;
  if (mode === "off-peak") {
    if (selection.kind !== "individual-coding-plan" && selection.kind !== "team-coding-plan") {
      return null;
    }
  } else if (selection.kind !== mode) {
    return null;
  }
  if (!(await input.loadAccountIdentity(accountType))?.trim()) return null;
  if (selection.kind === "team-coding-plan") {
    return {
      type: "zhipu-account",
      family: accountType,
      planKind: selection.kind,
      productId: selection.productId,
      organizationId: selection.organizationId,
      projectId: selection.projectId,
    };
  }
  return {
    type: "zhipu-account",
    family: accountType,
    planKind: selection.kind,
  };
}
