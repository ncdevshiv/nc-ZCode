//! `usage-stats` channel — contract-aligned with `IUsageStatsService`.
//!
//! Local-only, honest behavior (ZCode cloud usage/stat surfaces not connected here):
//! - `getAppUsageSnapshot`: returns a contract-shaped `AppUsageSnapshot` with zero/empty
//!   data (no fabricated usage). Range/timeZone echoed from request where valid.
//! - `getSnapshot` / `getEntitlementSnapshot`: return contract-shaped defaults.
//! - Coding-plan usage/reset methods: fail with `no_bigmodel_api_key` (honest, matches
//!   TS service behavior when no BigModel key is available).

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::ServiceArgs;
use serde_json::{json, Value};

pub struct UsageStatsService;

impl UsageStatsService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    fn no_bigmodel_api_key() -> ChannelFailure {
        ChannelFailure::error("no_bigmodel_api_key")
    }

    fn app_usage_snapshot(request: &Value) -> Value {
        let range = request
            .get("range")
            .and_then(|v| v.as_str())
            .filter(|r| matches!(*r, "all" | "7d" | "30d"))
            .unwrap_or("all");
        let time_zone = request
            .get("timeZone")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("UTC");
        json!({
            "range": range,
            "generatedAt": Self::now_ms(),
            "timeZone": time_zone,
            "source": "agent-db",
            "summary": {
                "totalTokens": 0,
                "inputTokens": 0,
                "outputTokens": 0,
                "reasoningTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 0,
                "cacheHitRate": 0.0,
                "totalSessions": 0,
                "totalTurns": 0,
                "toolCallCount": 0,
                "toolErrorRate": 0.0,
                "modelErrorRate": 0.0,
                "activeDays": 0,
                "currentStreakDays": 0,
                "longestSessionMs": 0,
                "longestStreakDays": 0,
                "peakDayTokens": 0,
                "avgTimeToFirstTokenMs": Value::Null,
                "avgTurnDurationMs": Value::Null,
                "favoriteModel": Value::Null
            },
            "heatmap": {
                "maxTokens": 0,
                "weeks": [],
                "startDate": Value::Null,
                "endDate": Value::Null
            },
            "dailyModelUsage": [],
            "models": [],
            "tools": []
        })
    }

    fn usage_stats_snapshot(request: &Value) -> Value {
        let range = request
            .get("range")
            .and_then(|v| v.as_str())
            .filter(|r| matches!(*r, "all" | "7d" | "30d"))
            .unwrap_or("all");
        let time_zone = request
            .get("timeZone")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("UTC");
        json!({
            "range": range,
            "generatedAt": Self::now_ms(),
            "timeZone": time_zone,
            "estimatedTokenCharDivisor": 3,
            "summary": {
                "totalSessions": 0,
                "totalMessages": 0,
                "totalCharacters": 0,
                "totalEstimatedTokens": 0,
                "activeDays": 0,
                "mostActiveDay": Value::Null,
                "favoriteModel": Value::Null,
                "longestSessionMs": 0,
                "longestStreakDays": 0,
                "currentStreakDays": 0,
                "firstActivityDate": Value::Null,
                "lastActivityDate": Value::Null,
                "peakHour": Value::Null
            },
            "daily": [],
            "heatmap": {
                "startDate": Value::Null,
                "endDate": Value::Null,
                "maxActivityScore": 0,
                "weeks": [],
                "monthLabels": []
            },
            "models": [],
            "source": "local"
        })
    }

    fn entitlement_snapshot() -> Value {
        json!({
            "generatedAt": Self::now_ms(),
            "authenticated": false,
            "unavailableReason": "not_configured",
            "context": Value::Null,
            "provider": Value::Null,
            "remaining": Value::Null,
            "subscription": Value::Null,
            "quota": Value::Null,
            "mcpQuota": Value::Null
        })
    }
}

