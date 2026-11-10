//! Independent verification: acceptance criteria are executed in the task worktree by
//! the verifier, never taken from the author claim (spec section 6).

use std::path::{Path, PathBuf};

use serde_json::json;

use crate::db::now_ms;
use crate::error::Result;
use crate::events::EventHub;
use crate::types::{AcceptanceCheck, AcceptanceKind, Evidence, EvidenceKind, ProjectConfig, Task};
use crate::Store;

#[derive(Debug, Clone)]
pub struct CriterionResult {
    pub check_id: String,
    pub description: String,
    /// The evidence kind this criterion produces. It is recorded verbatim: a
    /// `FileExists` check must never be stored as a `Review` row, or a pre-review
    /// "the file exists" pass would read as an independent-review approval.
    pub kind: EvidenceKind,
    pub passed: bool,
    /// A criterion that cannot be judged yet (review/audit evidence before the review
    /// happens). Pending criteria are not successes and not failures: they defer the
    /// task to the review stage.
    pub pending: bool,
    pub detail: String,
    pub command: Option<String>,
}

/// Which pass is running. Pre-review judges only what can be judged mechanically;
/// post-review additionally requires the independent review/audit evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationPhase {
    PreReview,
    PostReview,
}

#[derive(Debug, Clone)]
pub struct VerificationReport {
    pub passed: bool,
    pub criteria: Vec<CriterionResult>,
    pub failed: Vec<String>,
}

/// Run every criterion that can be executed mechanically. Review/audit criteria are
/// pending in the pre-review pass and resolved by the review/audit evidence path.
pub fn verify_locally(
    workspace: &Path,
    task: &Task,
    config: &ProjectConfig,
    review_evidence_ok: bool,
    phase: VerificationPhase,
) -> VerificationReport {
    let mut criteria = Vec::new();
    for check in &task.acceptance {
        let result = match check.kind {
            AcceptanceKind::CommandSucceeded => {
                run_check_command(workspace, check, config, false, EvidenceKind::Command)
            }
            AcceptanceKind::TestPassed => {
                run_check_command(workspace, check, config, false, EvidenceKind::Test)
            }
            AcceptanceKind::FileExists => check_file_exists(workspace, check),
            AcceptanceKind::FileContains => check_file_contains(workspace, check),
            AcceptanceKind::OutputContains => {
                run_check_command(workspace, check, config, true, EvidenceKind::OutputContains)
            }
            AcceptanceKind::ReviewApproved => CriterionResult {
                check_id: check.id.clone(),
                description: check.description.clone(),
                kind: EvidenceKind::Review,
                passed: review_evidence_ok,
                pending: phase == VerificationPhase::PreReview,
                detail: if review_evidence_ok {
                    "independent review evidence present".to_string()
                } else if phase == VerificationPhase::PreReview {
                    "deferred to the independent review stage".to_string()
                } else {
                    "independent review evidence missing".to_string()
                },
                command: None,
            },
            AcceptanceKind::AuditClean => CriterionResult {
                check_id: check.id.clone(),
                description: check.description.clone(),
                kind: EvidenceKind::Audit,
                passed: review_evidence_ok,
                pending: phase == VerificationPhase::PreReview,
                detail: if phase == VerificationPhase::PreReview {
                    "deferred to the security audit stage".to_string()
                } else if review_evidence_ok {
                    "audit evidence present".to_string()
                } else {
                    "audit evidence missing".to_string()
                },
                command: None,
            },
        };
        criteria.push(result);
    }
    let mut failed: Vec<String> = criteria
        .iter()
        .filter(|item| !item.passed && !item.pending)
        .map(|item| format!("{}: {}", item.check_id, item.description))
        .collect();
    // A task with nothing to judge is not verified. Zero criteria used to report success,
    // which made "verification.passed" meaningless (observed live: criteria: 0).
    let judged = criteria.iter().filter(|item| !item.pending).count();
    let any_criteria = !criteria.is_empty();
    // An empty list is never verified; a list whose only criteria are pending is
    // allowed pre-review (the review stage will judge them) and fails post-review.
    if !any_criteria || (judged == 0 && phase == VerificationPhase::PostReview) {
        failed.push("no acceptance criteria were defined for this task".to_string());
    }
    VerificationReport {
        passed: failed.is_empty(),
        criteria,
        failed,
    }
}

