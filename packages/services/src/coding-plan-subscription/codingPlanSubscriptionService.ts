import type { ApiClient } from "@zcode/shared";
import type { ICredentialService } from "../credential/credential.js";
import type { ICodingPlanSubscriptionService } from "./codingPlanSubscription.js";
import type { ModelSelectionView } from "@zcode/provider";

interface CodingPlanSubscriptionServiceDependencies {
  apiClient: ApiClient;
  credentialService: Pick<ICredentialService, "load">;
  resolveOffPeakModelSelectionView?: () => Promise<ModelSelectionView>;
}

export function createCodingPlanSubscriptionService(
  _dependencies: CodingPlanSubscriptionServiceDependencies,
): ICodingPlanSubscriptionService {
  const disabledError = () => new Error("CodingPlanPurchaseRemoved");
  const emptyModelSelectionView: ModelSelectionView = {
    providers: [],
    groups: [],
  };

  return {
    batchPreview: async () => ({ items: [] } as any),
    getStaticProducts: async () => ({ products: [], teamProducts: [] } as any),
    getStaticTeamProducts: async () => ({ teamProducts: [] } as any),
    getStartPlanPreview: async () => null,
    getOffPeakClientConfig: async () => ({ enabled: false, modelSelectionView: emptyModelSelectionView }),
    getDynamicWorkflowClientConfig: async () => ({ mode: "onDemand" } as any),
    getModelContextBudgetStrategy: async () => ({ strategy: "preflight-v1" } as any),
    getForceUpdateConfig: async () => null,
    productInfo: async () => { throw disabledError(); },
    preview: async () => { throw disabledError(); },
    createSign: async () => { throw disabledError(); },
    updateSign: async () => { throw disabledError(); },
    checkPayment: async () => { throw disabledError(); },
    checkPendingOrders: async () => ({ orders: [] } as any),
    queryStripeCards: async () => [],
    bindStripeCard: async () => { throw disabledError(); },
    unbindStripeCard: async () => { throw disabledError(); },
    payStripe: async () => { throw disabledError(); },
    checkPaypalSupport: async () => ({ supported: false } as any),
    createPaypalSetupToken: async () => { throw disabledError(); },
    subscribePaypal: async () => { throw disabledError(); },
    getEnterprisePricing: async () => { throw disabledError(); },
    getEnterpriseBalance: async () => { throw disabledError(); },
    calculateEnterpriseOrder: async () => { throw disabledError(); },
    createEnterpriseOrder: async () => { throw disabledError(); },
    getEnterprisePendingOrders: async () => [],
    cancelEnterpriseOrder: async () => { throw disabledError(); },
    continueEnterpriseOrderPayment: async () => { throw disabledError(); },
    checkEnterpriseOrderStatus: async () => { throw disabledError(); },
  };
}
