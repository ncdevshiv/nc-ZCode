//! Integration manager: turns a task branch into an integrated commit only when the
//! evidence is green. A textual merge is a hypothesis; tests decide (section 6).

use serde_json::json;

use crate::db::now_ms;
use crate::error::{NcagError, Result};
use crate::events::EventHub;
use crate::git::{overlapping, GitRepo};
use crate::types::{EvidenceKind, Integration, ProjectConfig, Task};
use crate::Store;

#[derive(Debug, Clone, PartialEq)]
pub enum IntegrationOutcome {
    Merged {
        commit: String,
        changed: Vec<String>,
    },
    Conflict {
        files: Vec<String>,
        detail: String,
    },
    /// Integration was refused because required evidence is missing or red.
    Refused {
        reason: String,
    },
}

/// Pre-check: everything that must be true before a merge is attempted.
///
/// Judged on **current** evidence (latest row per kind+criterion): a repair cycle that
/// turned a rejection into an approval must not be blocked by the superseded rejection
/// (that refused a green live task exactly once and aborted the request).
pub fn precheck(store: &Store, task: &Task) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    let evidence = crate::verification::current_evidence(store, &task.id)?;
    if evidence.is_empty() {
        problems.push("no verification evidence was recorded for this task".to_string());
    }
    if evidence.iter().any(|row| row.result == "failed") {
        problems.push(
            "the task has failing evidence; a merge would integrate known-bad work".to_string(),
        );
    }
    if !evidence.iter().any(|row| row.kind == EvidenceKind::Review) {
        problems.push("no independent review evidence".to_string());
    }
    if task.worktree.is_none() {
        problems.push("the task has no worktree to integrate".to_string());
    }
    Ok(problems)
}

/// Merge a task branch into the project branch when its evidence is green.
#[allow(clippy::too_many_arguments)]
pub fn integrate(
    store: &Store,
    hub: &EventHub,
    repo: &GitRepo,
    project_id: &str,
    project_branch: &str,
    base_commit: &str,
    task: &Task,
    config: &ProjectConfig,
) -> Result<IntegrationOutcome> {
    let problems = precheck(store, task)?;
    if !problems.is_empty() {
        record(
            store,
            hub,
            project_id,
            task,
            IntegrationOutcome::Refused {
                reason: problems.join("; "),
            },
        )?;
        return Ok(IntegrationOutcome::Refused {
            reason: problems.join("; "),
        });
    }
    let Some(worktree) = &task.worktree else {
        return Err(NcagError::invalid("task has no worktree"));
    };
    let worktree = std::path::PathBuf::from(worktree);
    let branch = task
        .branch
        .clone()
        .ok_or_else(|| NcagError::invalid("task has no branch"))?;
    // Manifest of what this task changed, and the overlap with already integrated work.
    let changed = repo.changed_files(&worktree, base_commit)?;
    let committed = repo.commit_all(&worktree, &format!("ncag: {}", task.title))?;
    if let Some(sha) = &committed {
        let _ = sha;
    }
    let scratch = format!("merge-{}", task.id.replace(['/', '\\', ':'], "-"));
    let attempt = repo.try_merge(project_branch, &branch, &scratch)?;
    if !attempt.merged {
        let detail = format!(
            "textual conflict while merging {}: {}",
            branch,
            attempt.conflicts.join(", ")
        );
        // A conflict is a decision-worthy fact: record it, never silently resolve.
        crate::messaging::escalate(
            store,
            project_id,
            Some(&task.id),
            "integration conflict",
            &detail,
        )?;
        record(
            store,
            hub,
            project_id,
            task,
            IntegrationOutcome::Conflict {
                files: attempt.conflicts.clone(),
                detail: detail.clone(),
            },
        )?;
        return Ok(IntegrationOutcome::Conflict {
            files: attempt.conflicts,
            detail,
        });
    }
    // Run the project's test command in the merged scratch worktree before committing.
    let mut tests: Vec<String> = Vec::new();
    let mut tests_ok = true;
    if let Some(command) = crate::planner::detect_repo_facts(&worktree).test_command {
        if config.allowed_commands.iter().any(|allowed| {
            command
                .to_ascii_lowercase()
                .starts_with(&allowed.to_ascii_lowercase())
        }) {
            let scratch_path = repo.worktrees_dir.join(&scratch);
            let mut process = crate::tools::shell_command(&command);
            process.current_dir(&scratch_path);
            process
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            #[cfg(windows)]
            {
                #[allow(unused_imports)]
                use std::os::windows::process::CommandExt;
                process.creation_flags(0x0800_0000);
            }
            let output = process.output();
            match output {
                Ok(output) if output.status.success() => tests.push(format!("{command}: passed")),
                Ok(output) => {
                    tests_ok = false;
                    tests.push(format!(
                        "{command}: failed (exit {}) {}",
                        output.status.code().unwrap_or(-1),
                        crate::tools::truncate_text(
                            &crate::tools::decode_output(&output.stderr),
                            400
                        )
                    ));
                }
                Err(error) => {
                    tests_ok = false;
                    tests.push(format!("{command}: could not run ({error})"));
                }
            }
        }
    }
    if !tests_ok {
        repo.abort_merge(&scratch)?;
        let detail = format!("integrated build failed after merge: {}", tests.join("; "));
        record(
            store,
            hub,
            project_id,
            task,
            IntegrationOutcome::Conflict {
                files: changed,
                detail: detail.clone(),
            },
        )?;
        return Ok(IntegrationOutcome::Conflict {
            files: Vec::new(),
            detail,
        });
    }
    let commit = repo.finalize_merge(
        &scratch,
        project_branch,
        &format!("ncag: integrate {} ({})", task.title, task.id),
    )?;
    let outcome = IntegrationOutcome::Merged {
        commit: commit.clone(),
        changed: changed.clone(),
    };
    record(
        store,
        hub,
        project_id,
        task,
        IntegrationOutcome::Merged {
            commit: commit.clone(),
            changed: changed.clone(),
        },
    )?;
    Ok(outcome)
}