fn command_allowed(command: &str, config: &ProjectConfig) -> bool {
    let lowercase = command.trim().to_ascii_lowercase();
    config.allowed_commands.iter().any(|allowed| {
        lowercase == allowed.to_ascii_lowercase()
            || lowercase.starts_with(&format!("{} ", allowed.to_ascii_lowercase()))
    })
}

fn run_check_command(
    workspace: &Path,
    check: &AcceptanceCheck,
    config: &ProjectConfig,
    expect_output: bool,
    kind: EvidenceKind,
) -> CriterionResult {
    let Some(command) = check.command.clone() else {
        return CriterionResult {
            check_id: check.id.clone(),
            description: check.description.clone(),
            kind,
            passed: false,
            pending: false,
            detail: "criterion has no command".to_string(),
            command: None,
        };
    };
    if !command_allowed(&command, config) {
        return CriterionResult {
            check_id: check.id.clone(),
            description: check.description.clone(),
            kind,
            passed: false,
            pending: false,
            detail: format!("command is not in the project allowlist: {command}"),
            command: Some(command),
        };
    }
    let mut process = crate::tools::shell_command(&command);
    process.current_dir(workspace);
    process
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let output = match process.output() {
        Ok(output) => output,
        Err(error) => {
            return CriterionResult {
                check_id: check.id.clone(),
                description: check.description.clone(),
                kind,
                passed: false,
                pending: false,
                detail: format!("command could not run: {error}"),
                command: Some(command),
            }
        }
    };
    let stdout = crate::tools::decode_output(&output.stdout);
    let stderr = crate::tools::decode_output(&output.stderr);
    let combined = format!("{stdout}\n{stderr}");
    let exit_ok = output.status.success();
    let output_ok = if expect_output {
        check
            .expect
            .as_deref()
            .map(|expect| combined.contains(expect))
            .unwrap_or(true)
    } else {
        true
    };
    let passed = exit_ok && output_ok;
    CriterionResult {
        check_id: check.id.clone(),
        description: check.description.clone(),
        kind,
        passed,
        pending: false,
        detail: format!(
            "exit={} {}{}",
            output.status.code().unwrap_or(-1),
            if output_ok {
                ""
            } else {
                "expected output not found; "
            },
            crate::tools::truncate_text(&combined, 800)
        ),
        command: Some(command),
    }
}

fn check_file_exists(workspace: &Path, check: &AcceptanceCheck) -> CriterionResult {
    let target = check.target.clone().unwrap_or_default();
    let path = workspace.join(&target);
    let exists = path.is_file();
    CriterionResult {
        check_id: check.id.clone(),
        description: check.description.clone(),
        kind: EvidenceKind::FileExists,
        passed: exists,
        pending: false,
        detail: if exists {
            format!("{target} exists")
        } else {
            format!("{target} is missing in the worktree")
        },
        command: None,
    }
}

fn check_file_contains(workspace: &Path, check: &AcceptanceCheck) -> CriterionResult {
    let target = check.target.clone().unwrap_or_default();
    let expect = check.expect.clone().unwrap_or_default();
    let path = workspace.join(&target);
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let passed = !expect.is_empty() && content.contains(&expect);
    CriterionResult {
        check_id: check.id.clone(),
        description: check.description.clone(),
        kind: EvidenceKind::FileContains,
        passed,
        pending: false,
        detail: if passed {
            format!("{target} contains the expected text")
        } else if !path.exists() {
            format!("{target} is missing")
        } else {
            format!("{target} does not contain the expected text")
        },
        command: None,
    }
}

