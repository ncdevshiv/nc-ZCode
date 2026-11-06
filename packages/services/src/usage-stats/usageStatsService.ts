import type {
  AppUsageRequest,
  AppUsageSnapshot,
  CodingPlanUsageRequest,
  CodingPlanUsageSnapshot,
  CodingPlanResetOpportunityRequest,
  CodingPlanResetOpportunityResult,
  CodingPlanResetScopeRequest,
  CodingPlanResetStatusSnapshot,
  CodingPlanResetUseRequest,
  CodingPlanResetUseResult,
  UsageEntitlementRequest,
  UsageEntitlementSnapshot,
  UsageStatsRequest,
  UsageStatsSnapshot,
} from "@zcode/shared";
import type { IUsageStatsService } from "./usageStats.js";

interface UsageStatsServiceDependencies {
  apiClient: any;
  accountRequestAuthService: any;
  resolveApiAuthorization?: any;
  credentialService?: any;
  env?: NodeJS.ProcessEnv;
  zcodeAgentService?: any;
  officialMcpCredentialSource?: any;
}

export function createUsageStatsService(
  _dependencies: UsageStatsServiceDependencies,
): IUsageStatsService {
  const emptyAppUsage: AppUsageSnapshot = {
    totalTokens: 0,
    totalCostUsd: 0,
    breakdown: [],
  } as any;

  const emptyCodingPlanUsage: CodingPlanUsageSnapshot = {
    usage: 0,
    quota: 0,
    remaining: 0,
    resetAt: null,
  } as any;

  const emptyResetStatus: CodingPlanResetStatusSnapshot = {
    hasOpportunity: false,
    opportunities: [],
  } as any;

  const emptyUsageStats: UsageStatsSnapshot = {
    usage: 0,
    quota: 0,
    remaining: 0,
  } as any;

  const emptyEntitlement: UsageEntitlementSnapshot = {
    entitled: false,
  } as any;

  return {
    async getAppUsageSnapshot(_request: AppUsageRequest): Promise<AppUsageSnapshot> {
      return emptyAppUsage;
    },
    async getCodingPlanUsageSnapshot(_request: CodingPlanUsageRequest): Promise<CodingPlanUsageSnapshot> {
      throw new Error("no_bigmodel_api_key");
    },
    async getCodingPlanResetStatus(_request: CodingPlanResetScopeRequest): Promise<CodingPlanResetStatusSnapshot> {
      throw new Error("no_bigmodel_api_key");
    },
    async requestCodingPlanResetOpportunity(_request: CodingPlanResetOpportunityRequest): Promise<CodingPlanResetOpportunityResult> {
      throw new Error("no_bigmodel_api_key");
    },
    async useCodingPlanReset(_request: CodingPlanResetUseRequest): Promise<CodingPlanResetUseResult> {
      throw new Error("no_bigmodel_api_key");
    },
    async markCodingPlanResetHistoryRead(_request: CodingPlanResetScopeRequest): Promise<void> {
      throw new Error("no_bigmodel_api_key");
    },
    async getSnapshot(_request: UsageStatsRequest): Promise<UsageStatsSnapshot> {
      return emptyUsageStats;
    },
    async getEntitlementSnapshot(_request: UsageEntitlementRequest = {}): Promise<UsageEntitlementSnapshot> {
      return emptyEntitlement;
    },
  };
}
