//! Integration tests for service-core utility modules ported from TS services.

use std::collections::BTreeMap;
use std::sync::Arc;

use nc_code_service_core::bigmodel::{
    classify_team_coding_plan, is_active_personal_coding_plan, CodingPlanEntitlement,
    PersonalCodingPlanSubscription, TeamCodingPlanSubscription,
};
use nc_code_service_core::conversation_telemetry::{
    ConversationTelemetryFact, ConversationTelemetrySink, ConversationTelemetryWorkspaceTarget,
    VecConversationTelemetrySink,
};
use nc_code_service_core::fs_fault_injection::{
    parse_fs_fault_rules_from_env_value, FsFaultCheckInput, FsFaultInjector, FsFaultOperation,
    FsFaultRuleConfig,
};
use nc_code_service_core::logger::{CaptureLogSink, ServiceLogger};
use nc_code_service_core::memory_diagnostics::{self, MemoryDiagnosticsRegistry};
use nc_code_service_core::remote_sync::check_remote_sync_directory_write_access;

#[test]
fn fs_fault_injection_integration() {
    let injector = FsFaultInjector::new(&[FsFaultRuleConfig {
        id: "write-block".into(),
        code: "EPERM".into(),
        operations: vec!["writeFile".into(), "rename".into()],
        path_ends_with: Some(".json".into()),
        max_matches: Some(2),
        ..Default::default()
    }])
    .unwrap();
    injector
        .maybe_throw(&FsFaultCheckInput {
            operation: FsFaultOperation::WriteFile,
            path: "a.json".into(),
        })
        .unwrap_err();
    injector
        .maybe_throw(&FsFaultCheckInput {
            operation: FsFaultOperation::ReadFile,
            path: "a.json".into(),
        })
        .unwrap();
    assert_eq!(injector.get_hits().len(), 1);
    injector.reset();
    assert!(injector.get_hits().is_empty());
}

#[test]
fn fs_fault_env_parse_integration() {
    let rules = parse_fs_fault_rules_from_env_value(
        r#"[{"id":"x","code":"EIO","operations":["readFile"],"pathIncludes":"logs"}]"#,
    )
    .unwrap();
    assert_eq!(rules[0].operations, vec!["readFile"]);
}

#[test]
fn memory_diagnostics_integration() {
    let reg = MemoryDiagnosticsRegistry::new();
    let _h = reg.register(
        "tasks",
        Arc::new(|| BTreeMap::from([("cached".to_string(), 3.0), ("open".to_string(), 1.0)])),
    );
    let got = reg.collect();
    assert_eq!(got.get("tasks.cached"), Some(&3.0));
    assert_eq!(got.get("tasks.open"), Some(&1.0));
    // global helpers are callable
    let _ = memory_diagnostics::collect_service_memory_diagnostics();
}

#[tokio::test]
async fn remote_sync_integration() {
    let tmp = tempfile::TempDir::new().unwrap();
    let r = check_remote_sync_directory_write_access(tmp.path()).await;
    assert!(r.ok);
}

#[test]
fn logger_and_telemetry_integration() {
    let sink = Arc::new(CaptureLogSink::default());
    let logger = ServiceLogger::new("core")
        .with_debug_enabled(true)
        .with_sink(sink.clone());
    logger.warn(Some("tid"), "warn");
    assert_eq!(sink.lines.lock().unwrap().len(), 1);
    let tsink = VecConversationTelemetrySink::default();
    tsink.publish(
        &ConversationTelemetryWorkspaceTarget {
            workspace_path: "w".into(),
            workspace_identity: None,
        },
        ConversationTelemetryFact {
            fields: serde_json::json!({"k":1}),
        },
    );
    assert_eq!(tsink.events.lock().unwrap().len(), 1);
}

#[test]
fn bigmodel_integration() {
    let p = PersonalCodingPlanSubscription {
        product_id: Some("coding".into()),
        status: Some("VALID".into()),
        in_current_period: Some(true),
        ..Default::default()
    };
    assert!(is_active_personal_coding_plan(&p));
    let t = TeamCodingPlanSubscription {
        has_subscription: true,
        status: Some("EFFECTIVE".into()),
        member_grant_status: Some("VALID".into()),
        ..Default::default()
    };
    assert!(matches!(
        classify_team_coding_plan(&t),
        CodingPlanEntitlement::Available { .. }
    ));
}