/// Persist each judged criterion outcome as evidence; failed checks stay visible forever.
pub fn record_evidence(
    store: &Store,
    hub: &EventHub,
    project_id: &str,
    task: &Task,
    report: &VerificationReport,
    produced_by: &str,
) -> Result<()> {
    for criterion in report.criteria.iter().filter(|item| !item.pending) {
        let kind = criterion.kind;
        let evidence = Evidence {
            id: uuid::Uuid::new_v4().to_string(),
            project_id: project_id.to_string(),
            task_id: task.id.clone(),
            kind,
            criterion: criterion.check_id.clone(),
            command: criterion.command.clone(),
            result: if criterion.passed { "passed" } else { "failed" }.to_string(),
            detail: crate::security::redact(&criterion.detail),
            artifact: task.worktree.clone(),
            produced_by: produced_by.to_string(),
            created_at: now_ms(),
        };
        store.insert_evidence(&evidence)?;
    }
    let payload = json!({
        "taskId": task.id,
        "passed": report.passed,
        "failed": report.failed,
        "criteria": report.criteria.len(),
    });
    crate::events::emit(
        store,
        hub,
        Some(project_id),
        Some(&task.id),
        None,
        if report.passed {
            "verification.passed"
        } else {
            "verification.failed"
        },
        payload,
    )?;
    Ok(())
}

/// The evidence that is **currently** true: the latest row per `(kind, criterion)`.
///
/// Evidence is append-only on purpose (each repair cycle and each verification phase
/// appends its own rows), so "any row failed" is not the task's state — a review that
/// failed in cycle 1 and passed after the repair is a green task, and refusing its
/// merge over the historical failure blocked exactly that (observed live).
pub fn current_evidence(store: &Store, task_id: &str) -> Result<Vec<Evidence>> {
    let rows = store.evidence_for_task(task_id)?;
    let mut latest: std::collections::BTreeMap<(EvidenceKind, String), Evidence> =
        std::collections::BTreeMap::new();
    // `evidence_for_task` is ordered by created_at ASC, so the last write wins.
    for row in rows {
        latest.insert((row.kind, row.criterion.clone()), row);
    }
    Ok(latest.into_values().collect())
}

/// Did a task's **current** verdict approve it?
///
/// A rejected verdict is superseded by the later approved one of the repair loop; only
/// the latest review rows count.
pub fn has_passing_review(store: &Store, task_id: &str) -> Result<bool> {
    let current = current_evidence(store, task_id)?;
    Ok(current
        .iter()
        .any(|row| row.kind == EvidenceKind::Review && row.result == "passed"))
}

/// Collect the criteria still failing (drives the repair loop). Only the current
/// verdict per criterion is a failure; superseded rows are history, not state.
pub fn failing(store: &Store, task_id: &str) -> Result<Vec<String>> {
    let mut failures: Vec<String> = current_evidence(store, task_id)?
        .iter()
        .filter(|row| row.result == "failed")
        .map(|row| format!("{}: {}", row.criterion, row.detail))
        .collect();
    failures.sort();
    failures.dedup();
    Ok(failures)
}