fn record(
    store: &Store,
    hub: &EventHub,
    project_id: &str,
    task: &Task,
    outcome: IntegrationOutcome,
) -> Result<()> {
    let (outcome_text, detail, commit, changed, conflicts, tests) = match &outcome {
        IntegrationOutcome::Merged { commit, changed } => (
            "merged".to_string(),
            format!("merged {} file(s)", changed.len()),
            Some(commit.clone()),
            changed.clone(),
            Vec::new(),
            Vec::new(),
        ),
        IntegrationOutcome::Conflict { files, detail } => (
            "conflict".to_string(),
            detail.clone(),
            None,
            Vec::new(),
            files.clone(),
            Vec::new(),
        ),
        IntegrationOutcome::Refused { reason } => (
            "refused".to_string(),
            reason.clone(),
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ),
    };
    let integration = Integration {
        id: uuid::Uuid::new_v4().to_string(),
        project_id: project_id.to_string(),
        task_id: task.id.clone(),
        base_commit: task.started_at.map(|_| String::new()),
        commit_sha: commit.clone(),
        changed,
        conflicts: conflicts.clone(),
        tests,
        outcome: outcome_text.clone(),
        detail: crate::security::redact(&detail),
        created_at: now_ms(),
    };
    store.insert_integration(&integration)?;
    store.insert_evidence(&crate::types::Evidence {
        id: uuid::Uuid::new_v4().to_string(),
        project_id: project_id.to_string(),
        task_id: task.id.clone(),
        kind: EvidenceKind::Integration,
        criterion: "integration".to_string(),
        command: None,
        result: if outcome_text == "merged" {
            "passed"
        } else {
            "failed"
        }
        .to_string(),
        detail: integration.detail.clone(),
        artifact: commit,
        produced_by: "integration-manager".to_string(),
        created_at: now_ms(),
    })?;
    crate::events::emit(
        store,
        hub,
        Some(project_id),
        Some(&task.id),
        None,
        if outcome_text == "merged" {
            "integration.merged"
        } else {
            "integration.conflict"
        },
        json!({
            "taskId": task.id,
            "outcome": outcome_text,
            "detail": integration.detail,
            "conflicts": conflicts,
        }),
    )?;
    store.append_audit(
        Some(project_id),
        "integration-manager",
        "integration",
        &json!({ "taskId": task.id, "outcome": outcome_text }),
        now_ms(),
    )?;
    Ok(())
}

/// Semantic conflict record: two implementations that are textually compatible but
/// contradict each other (different contracts for the same route, etc.).
pub fn record_semantic_conflict(
    store: &Store,
    hub: &EventHub,
    project_id: &str,
    task_a: &Task,
    task_b: &Task,
    files: &[String],
    why: &str,
) -> Result<String> {
    let topic = format!("semantic conflict: {} vs {}", task_a.title, task_b.title);
    let decision = crate::decisions::open(
        store,
        hub,
        crate::decisions::DebateRequest {
            project_id: project_id.to_string(),
            request_id: Some(task_a.request_id.clone()),
            task_id: Some(task_a.id.clone()),
            topic: topic.clone(),
            requested: format!("{} proposes {}", task_a.title, why),
            recommendation: format!(
                "reconcile the two implementations in one follow-up task after the orchestrator decides the contract; {} stays unmerged until then",
                task_b.title
            ),
            evidence: vec![format!("overlapping files: {}", files.join(", ")), why.to_string()],
            risks: vec!["shipping both contracts would leave two sources of truth".to_string()],
            class: crate::types::DecisionClass::TechnicalRecommendation,
        },
    )?;
    crate::messaging::escalate(
        store,
        project_id,
        Some(&task_a.id),
        "architecture conflict",
        &format!("{} ({})", topic, decision.id),
    )?;
    Ok(decision.id)
}

