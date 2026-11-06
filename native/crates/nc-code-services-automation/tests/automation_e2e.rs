//! automation warehouse/service e2e: compatible with TS built by **real sqlite** (nc-code-host::storage::open
//! schema) run create/list/update/delete/runNow/claim/settle state machine. Not a memory mock——
//! Each assertion verifies cross-call persistence and atomic state migration on the disk library.

use std::sync::Arc;

use nc_code_services_automation::repo::AutomationRepo;
use nc_code_services_automation::types::ScheduleUnit;
use nc_code_services_automation::types::{LifecycleStatus, Trigger};
use nc_code_services_automation::{AutomationService, CreateInput, Scope, UpdateInput};

fn temp_db() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zcode-auto-e2e-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir.join("tasks-index.sqlite")
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn make_service(db: &std::path::Path) -> AutomationService {
    AutomationService::new(Arc::new(AutomationRepo::new(db)))
}

fn scope() -> Scope {
    Scope {
        workspace_path: Some("/ws/alpha".to_string()),
        workspace_identity: None,
    }
}

fn create_input(title: &str) -> CreateInput {
    CreateInput {
        title: title.to_string(),
        cron_expr: "30 9 * * *".to_string(),
        prompt: "run the thing".to_string(),
        workspace_path: "/ws/alpha".to_string(),
        recurring: true,
        ..Default::default()
    }
}

fn create_daily(
    service: &AutomationService,
    title: &str,
) -> nc_code_services_automation::Automation {
    service
        .create(&create_input(title), &scope())
        .expect("create")
}

#[test]
fn create_persists_and_round_trips_fields() {
    let db = temp_db();
    let service = make_service(&db);
    let created = create_daily(&service, "daily report");
    assert_eq!(created.title, "daily report");
    assert_eq!(created.cron_expr, "30 9 * * *");
    assert!(created.recurring);
    assert!(created.enabled);
    assert_eq!(created.lifecycle_status, LifecycleStatus::Active);
    assert_eq!(created.workspace_key, "/ws/alpha");
    assert!(created.automation_id.starts_with("automation-"));
    assert!(
        created.next_run_at.is_some_and(|next| next > now()),
        "nextRunAt must be in the future"
    );
    let service2 = make_service(&db);
    let fetched = service2
        .get(&created.automation_id, &scope())
        .expect("get")
        .expect("exists");
    assert_eq!(
        fetched, created,
        "Disk read back must be consistent with write"
    );
}

#[test]
fn create_enforces_the_20_limit() {
    let db = temp_db();
    let service = make_service(&db);
    for index in 0..20 {
        create_daily(&service, &format!("auto {index}"));
    }
    let err = service
        .create(&create_input("overflow"), &scope())
        .unwrap_err();
    assert!(err.to_string().contains("AUTOMATION_CREATE_LIMIT_REACHED"));
    assert_eq!(service.list(&scope()).expect("list").len(), 20);
}

#[test]
fn delete_and_workspace_isolation() {
    let db = temp_db();
    let service = make_service(&db);
    let created = create_daily(&service, "mine");
    let other = Scope {
        workspace_path: Some("/ws/beta".to_string()),
        workspace_identity: None,
    };
    assert!(service
        .get(&created.automation_id, &other)
        .expect("get")
        .is_none());
    assert!(service
        .delete(&created.automation_id, &scope())
        .expect("delete"));
    assert!(service
        .get(&created.automation_id, &scope())
        .expect("get")
        .is_none());
    assert!(!service
        .delete(&created.automation_id, &scope())
        .expect("delete again"));
}

