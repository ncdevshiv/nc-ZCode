//! Review, critique and audit findings: evidence-based verdicts with bounded cycles.
//!
//! The reviewer is an independent agent (never the author). Its verdict is parsed from
//! the structured `review_verdict` block it is required to emit; the orchestrator only
//! accepts it when it names files and evidence (spec section 6).

use serde_json::{json, Value};

use crate::db::now_ms;
use crate::error::{NcagError, Result};
use crate::types::{Evidence, EvidenceKind, Severity, Task};
use crate::Store;

#[derive(Debug, Clone)]
pub struct Finding {
    pub id: String,
    pub severity: Severity,
    pub description: String,
    pub evidence: String,
    pub file: Option<String>,
    pub impact: String,
    pub correction: String,
    pub verification: String,
}

#[derive(Debug, Clone)]
pub struct Verdict {
    pub approved: bool,
    pub findings: Vec<Finding>,
    pub rationale: String,
    /// True when the reviewer looked at files (a verdict without evidence is rejected).
    pub has_evidence: bool,
}

pub const VERDICT_START: &str = "<review_verdict>";
pub const VERDICT_END: &str = "</review_verdict>";

/// Parse the verdict block the reviewer is instructed to emit.
pub fn parse_verdict(text: &str) -> Result<Verdict> {
    let start = text
        .find(VERDICT_START)
        .ok_or_else(|| NcagError::invalid("reviewer did not emit a <review_verdict> block"))?;
    let end = text
        .find(VERDICT_END)
        .ok_or_else(|| NcagError::invalid("reviewer verdict block is unterminated"))?;
    let body = &text[start + VERDICT_START.len()..end];
    let value: Value = serde_json::from_str(body.trim()).map_err(|error| {
        NcagError::invalid(format!("reviewer verdict is not valid JSON: {error}"))
    })?;
    let approved = value
        .get("approved")
        .and_then(Value::as_bool)
        .ok_or_else(|| NcagError::invalid("reviewer verdict has no boolean 'approved'"))?;
    let findings: Vec<Finding> = value
        .get("findings")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .enumerate()
                .map(|(index, item)| Finding {
                    id: item
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("F{}", index + 1)),
                    severity: parse_severity(
                        item.get("severity")
                            .and_then(Value::as_str)
                            .unwrap_or("medium"),
                    ),
                    description: item
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    evidence: item
                        .get("evidence")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    file: item.get("file").and_then(Value::as_str).map(str::to_string),
                    impact: item
                        .get("impact")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    correction: item
                        .get("correction")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    verification: item
                        .get("verification")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    let has_evidence = value
        .get("reviewedFiles")
        .and_then(Value::as_array)
        .map(|files| {
            files
                .iter()
                .any(|file| file.as_str().is_some_and(|value| !value.is_empty()))
        })
        .unwrap_or(false)
        || findings.iter().any(|finding| finding.file.is_some());
    // An approval without evidence, or with a critical/high finding, is not an approval.
    let blocked = findings
        .iter()
        .any(|finding| matches!(finding.severity, Severity::Critical | Severity::High));
    let approved = approved && has_evidence && !blocked;
    Ok(Verdict {
        approved,
        findings,
        rationale: value
            .get("rationale")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        has_evidence,
    })
}

pub fn parse_severity(text: &str) -> Severity {
    match text.trim().to_ascii_lowercase().as_str() {
        "critical" => Severity::Critical,
        "high" => Severity::High,
        "low" => Severity::Low,
        "informational" | "info" => Severity::Informational,
        _ => Severity::Medium,
    }
}

/// Persist a verdict as evidence and as review_finding messages.
pub fn record_verdict(
    store: &Store,
    hub: &crate::events::EventHub,
    project_id: &str,
    task: &Task,
    verdict: &Verdict,
    reviewer: &str,
) -> Result<()> {
    let detail = json!({
        "rationale": verdict.rationale,
        "findings": verdict.findings.iter().map(|finding| json!({
            "id": finding.id,
            "severity": finding.severity.as_str(),
            "description": finding.description,
            "evidence": finding.evidence,
            "file": finding.file,
            "impact": finding.impact,
            "correction": finding.correction,
            "verification": finding.verification,
        })).collect::<Vec<_>>(),
    });
    store.insert_evidence(&Evidence {
        id: uuid::Uuid::new_v4().to_string(),
        project_id: project_id.to_string(),
        task_id: task.id.clone(),
        kind: EvidenceKind::Review,
        criterion: "independent-review".to_string(),
        command: None,
        result: if verdict.approved { "passed" } else { "failed" }.to_string(),
        detail: crate::security::redact(&detail.to_string()),
        artifact: task.worktree.clone(),
        produced_by: reviewer.to_string(),
        created_at: now_ms(),
    })?;
    for finding in &verdict.findings {
        if finding.severity < Severity::Low {
            continue;
        }
        let message = crate::messaging::NewMessage::to_project(
            project_id,
            Some(&task.id),
            crate::types::MessageKind::ReviewFinding,
            format!("[{}] {}", finding.severity.as_str(), finding.description),
            finding.evidence.clone(),
        );
        let _ = crate::messaging::send(store, message);
    }
    crate::events::emit(
        store,
        hub,
        Some(project_id),
        Some(&task.id),
        None,
        if verdict.approved {
            "review.approved"
        } else {
            "review.rejected"
        },
        json!({
            "taskId": task.id,
            "reviewer": reviewer,
            "findings": verdict.findings.len(),
            "rationale": verdict.rationale,
        }),
    )?;
    store.append_audit(
        Some(project_id),
        reviewer,
        if verdict.approved {
            "review.approved"
        } else {
            "review.rejected"
        },
        &json!({ "taskId": task.id, "findings": verdict.findings.len() }),
        now_ms(),
    )?;
    Ok(())
}

/// Review-cycle accounting: after `max_cycles` failed reviews the loop must stop and
/// escalate instead of repeating.
#[derive(Debug, Clone, PartialEq)]
pub enum CycleDecision {
    Retry,
    Escalate { cycles: u32, reason: String },
}

pub fn cycle_decision(cycles: u32, max_cycles: u32, last_findings: &[Finding]) -> CycleDecision {
    if cycles >= max_cycles {
        let critical: Vec<&Finding> = last_findings
            .iter()
            .filter(|finding| matches!(finding.severity, Severity::Critical | Severity::High))
            .collect();
        let reason = if critical.is_empty() {
            format!("review did not converge after {cycles} cycle(s)")
        } else {
            format!(
                "review did not converge after {cycles} cycle(s); unresolved high/critical findings: {}",
                critical
                    .iter()
                    .map(|finding| finding.description.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        };
        CycleDecision::Escalate { cycles, reason }
    } else {
        CycleDecision::Retry
    }
}

/// The review prompt is shared by code review and security audit so both see the same
/// evidence contract.
pub fn review_prompt(role: &str, task: &Task, diff_hint: &str) -> String {
    format!(
        "You are the independent {role}. Review task \"{title}\" in the worktree.\n\
         Requirements and acceptance criteria:\n{acceptance}\n\
         Changed files (from git): {diff_hint}\n\
         Read the actual files and run the allowed checks. Do not accept the author's claim.\n\
         End your reply with exactly one block:\n\
         <review_verdict>{{\"approved\":<bool>,\"rationale\":\"...\",\
         \"reviewedFiles\":[\"...\"],\"findings\":[{{\"id\":\"F1\",\"severity\":\
         \"critical|high|medium|low|informational\",\"description\":\"...\",\"evidence\":\
         \"command output or code quote\",\"file\":\"path\",\"impact\":\"...\",\
         \"correction\":\"...\",\"verification\":\"how to verify the fix\"}}]}}</review_verdict>\n\
         Approve only when every acceptance criterion has evidence. Any critical or high \
         finding means not approved.",
        title = task.title,
        acceptance = task
            .acceptance
            .iter()
            .map(|check| format!("- {} ({:?})", check.description, check.kind))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Cycle accounting value persisted on the task result (no new table needed).
pub fn review_cycles_key() -> &'static str {
    "reviewCycles"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(approved: bool, findings: &str, files: bool) -> String {
        format!(
            "<review_verdict>{{\"approved\":{approved},\"rationale\":\"checked\",\
             \"reviewedFiles\":{},\"findings\":[{findings}]}}</review_verdict>",
            if files { "[\"src/lib.rs\"]" } else { "[]" }
        )
    }

    #[test]
    fn approval_requires_evidence_and_no_high_findings() {
        let parsed = parse_verdict(&verdict(true, "", true)).unwrap();
        assert!(parsed.approved);
        let parsed = parse_verdict(&verdict(true, "", false)).unwrap();
        assert!(!parsed.approved, "approval without evidence must not pass");
        let parsed = parse_verdict(&verdict(
            true,
            "{\"severity\":\"high\",\"description\":\"missing error handling\",\"file\":\"src/lib.rs\"}",
            true,
        ))
        .unwrap();
        assert!(!parsed.approved);
        assert_eq!(parsed.findings[0].severity, Severity::High);
    }

    #[test]
    fn missing_or_malformed_verdict_is_an_error() {
        assert!(parse_verdict("looks fine to me").is_err());
        assert!(parse_verdict("<review_verdict>not json</review_verdict>").is_err());
        assert!(parse_verdict("<review_verdict>{\"approved\":true}</review_verdict>").is_ok());
    }

    #[test]
    fn cycles_stop_at_the_cap_and_name_unresolved_findings() {
        let findings = vec![Finding {
            id: "F1".into(),
            severity: Severity::High,
            description: "SQL injection".into(),
            evidence: "string concatenation".into(),
            file: Some("db.rs".into()),
            impact: "data breach".into(),
            correction: "use parameters".into(),
            verification: "test".into(),
        }];
        assert_eq!(cycle_decision(0, 2, &findings), CycleDecision::Retry);
        match cycle_decision(2, 2, &findings) {
            CycleDecision::Escalate { reason, cycles } => {
                assert_eq!(cycles, 2);
                assert!(reason.contains("SQL injection"));
            }
            other => panic!("expected escalation, got {other:?}"),
        }
    }
}