/// Overlap of two tasks' changed files (integration pre-check).
pub fn file_overlap(
    store: &Store,
    repo: &GitRepo,
    base: &str,
    task_a: &Task,
    task_b: &Task,
) -> Result<Vec<String>> {
    let a = task_a
        .worktree
        .as_ref()
        .map(|path| repo.changed_files(&std::path::PathBuf::from(path), base))
        .transpose()?
        .unwrap_or_default();
    let b = task_b
        .worktree
        .as_ref()
        .map(|path| repo.changed_files(&std::path::PathBuf::from(path), base))
        .transpose()?
        .unwrap_or_default();
    let _ = store;
    Ok(overlapping(&a, &b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Evidence, TaskStatus};

    fn task(worktree: Option<&str>) -> Task {
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
            status: TaskStatus::Completed,
            priority: 1,
            depends_on: vec![],
            inputs: vec![],
            outputs: vec![],
            acceptance: vec![],
            resources: vec![],
            max_retries: 1,
            attempts: 1,
            timeout_ms: 60_000,
            deadline_at: None,
            worktree: worktree.map(str::to_string),
            branch: Some("ncag/t1".into()),
            result: None,
            blocker: None,
            created_at: 0,
            updated_at: 0,
            started_at: None,
            ended_at: None,
        }
    }

    #[test]
    fn integration_without_evidence_is_refused() {
        let store = Store::in_memory().unwrap();
        let problems = precheck(&store, &task(Some("x"))).unwrap();
        assert!(problems
            .iter()
            .any(|item| item.contains("no verification evidence")));
        assert!(problems.iter().any(|item| !item.contains("no worktree")));
    }

    #[test]
    fn failed_or_missing_review_blocks_integration() {
        let store = Store::in_memory().unwrap();
        let t = task(Some("x"));
        store
            .insert_evidence(&Evidence {
                id: "e1".into(),
                project_id: "p".into(),
                task_id: "t1".into(),
                kind: EvidenceKind::Command,
                criterion: "c".into(),
                command: Some("cargo test".into()),
                result: "failed".into(),
                detail: "red".into(),
                artifact: None,
                produced_by: "verifier".into(),
                created_at: 1,
            })
            .unwrap();
        let problems = precheck(&store, &t).unwrap();
        assert!(problems
            .iter()
            .any(|item| item.contains("failing evidence")));
        assert!(problems
            .iter()
            .any(|item| item.contains("no independent review")));
    }

    /// A task that was rejected in cycle 1 and approved after its repair is green: the
    /// superseded rejection must not refuse the merge (observed live — the refusal turned
    /// a repaired task into a failed request).
    #[test]
    fn a_superseded_rejection_does_not_block_the_merge() {
        let store = Store::in_memory().unwrap();
        let t = task(Some("x"));
        let row = |kind: EvidenceKind, criterion: &str, result: &str, at: i64| Evidence {
            id: format!("e-{criterion}-{at}"),
            project_id: "p".into(),
            task_id: "t1".into(),
            kind,
            criterion: criterion.into(),
            command: (kind == EvidenceKind::Command).then(|| "cargo test".to_string()),
            result: result.into(),
            detail: format!("{result} at cycle {at}"),
            artifact: None,
            produced_by: "verifier".into(),
            created_at: at,
        };
        store
            .insert_evidence(&row(EvidenceKind::Command, "build", "passed", 1))
            .unwrap();
        store
            .insert_evidence(&row(
                EvidenceKind::Review,
                "independent-review",
                "failed",
                2,
            ))
            .unwrap();
        store
            .insert_evidence(&row(
                EvidenceKind::Review,
                "independent-review",
                "passed",
                3,
            ))
            .unwrap();
        let problems = precheck(&store, &t).unwrap();
        assert!(
            problems.is_empty(),
            "a repaired, approved task is integrable: {problems:?}"
        );
    }

    /// A criterion that is still failing in the *current* verdict keeps refusing.
    #[test]
    fn a_still_failing_criterion_keeps_refusing_the_merge() {
        let store = Store::in_memory().unwrap();
        let t = task(Some("x"));
        for (criterion, result, at) in [
            ("build", "passed", 1),
            ("tests", "failed", 2),
            ("independent-review", "passed", 3),
        ] {
            let kind = if criterion == "independent-review" {
                EvidenceKind::Review
            } else {
                EvidenceKind::Command
            };
            store
                .insert_evidence(&Evidence {
                    id: format!("e-{criterion}"),
                    project_id: "p".into(),
                    task_id: "t1".into(),
                    kind,
                    criterion: criterion.into(),
                    command: (kind == EvidenceKind::Command).then(|| "cargo test".to_string()),
                    result: result.into(),
                    detail: result.to_string(),
                    artifact: None,
                    produced_by: "verifier".into(),
                    created_at: at,
                })
                .unwrap();
        }
        let problems = precheck(&store, &t).unwrap();
        assert!(problems
            .iter()
            .any(|item| item.contains("failing evidence")));
    }
}