#[test]
fn set_enabled_toggles_lifecycle_and_keeps_schedule() {
    let db = temp_db();
    let service = make_service(&db);
    let created = create_daily(&service, "pause me");
    let next = created.next_run_at;
    service
        .set_enabled(&created.automation_id, false, &scope())
        .expect("pause");
    let paused = service
        .get(&created.automation_id, &scope())
        .expect("get")
        .expect("exists");
    assert!(!paused.enabled);
    assert_eq!(paused.lifecycle_status, LifecycleStatus::Paused);
    assert_eq!(paused.next_run_at, next, "paused and reserved nextRunAt");
    service
        .set_enabled(&created.automation_id, true, &scope())
        .expect("resume");
    let resumed = service
        .get(&created.automation_id, &scope())
        .expect("get")
        .expect("exists");
    assert!(resumed.enabled);
    assert_eq!(resumed.lifecycle_status, LifecycleStatus::Active);
}

#[test]
fn run_now_creates_manual_run_and_claims_single_flight() {
    let db = temp_db();
    let service = make_service(&db);
    let created = create_daily(&service, "run now");
    let (automation, run) = service
        .run_now(&created.automation_id, &scope())
        .expect("run_now")
        .expect("claimed");
    assert_eq!(run.trigger, Trigger::Manual);
    assert!(run.run_id.contains(":manual:"));
    assert_eq!(run.attempts, 1);
    assert_eq!(automation.automation_id, created.automation_id);
    let second = service
        .run_now(&created.automation_id, &scope())
        .expect("run_now again");
    assert!(
        second.is_none(),
        "Repeat claims are not allowed when running=1"
    );
    let runs = service
        .list_runs(&created.automation_id, &scope())
        .expect("list_runs");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].run_id, run.run_id);
}

#[test]
fn update_changes_title_and_prompt() {
    let db = temp_db();
    let service = make_service(&db);
    let created = create_daily(&service, "before");
    let updated = service
        .update(
            &created.automation_id,
            &UpdateInput {
                title: Some("after".to_string()),
                prompt: Some("new prompt".to_string()),
                ..Default::default()
            },
            &scope(),
        )
        .expect("update")
        .expect("exists");
    assert_eq!(updated.title, "after");
    assert_eq!(updated.prompt, "new prompt");
    assert_eq!(
        updated.cron_expr, created.cron_expr,
        "Unmodified cron retains the original value"
    );
}

#[test]
fn update_cron_change_recomputes_next_run() {
    let db = temp_db();
    let service = make_service(&db);
    let created = create_daily(&service, "re cron");
    let updated = service
        .update(
            &created.automation_id,
            &UpdateInput {
                cron_expr: Some("0 12 * * *".to_string()),
                ..Default::default()
            },
            &scope(),
        )
        .expect("update")
        .expect("exists");
    assert_eq!(updated.cron_expr, "0 12 * * *");
    assert!(updated.next_run_at.is_some_and(|next| next > now()));
}

#[test]
fn update_rejects_invalid_carrier_pairing() {
    let db = temp_db();
    let service = make_service(&db);
    let created = create_daily(&service, "bad carrier");
    let err = service
        .update(
            &created.automation_id,
            &UpdateInput {
                interval_unit: Some(ScheduleUnit::Daily),
                ..Default::default()
            },
            &scope(),
        )
        .unwrap_err();
    assert!(err
        .to_string()
        .contains("Submit at the same time or omit at the same time"));
}

#[test]
fn relative_delay_builds_one_shot_and_rejects_recurring() {
    let db = temp_db();
    let service = make_service(&db);
    let created = service
        .create(
            &CreateInput {
                title: "delay".to_string(),
                cron_expr: "* * * * *".to_string(),
                prompt: "p".to_string(),
                workspace_path: "/ws/alpha".to_string(),
                relative_delay_minutes: Some(5),
                recurring: false,
                ..Default::default()
            },
            &scope(),
        )
        .expect("create");
    assert!(!created.recurring, "relative delay forced one-time");
    assert!(
        created.schedule_rule.is_some(),
        "relative delay completion scheduleRule"
    );
    let err = service
        .create(
            &CreateInput {
                relative_delay_minutes: Some(5),
                recurring: true,
                ..create_input("x")
            },
            &scope(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("recurring=false"));
}
