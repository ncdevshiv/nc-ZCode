//! Runtime client config channel (`coding-plan-subscription`).
//!
//! Only the generic, non-purchase config methods survive the Z.ai claim boundary:
//! dynamic workflow rollout, off-peak rollout, context-budget strategy and force-update.
//! Coding-plan purchase/team/enterprise methods are removed product surface and fail
//! explicitly instead of pretending to work.
//!
//! Rollout flags are **local decisions** here: the TS service reads
//! `{origin}/api/v1/client/configs`, and this build keeps no zcode cloud connection other
//! than auth/token claim and account/model setup (`specs/disabled-features.md`). The
//! answers therefore are: workflow `onDemand` (the engine now lives in the native host —
//! `specs/workflow-capability-boundary.md`; the operator override still wins), off-peak
//! `enabled:false` (its scheduler is a zcode cloud service), and force-update `null`
//! (no forced-update payload is fetched).
//! `nc_code_DYNAMIC_WORKFLOW_MODE` stays as the operator override the TS service also has.

use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::ServiceArgs;
use serde_json::{json, Value};

use crate::selection::ModelSelectionService;

const WORKFLOW_MODES: [&str; 3] = ["disabled", "onDemand", "alwaysOn"];

pub struct RuntimeConfigService {
    selection: Arc<ModelSelectionService>,
}

impl RuntimeConfigService {
    pub fn new(selection: Arc<ModelSelectionService>) -> Arc<Self> {
        Arc::new(Self { selection })
    }

    fn dynamic_workflow(&self) -> Value {
        match env_mode("ZCODE_DYNAMIC_WORKFLOW_MODE") {
            Some(mode) => workflow_config(mode, "override"),
            None => workflow_config("onDemand", "default"),
        }
    }

    async fn off_peak(&self) -> Value {
        let view = self
            .selection
            .view(None)
            .await
            .unwrap_or_else(|_| json!({"revision": 0, "providers": []}));
        // The model selection view is local provider state; the rollout flag is not
        // fetched, so off-peak task scheduling stays unavailable in this build.
        json!({
            "enabled": false,
            "modelSelectionView": view,
        })
    }
}

#[async_trait]
impl Channel for RuntimeConfigService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        match command {
            // forceRefresh is part of the TS signature; there is no remote snapshot to refresh.
            "getDynamicWorkflowClientConfig" => {
                let _ = args
                    .json(0)?
                    .and_then(|options| options.get("forceRefresh").and_then(Value::as_bool));
                Ok(WireValue::Json(self.dynamic_workflow()))
            }
            "getOffPeakClientConfig" => {
                let _ = args
                    .json(0)?
                    .and_then(|options| options.get("forceRefresh").and_then(Value::as_bool));
                Ok(WireValue::Json(self.off_peak().await))
            }
            "getModelContextBudgetStrategy" => Ok(WireValue::Json(json!("preflight-v1"))),
            "getForceUpdateConfig" => Ok(WireValue::Json(Value::Null)),
            other if is_removed_purchase_method(other) => Err(ChannelFailure::error_named(
                "CodingPlanPurchaseRemoved",
                format!("coding-plan purchase flow is not part of this product: {other}"),
            )),
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

fn is_removed_purchase_method(command: &str) -> bool {
    matches!(
        command,
        "batchPreview"
            | "getStaticProducts"
            | "getStaticTeamProducts"
            | "getStartPlanPreview"
            | "productInfo"
            | "preview"
            | "createSign"
            | "updateSign"
            | "checkPayment"
            | "checkPendingOrders"
            | "queryStripeCards"
            | "bindStripeCard"
            | "unbindStripeCard"
            | "payStripe"
            | "checkPaypalSupport"
            | "createPaypalSetupToken"
            | "subscribePaypal"
            | "getEnterprisePricing"
            | "getEnterpriseBalance"
            | "calculateEnterpriseOrder"
            | "createEnterpriseOrder"
            | "getEnterprisePendingOrders"
            | "cancelEnterpriseOrder"
            | "continueEnterpriseOrderPayment"
            | "checkEnterpriseOrderStatus"
    )
}

fn normalize_mode(value: &str) -> Option<&'static str> {
    WORKFLOW_MODES
        .iter()
        .find(|mode| **mode == value.trim())
        .copied()
}

fn env_mode(key: &str) -> Option<&'static str> {
    std::env::var(key).ok().as_deref().and_then(normalize_mode)
}

fn workflow_config(mode: &str, source: &str) -> Value {
    json!({
        "mode": mode,
        "enabled": mode != "disabled",
        "source": source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_modes_are_normalized_and_fail_closed() {
        assert_eq!(normalize_mode(" alwaysOn "), Some("alwaysOn"));
        assert_eq!(normalize_mode("OnDemand"), None);
        assert_eq!(
            workflow_config("onDemand", "remote"),
            json!({"mode": "onDemand", "enabled": true, "source": "remote"})
        );
        assert_eq!(
            workflow_config("disabled", "default")["enabled"],
            json!(false)
        );
    }

    fn services(tag: &str) -> crate::ProviderServices {
        let root = std::env::temp_dir().join(format!(
            "zcode-runtime-config-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        crate::ProviderServices::new(nc_code_service_core::AppPaths::new(
            root.join("home"),
            root.join("data"),
        ))
    }

    fn ctx() -> ChannelContext {
        ChannelContext::new(
            "runtime-config",
            nc_code_channels::ClientMode::DesktopContinuous,
        )
    }

    #[tokio::test]
    async fn removed_purchase_methods_are_named_errors() {
        let services = services("removed");
        let error = Channel::call(
            services.runtime_config.as_ref(),
            &ctx(),
            "createSign",
            vec![],
            CancelToken::new(),
        )
        .await
        .expect_err("purchase method removed");
        assert!(
            error.to_string().contains("CodingPlanPurchaseRemoved"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn workflow_offpeak_and_force_update_are_local() {
        let services = services("local");
        let workflow = Channel::call(
            services.runtime_config.as_ref(),
            &ctx(),
            "getDynamicWorkflowClientConfig",
            vec![],
            CancelToken::new(),
        )
        .await
        .expect("workflow config");
        let WireValue::Json(workflow) = workflow else {
            panic!("workflow config must be JSON");
        };
        assert_eq!(workflow["mode"], json!("onDemand"));
        assert_eq!(workflow["source"], json!("default"));

        let off_peak = Channel::call(
            services.runtime_config.as_ref(),
            &ctx(),
            "getOffPeakClientConfig",
            vec![],
            CancelToken::new(),
        )
        .await
        .expect("off-peak config");
        let WireValue::Json(off_peak) = off_peak else {
            panic!("off-peak config must be JSON");
        };
        assert_eq!(off_peak["enabled"], json!(false));

        let force_update = Channel::call(
            services.runtime_config.as_ref(),
            &ctx(),
            "getForceUpdateConfig",
            vec![],
            CancelToken::new(),
        )
        .await
        .expect("force update config");
        let WireValue::Json(force_update) = force_update else {
            panic!("force update config must be JSON");
        };
        assert_eq!(force_update, Value::Null);
    }
}