/// Files a task actually changed (from the worktree) - used for integration overlap.
pub fn worktree_root(task: &Task) -> Option<PathBuf> {
    task.worktree.as_ref().map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EvidenceKind, TaskStatus};

    fn task_with(checks: Vec<AcceptanceCheck>) -> Task {
        Task {
            id: "t1".into(),
            project_id: "p".into(),
            request_id: "r".into(),
            requirement_id: None,
            title: "T".into(),
            description: String::new(),
            capability: "backend".into(),
            role: "backend".into(),
            agent_id: None,
            status: TaskStatus::Verification,
            priority: 1,
            depends_on: vec![],
            inputs: vec![],
            outputs: vec![],
            acceptance: checks,
            resources: vec![],
            max_retries: 1,
            attempts: 1,
            timeout_ms: 60_000,
            deadline_at: None,
            worktree: None,
            branch: None,
            result: None,
            blocker: None,
            created_at: 0,
            updated_at: 0,
            started_at: None,
            ended_at: None,
        }
    }

    fn check(
        kind: AcceptanceKind,
        command: Option<&str>,
        target: Option<&str>,
        expect: Option<&str>,
    ) -> AcceptanceCheck {
        AcceptanceCheck {
            id: "c1".into(),
            description: "criterion".into(),
            kind,
            command: command.map(str::to_string),
            target: target.map(str::to_string),
            expect: expect.map(str::to_string),
        }
    }

    #[test]
    fn file_and_text_criteria_are_executed_for_real() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("out.txt"), "hello world").unwrap();
        let config = ProjectConfig::default();
        let task = task_with(vec![
            check(AcceptanceKind::FileExists, None, Some("out.txt"), None),
            check(
                AcceptanceKind::FileContains,
                None,
                Some("out.txt"),
                Some("hello"),
            ),
        ]);
        let report = verify_locally(
            dir.path(),
            &task,
            &config,
            false,
            VerificationPhase::PreReview,
        );
        assert!(report.criteria.iter().all(|c| c.passed));
    }

    #[test]
    fn pending_review_criteria_defer_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let task = task_with(vec![check(
            AcceptanceKind::ReviewApproved,
            None,
            None,
            None,
        )]);
        // Before the review, a review-only criterion is pending, not failed.
        let report = verify_locally(
            dir.path(),
            &task,
            &ProjectConfig::default(),
            false,
            VerificationPhase::PreReview,
        );
        assert!(
            report.passed,
            "pending criteria must not fail the mechanical pass"
        );
        assert!(report.criteria[0].pending);
        // After the review stage the same criterion needs real evidence.
        let report = verify_locally(
            dir.path(),
            &task,
            &ProjectConfig::default(),
            false,
            VerificationPhase::PostReview,
        );
        assert!(!report.passed, "post-review without evidence must fail");
        let report = verify_locally(
            dir.path(),
            &task,
            &ProjectConfig::default(),
            true,
            VerificationPhase::PostReview,
        );
        assert!(report.passed, "post-review with evidence must pass");
    }

    #[test]
    fn a_task_without_criteria_is_not_verified() {
        let dir = tempfile::tempdir().unwrap();
        let task = task_with(Vec::new());
        let report = verify_locally(
            dir.path(),
            &task,
            &ProjectConfig::default(),
            true,
            VerificationPhase::PostReview,
        );
        assert!(!report.passed, "an empty criteria list must fail closed");
        assert!(report
            .failed
            .iter()
            .any(|item| item.contains("no acceptance criteria")));
    }

    #[test]
    fn non_allowlisted_verification_command_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let task = task_with(vec![check(
            AcceptanceKind::CommandSucceeded,
            Some("curl http://evil.example"),
            None,
            None,
        )]);
        let report = verify_locally(
            dir.path(),
            &task,
            &ProjectConfig::default(),
            true,
            VerificationPhase::PostReview,
        );
        assert!(
            !report.passed,
            "an allowlist refusal can never be an approval"
        );
        assert!(report.criteria[0].detail.contains("allowlist"));
    }
    #[test]
    fn failing_evidence_rows_stay_visible() {
        let store = Store::in_memory().unwrap();
        store
            .insert_project("p", "P", "G:/w", "main", None, &Default::default())
            .unwrap();
        let hub = EventHub::new();
        let task = task_with(vec![check(
            AcceptanceKind::ReviewApproved,
            None,
            None,
            None,
        )]);
        let report = verify_locally(
            Path::new("."),
            &task,
            &ProjectConfig::default(),
            false,
            VerificationPhase::PostReview,
        );
        record_evidence(&store, &hub, "p", &task, &report, "verifier").unwrap();
        let rows = store.evidence_for_task("t1").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].result, "failed");
        assert_eq!(rows[0].kind, EvidenceKind::Review);
        assert_eq!(failing(&store, "t1").unwrap().len(), 1);
    }

    /// Evidence is append-only: a repair cycle's later approval supersedes the earlier
    /// rejection. Reading history as state refused a green merge (observed live).
    #[test]
    fn a_later_approval_supersedes_an_earlier_rejection() {
        let store = Store::in_memory().unwrap();
        store
            .insert_project("p", "P", "G:/w", "main", None, &Default::default())
            .unwrap();
        let row = |result: &str, at: i64| Evidence {
            id: format!("e-{result}-{at}"),
            project_id: "p".into(),
            task_id: "t1".into(),
            kind: EvidenceKind::Review,
            criterion: "independent-review".into(),
            command: None,
            result: result.into(),
            detail: format!("{result} verdict of cycle {at}"),
            artifact: None,
            produced_by: "reviewer".into(),
            created_at: at,
        };
        store.insert_evidence(&row("failed", 1_000)).unwrap();
        assert!(!has_passing_review(&store, "t1").unwrap());
        assert_eq!(failing(&store, "t1").unwrap().len(), 1);
        store.insert_evidence(&row("passed", 2_000)).unwrap();
        assert!(has_passing_review(&store, "t1").unwrap());
        assert!(
            failing(&store, "t1").unwrap().is_empty(),
            "the superseded rejection is history, not the current state"
        );
        // The raw append-only ledger still keeps both rows for forensics.
        assert_eq!(store.evidence_for_task("t1").unwrap().len(), 2);
        assert_eq!(current_evidence(&store, "t1").unwrap().len(), 1);
    }

    /// A pre-review `FileExists` criterion must be recorded as file evidence, never as a
    /// review: `has_passing_review` would otherwise read "the file exists" as an
    /// independent approval and let a rejected task reach integration (observed live).
    #[test]
    fn a_file_exists_criterion_is_not_review_evidence() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("NCAG_CHANGES.md"), "# changes\n").unwrap();
        let task = task_with(vec![check(
            AcceptanceKind::FileExists,
            None,
            Some("NCAG_CHANGES.md"),
            None,
        )]);
        let report = verify_locally(
            dir.path(),
            &task,
            &ProjectConfig::default(),
            false,
            VerificationPhase::PreReview,
        );
        assert!(report.passed);
        assert_eq!(report.criteria[0].kind, EvidenceKind::FileExists);
        let store = Store::in_memory().unwrap();
        store
            .insert_project("p", "P", "G:/w", "main", None, &Default::default())
            .unwrap();
        let hub = EventHub::new();
        record_evidence(&store, &hub, "p", &task, &report, "verifier").unwrap();
        let rows = store.evidence_for_task("t1").unwrap();
        assert_eq!(rows[0].kind, EvidenceKind::FileExists, "{rows:?}");
        assert!(
            !has_passing_review(&store, "t1").unwrap(),
            "a file-existence pass is not an independent review"
        );
    }

    /// Each mechanical criterion keeps its own evidence kind; only the review/audit
    /// criteria are review/audit rows.
    #[test]
    fn each_criterion_records_its_own_evidence_kind() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "needle\n").unwrap();
        let task = task_with(vec![
            check(
                AcceptanceKind::CommandSucceeded,
                Some("echo ok"),
                None,
                None,
            ),
            check(AcceptanceKind::TestPassed, Some("echo ok"), None, None),
            check(AcceptanceKind::FileExists, None, Some("notes.txt"), None),
            check(
                AcceptanceKind::FileContains,
                None,
                Some("notes.txt"),
                Some("needle"),
            ),
            check(AcceptanceKind::ReviewApproved, None, None, None),
        ]);
        let report = verify_locally(
            dir.path(),
            &task,
            &ProjectConfig::default(),
            false,
            VerificationPhase::PreReview,
        );
        let kinds: Vec<EvidenceKind> = report.criteria.iter().map(|item| item.kind).collect();
        assert_eq!(
            kinds,
            vec![
                EvidenceKind::Command,
                EvidenceKind::Test,
                EvidenceKind::FileExists,
                EvidenceKind::FileContains,
                EvidenceKind::Review,
            ]
        );
    }

    /// Between the two phases of one cycle the pre-review criterion and the post-review
    /// criterion are distinct keys, so a pending/other-criterion row does not shadow a
    /// passing one with the same name.
    #[test]
    fn current_evidence_is_keyed_by_kind_and_criterion() {
        let store = Store::in_memory().unwrap();
        store
            .insert_project("p", "P", "G:/w", "main", None, &Default::default())
            .unwrap();
        store
            .insert_evidence(&Evidence {
                id: "a".into(),
                project_id: "p".into(),
                task_id: "t1".into(),
                kind: EvidenceKind::Command,
                criterion: "build".into(),
                command: Some("cargo check".into()),
                result: "passed".into(),
                detail: "exit=0".into(),
                artifact: None,
                produced_by: "verifier".into(),
                created_at: 1,
            })
            .unwrap();
        store
            .insert_evidence(&Evidence {
                id: "b".into(),
                project_id: "p".into(),
                task_id: "t1".into(),
                kind: EvidenceKind::Review,
                criterion: "independent-review".into(),
                command: None,
                result: "passed".into(),
                detail: "approved".into(),
                artifact: None,
                produced_by: "reviewer".into(),
                created_at: 2,
            })
            .unwrap();
        assert_eq!(current_evidence(&store, "t1").unwrap().len(), 2);
        assert!(failing(&store, "t1").unwrap().is_empty());
    }
}
