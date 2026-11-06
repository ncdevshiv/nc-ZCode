//! Disabled `off-peak-task` channel.
//!
//! Off-peak tasks are executed by the **zcode cloud scheduler**: creation takes a ticket
//! from the server (`POST /ticket`), progress comes from batch status polling, and terminal
//! tasks are settled back to the server. This build keeps no zcode cloud connection other
//! than auth/token claim and account/model setup (`specs/disabled-features.md`), so the
//! channel is registered in a disabled form:
//!
//! - `getCodingPlanSupport` → `{ supported: false, reason: "feature_disabled" }`, which is
//!   what the UI uses to hide the off-peak surfaces;
//! - `getTakeNumberAvailability` → `{ canTakeNumber: false }`;
//! - `list` → `[]` (no task can ever be created);
//! - `createTask` → the documented failure union (`errorCode: "offpeak_feature_disabled"`);
//! - everything else → a named `OffPeakTaskDisabled` error.
//!
//! The domain layer (repo/state machine/client/service, all tested) stays in this crate as
//! the reference implementation; it is not reachable while the product decision stands.

use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use serde_json::json;

pub const DISABLED_ERROR: &str = "Off-peak task scheduling is disabled in this build: it needs the zcode cloud scheduler, and this product only connects to zcode for sign-in, the free-credit claim, and model access.";
pub const DISABLED_ERROR_CODE: &str = "offpeak_feature_disabled";

pub struct DisabledOffPeakTaskService;

impl DisabledOffPeakTaskService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl Default for DisabledOffPeakTaskService {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl Channel for DisabledOffPeakTaskService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        _args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        match command {
            "getCodingPlanSupport" => Ok(WireValue::Json(json!({
                "supported": false,
                "reason": "feature_disabled",
            }))),
            "getTakeNumberAvailability" => Ok(WireValue::Json(json!({
                "canTakeNumber": false,
            }))),
            "list" => Ok(WireValue::Json(json!([]))),
            "createTask" => Ok(WireValue::Json(json!({
                "ok": false,
                "failureStage": "ticket_request",
                "errorCategory": "unknown",
                "errorCode": DISABLED_ERROR_CODE,
                "providerName": "",
            }))),
            "get" | "updateTask" | "pauseTask" | "continueTask" | "cancelTask" | "deleteTask"
            | "deleteHistory" => Err(ChannelFailure::error_named(
                "OffPeakTaskDisabled",
                DISABLED_ERROR,
            )),
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;

    fn ctx() -> ChannelContext {
        ChannelContext::new("offpeak-disabled", ClientMode::DesktopContinuous)
    }

    #[tokio::test]
    async fn support_and_availability_expose_the_disabled_state() {
        let service = DisabledOffPeakTaskService::new();
        let support = service
            .call(&ctx(), "getCodingPlanSupport", vec![], CancelToken::new())
            .await
            .expect("support");
        let WireValue::Json(support) = support else {
            panic!("support must be JSON");
        };
        assert_eq!(support["supported"], json!(false));
        assert_eq!(support["reason"], json!("feature_disabled"));

        let availability = service
            .call(
                &ctx(),
                "getTakeNumberAvailability",
                vec![],
                CancelToken::new(),
            )
            .await
            .expect("availability");
        let WireValue::Json(availability) = availability else {
            panic!("availability must be JSON");
        };
        assert_eq!(availability["canTakeNumber"], json!(false));
    }

    #[tokio::test]
    async fn list_is_empty_and_create_reports_the_disabled_failure() {
        let service = DisabledOffPeakTaskService::new();
        let list = service
            .call(&ctx(), "list", vec![], CancelToken::new())
            .await
            .expect("list");
        let WireValue::Json(list) = list else {
            panic!("list must be JSON");
        };
        assert_eq!(list, json!([]));

        let created = service
            .call(&ctx(), "createTask", vec![], CancelToken::new())
            .await
            .expect("create result");
        let WireValue::Json(created) = created else {
            panic!("create must be JSON");
        };
        assert_eq!(created["ok"], json!(false));
        assert_eq!(created["errorCode"], json!(DISABLED_ERROR_CODE));
    }

    #[tokio::test]
    async fn task_mutations_fail_with_the_disabled_error() {
        let service = DisabledOffPeakTaskService::new();
        for member in [
            "get",
            "updateTask",
            "pauseTask",
            "continueTask",
            "cancelTask",
            "deleteTask",
            "deleteHistory",
        ] {
            let error = service
                .call(&ctx(), member, vec![], CancelToken::new())
                .await
                .expect_err(member);
            assert!(error.to_string().contains(DISABLED_ERROR), "{member}");
        }
    }

    #[tokio::test]
    async fn unknown_members_are_method_not_found() {
        let service = DisabledOffPeakTaskService::new();
        let error = service
            .call(&ctx(), "bogus", vec![], CancelToken::new())
            .await
            .expect_err("unknown");
        assert!(error.to_string().contains("Method not found"));
    }
}
