//! Coding plan subscription channel — mirrors the disabled TS implementation
//! (`packages/services/src/coding-plan-subscription/codingPlanSubscriptionService.ts`).
//!
//! Purchase/subscription mutations fail with `CodingPlanPurchaseRemoved`; read methods
//! return explicit disabled/empty defaults. No network calls, no fake success.

use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::ServiceArgs;
use serde_json::json;

const PURCHASE_REMOVED: &str = "CodingPlanPurchaseRemoved";

pub struct CodingPlanSubscriptionService;

impl CodingPlanSubscriptionService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl Default for CodingPlanSubscriptionService {
    fn default() -> Self {
        Self
    }
}

fn removed() -> ChannelFailure {
    ChannelFailure::error(PURCHASE_REMOVED)
}

#[async_trait]
impl Channel for CodingPlanSubscriptionService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let _args = ServiceArgs::new(args);
        let response = match command {
            "batchPreview" => json!({ "items": [] }),
            "getStaticProducts" => json!({ "products": [], "teamProducts": [] }),
            "getStaticTeamProducts" => json!({ "teamProducts": [] }),
            "getStartPlanPreview" | "getForceUpdateConfig" => serde_json::Value::Null,
            "getOffPeakClientConfig" => json!({
                "enabled": false,
                "modelSelectionView": { "providers": [], "groups": [] }
            }),
            "getDynamicWorkflowClientConfig" => json!({ "mode": "onDemand" }),
            "getModelContextBudgetStrategy" => json!({ "strategy": "preflight-v1" }),
            "checkPendingOrders" => json!({ "orders": [] }),
            "queryStripeCards" | "getEnterprisePendingOrders" => json!([]),
            "checkPaypalSupport" => json!({ "supported": false }),
            "productInfo"
            | "preview"
            | "createSign"
            | "updateSign"
            | "checkPayment"
            | "bindStripeCard"
            | "unbindStripeCard"
            | "payStripe"
            | "createPaypalSetupToken"
            | "subscribePaypal"
            | "getEnterprisePricing"
            | "getEnterpriseBalance"
            | "calculateEnterpriseOrder"
            | "createEnterpriseOrder"
            | "cancelEnterpriseOrder"
            | "continueEnterpriseOrderPayment"
            | "checkEnterpriseOrderStatus" => return Err(removed()),
            other => return Err(ChannelFailure::method_not_found(other)),
        };
        Ok(WireValue::Json(response))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;

    fn ctx() -> ChannelContext {
        ChannelContext::new("coding-plan-subscription", ClientMode::DesktopContinuous)
    }

    async fn call(command: &str) -> Result<WireValue, ChannelFailure> {
        CodingPlanSubscriptionService::new()
            .call(&ctx(), command, vec![], CancelToken::new())
            .await
    }

    fn json_value(value: WireValue) -> serde_json::Value {
        match value {
            WireValue::Json(value) => value,
            other => panic!("expected json wire value, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn disabled_reads_return_explicit_defaults() {
        let batch = json_value(call("batchPreview").await.expect("batch"));
        assert_eq!(batch["items"], json!([]));
        let offpeak = json_value(call("getOffPeakClientConfig").await.expect("offpeak"));
        assert_eq!(offpeak["enabled"], json!(false));
        assert_eq!(offpeak["modelSelectionView"]["providers"], json!([]));
        let dynamic = json_value(
            call("getDynamicWorkflowClientConfig")
                .await
                .expect("dynamic"),
        );
        assert_eq!(dynamic["mode"], json!("onDemand"));
        let strategy = json_value(
            call("getModelContextBudgetStrategy")
                .await
                .expect("strategy"),
        );
        assert_eq!(strategy["strategy"], json!("preflight-v1"));
    }

    #[tokio::test]
    async fn purchase_mutations_fail_closed_with_stable_code() {
        for method in [
            "productInfo",
            "preview",
            "payStripe",
            "createEnterpriseOrder",
            "checkEnterpriseOrderStatus",
        ] {
            let err = call(method).await.expect_err(method);
            let text = err.to_string();
            assert!(text.contains(PURCHASE_REMOVED), "{method}: {text}");
        }
    }

    #[tokio::test]
    async fn unknown_method_is_not_found() {
        let err = call("nope").await.expect_err("not found");
        let text = err.to_string().to_lowercase();
        assert!(text.contains("nope") || text.contains("method"));
    }
}