impl Default for UsageStatsService {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl Channel for UsageStatsService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        let request = args
            .required_json(0, "request")
            .unwrap_or_else(|_| json!({}));
        let result = match command {
            "getAppUsageSnapshot" => Self::app_usage_snapshot(&request),
            "getSnapshot" => Self::usage_stats_snapshot(&request),
            "getEntitlementSnapshot" => Self::entitlement_snapshot(),
            "getCodingPlanUsageSnapshot"
            | "getCodingPlanResetStatus"
            | "requestCodingPlanResetOpportunity"
            | "useCodingPlanReset"
            | "markCodingPlanResetHistoryRead" => return Err(Self::no_bigmodel_api_key()),
            other => return Err(ChannelFailure::method_not_found(other)),
        };
        Ok(WireValue::Json(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;

    fn ctx() -> ChannelContext {
        ChannelContext::new("usage-stats", ClientMode::DesktopContinuous)
    }

    #[tokio::test]
    async fn get_app_usage_returns_contract_shape() {
        let s = UsageStatsService::new();
        let r = s
            .call(
                &ctx(),
                "getAppUsageSnapshot",
                vec![WireValue::Json(
                    json!({"range": "7d", "timeZone": "Asia/Kolkata"}),
                )],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = r else {
            panic!("json")
        };
        assert_eq!(v["range"], "7d");
        assert_eq!(v["timeZone"], "Asia/Kolkata");
        assert_eq!(v["source"], "agent-db");
        assert!(v["generatedAt"].is_number());
        assert_eq!(v["summary"]["totalTokens"], 0);
        assert_eq!(v["summary"]["cacheHitRate"], 0.0);
        assert!(v["summary"]["favoriteModel"].is_null());
        assert!(v["heatmap"]["weeks"].is_array());
        assert!(v["dailyModelUsage"].is_array());
        assert!(v["models"].is_array());
        assert!(v["tools"].is_array());
    }

    #[tokio::test]
    async fn get_snapshot_returns_contract_shape() {
        let s = UsageStatsService::new();
        let r = s
            .call(
                &ctx(),
                "getSnapshot",
                vec![WireValue::Json(json!({"range": "30d"}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = r else {
            panic!("json")
        };
        assert_eq!(v["range"], "30d");
        assert_eq!(v["estimatedTokenCharDivisor"], 3);
        assert_eq!(v["source"], "local");
        assert!(v["summary"]["mostActiveDay"].is_null());
        assert!(v["daily"].is_array());
        assert!(v["heatmap"]["monthLabels"].is_array());
        assert!(v["models"].is_array());
    }

    #[tokio::test]
    async fn get_entitlement_snapshot_returns_honest_defaults() {
        let s = UsageStatsService::new();
        let r = s
            .call(
                &ctx(),
                "getEntitlementSnapshot",
                vec![WireValue::Json(json!({}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = r else {
            panic!("json")
        };
        assert_eq!(v["authenticated"], false);
        assert_eq!(v["unavailableReason"], "not_configured");
        assert!(v["provider"].is_null());
        assert!(v["quota"].is_null());
    }

    #[tokio::test]
    async fn coding_plan_methods_fail_with_no_bigmodel_api_key() {
        let s = UsageStatsService::new();
        for cmd in [
            "getCodingPlanUsageSnapshot",
            "getCodingPlanResetStatus",
            "requestCodingPlanResetOpportunity",
            "useCodingPlanReset",
            "markCodingPlanResetHistoryRead",
        ] {
            let err = s
                .call(
                    &ctx(),
                    cmd,
                    vec![WireValue::Json(json!({}))],
                    CancelToken::new(),
                )
                .await
                .expect_err(cmd);
            assert!(
                err.to_string().contains("no_bigmodel_api_key"),
                "{cmd}: {err}"
            );
        }
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use nc_code_channels::ClientMode;

    #[tokio::test]
    async fn unknown_method_fails_closed() {
        let s = UsageStatsService::new();
        let ctx = ChannelContext::new("usage-stats", ClientMode::DesktopContinuous);
        let err = s
            .call(&ctx, "nope", vec![], CancelToken::new())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Method not found"));
    }
}
