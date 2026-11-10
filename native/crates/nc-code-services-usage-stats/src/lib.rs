//! `usage-stats` channel — contract-aligned with `IUsageStatsService`.
//!
//! Local-only, honest behavior (ZCode cloud usage/stat surfaces not connected here):
//! - `getAppUsageSnapshot`: aggregates the **durable** per-turn usage facts in the
//!   local agent transcripts (`durable_app_usage`); nothing is fabricated.
//! - `getSnapshot` / `getEntitlementSnapshot`: return contract-shaped defaults.
//! - Coding-plan usage/reset methods: fail with `no_bigmodel_api_key` (honest, matches
//!   TS service behavior when no BigModel key is available).

/// Handoff skeleton (pure data/types): serde persistence structs, port trait, Send/Sync markers.
pub mod handoff;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::{build_app_usage_snapshot, scan_agent_usage, AppPaths, ServiceArgs};
use serde_json::{json, Value};

pub struct UsageStatsService {
    /// Root of the per-workspace agent transcripts (`<app-config>/agent`). The
    /// `v4/usage/stats` facts are read from there — the same append-only transcripts
    /// the host writes, so no second database exists.
    agent_root: std::path::PathBuf,
}

impl UsageStatsService {
    pub fn new() -> Arc<Self> {
        let paths = AppPaths::from_env();
        Self::with_agent_root(paths.app_config_dir().join("agent"))
    }

    pub fn with_agent_root(agent_root: impl Into<std::path::PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            agent_root: agent_root.into(),
        })
    }

    /// Range window in ms (TS `APP_USAGE_RANGES`: all = no lower bound).
    fn range_since(range: &str, now_ms: u64) -> Option<u64> {
        let days = match range {
            "7d" => 7,
            "30d" => 30,
            _ => return None,
        };
        Some(now_ms.saturating_sub(days as u64 * 86_400_000))
    }

    /// The durable app-usage snapshot: real per-turn facts aggregated over the range.
    fn durable_app_usage(&self, request: &Value) -> Value {
        let range = request
            .get("range")
            .and_then(Value::as_str)
            .filter(|r| matches!(*r, "all" | "7d" | "30d"))
            .unwrap_or("all");
        let time_zone = request
            .get("timeZone")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("UTC");
        let now = Self::now_ms();
        let scan = scan_agent_usage(&self.agent_root);
        let mut snapshot = build_app_usage_snapshot(
            &scan.facts,
            range,
            time_zone,
            Self::range_since(range, now).unwrap_or(0) as i64,
            now as i64,
        );
        if scan.skipped_files > 0 || scan.skipped_lines > 0 {
            // Unreadable/malformed transcripts are counted in the service log: a
            // missing fact must be visible, never silently folded into zero usage.
            nc_code_service_core::logger::ServiceLogger::new("usage-stats").warn(
                None,
                format!(
                    "usage scan skipped {} file(s) / {} line(s) under {}",
                    scan.skipped_files,
                    scan.skipped_lines,
                    self.agent_root.display()
                ),
            );
        }
        if let Some(object) = snapshot.as_object_mut() {
            object.insert("scannedTranscripts".to_string(), json!(scan.transcripts));
        }
        snapshot
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
        let paths = AppPaths::from_env();
        Self {
            agent_root: paths.app_config_dir().join("agent"),
        }
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
            "getAppUsageSnapshot" => self.durable_app_usage(&request),
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

    /// A local agent root with one real transcript carrying a usage fact.
    fn seeded_agent_root(tag: &str, now_ms: i64) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("zcode-usage-stats-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sessions = root.join("ws").join("sessions").join("s1");
        std::fs::create_dir_all(&sessions).expect("session dir");
        let fact = json!({
            "sessionId": "s1",
            "modelId": "deepseek-v4.1-flash-eco",
            "inputTokens": 120,
            "outputTokens": 60,
            "toolCallCount": 1,
            "toolErrorCount": 0,
            "startedAt": now_ms - 1000,
            "endedAt": now_ms,
            "durationMs": 1000,
            "modelRequestCount": 2,
            "tools": [{"name": "Bash", "error": false, "durationMs": 300}],
        });
        std::fs::write(
            sessions.join("transcript.jsonl"),
            format!("{}\n", json!({"t": "usage", "fact": fact})),
        )
        .expect("transcript");
        root
    }

    #[tokio::test]
    async fn get_app_usage_returns_contract_shape() {
        let now = UsageStatsService::now_ms() as i64;
        let root = seeded_agent_root("shape", now);
        let s = UsageStatsService::with_agent_root(root.clone());
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
        // The durable fact is aggregated, not zeroed: 120 + 60 tokens, one tool call.
        assert_eq!(v["summary"]["totalTokens"], json!(180), "{v}");
        assert_eq!(v["summary"]["inputTokens"], json!(120), "{v}");
        assert_eq!(v["summary"]["totalSessions"], json!(1), "{v}");
        assert_eq!(v["summary"]["toolCallCount"], json!(1), "{v}");
        assert_eq!(
            v["summary"]["favoriteModel"]["modelId"],
            json!("deepseek-v4.1-flash-eco")
        );
        assert!(v["heatmap"]["weeks"].is_array());
        assert!(v["dailyModelUsage"].is_array());
        assert!(v["models"].is_array());
        assert!(v["tools"].is_array());
        assert_eq!(v["scannedTranscripts"], json!(1), "{v}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn an_empty_agent_root_reports_zero_usage_without_fabricating() {
        let root = std::env::temp_dir().join(format!("zcode-usage-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let s = UsageStatsService::with_agent_root(root.clone());
        let r = s
            .call(
                &ctx(),
                "getAppUsageSnapshot",
                vec![WireValue::Json(json!({"range": "all", "timeZone": "UTC"}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = r else {
            panic!("json")
        };
        assert_eq!(v["summary"]["totalTokens"], json!(0), "{v}");
        assert_eq!(v["summary"]["totalSessions"], json!(0), "{v}");
        assert!(v["summary"]["favoriteModel"].is_null(), "{v}");
        assert_eq!(v["scannedTranscripts"], json!(0), "{v}");
        let _ = std::fs::remove_dir_all(&root);
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
