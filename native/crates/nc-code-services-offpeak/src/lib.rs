//! Off-peak task service disabled (ZCode services removed).

/// Handoff skeleton (pure data/types): serde persistence structs, port trait, Send/Sync markers.
pub mod handoff;

pub mod disabled;

pub use disabled::DisabledOffPeakTaskService;

#[cfg(test)]
mod tests {
    /// Off-peak scheduling is disabled by product decision: the capability probe must
    /// say so explicitly (the UI hides the surfaces from this answer).
    #[tokio::test]
    async fn disabled_surface_reports_the_product_decision() {
        use nc_code_channels::{CancelToken, Channel, ChannelContext, ClientMode};
        use nc_code_rpc::WireValue;
        let service = super::DisabledOffPeakTaskService::new();
        let ctx = ChannelContext::new("offpeak", ClientMode::DesktopContinuous);
        let support = service
            .call(&ctx, "getCodingPlanSupport", vec![], CancelToken::new())
            .await
            .expect("support");
        let WireValue::Json(value) = support else {
            panic!("support JSON");
        };
        assert_eq!(value["supported"], serde_json::json!(false));
        assert_eq!(value["reason"], serde_json::json!("feature_disabled"));
        let create = service
            .call(
                &ctx,
                "createTask",
                vec![WireValue::Json(serde_json::json!({"objective": "x"}))],
                CancelToken::new(),
            )
            .await
            .expect("create failure union");
        let WireValue::Json(create) = create else {
            panic!("create JSON");
        };
        assert_eq!(
            create["errorCode"],
            serde_json::json!(super::disabled::DISABLED_ERROR_CODE)
        );
    }
}
