//! Debate and decision system (spec: specs/nc-ag-orchestration.md section 1.3, 2.4).
//!
//! Four classes: autonomous, technical recommendation, material, user-reserved.
//! A material/user-reserved decision is never auto-resolved: it opens a decision and the
//! project waits for the user. The record keeps the requested approach, the
//! recommendation, evidence, pros/cons/risks and the final resolution.

use serde_json::json;

use crate::db::now_ms;
use crate::error::{NcagError, Result};
use crate::events::EventHub;
use crate::repo_misc::option;
use crate::types::{Decision, DecisionClass, DecisionStatus, MessageKind};
use crate::Store;

#[derive(Debug, Clone)]
pub struct DebateRequest {
    pub project_id: String,
    pub request_id: Option<String>,
    pub task_id: Option<String>,
    pub topic: String,
    pub requested: String,
    pub recommendation: String,
    pub evidence: Vec<String>,
    pub risks: Vec<String>,
    pub class: DecisionClass,
}

/// Open a decision record. Material/user-reserved decisions pause their request.
pub fn open(store: &Store, hub: &EventHub, debate: DebateRequest) -> Result<Decision> {
    if debate.requested.trim().is_empty() || debate.recommendation.trim().is_empty() {
        return Err(NcagError::invalid(
            "a debate needs both the requested approach and the recommendation",
        ));
    }
    if debate.evidence.is_empty() {
        return Err(NcagError::invalid(
            "a debate needs evidence; unsupported disagreements are not recorded",
        ));
    }
    let decision = Decision {
        id: uuid::Uuid::new_v4().to_string(),
        project_id: debate.project_id.clone(),
        request_id: debate.request_id.clone(),
        task_id: debate.task_id.clone(),
        class: debate.class,
        status: DecisionStatus::Open,
        topic: debate.topic,
        requested: debate.requested,
        recommendation: debate.recommendation,
        evidence: debate.evidence,
        options: vec![
            option(
                "requested",
                "The requested approach",
                &["matches the user's stated preference", "simpler to explain"],
                &["carries the risks listed in the evidence"],
                &["see the recorded risks"],
            ),
            option(
                "recommended",
                "NC-AG's recommendation",
                &["addresses each recorded risk", "leaves a rollback path"],
                &["costs additional implementation effort"],
                &["may change established workflow"],
            ),
        ],
        risks: debate.risks,
        resolution: None,
        rationale: None,
        decided_by: None,
        created_at: now_ms(),
        resolved_at: None,
    };
    store.insert_decision(&decision)?;
    let message = crate::messaging::NewMessage {
        project_id: decision.project_id.clone(),
        task_id: decision.task_id.clone(),
        kind: MessageKind::Escalation,
        sender: "orchestrator".to_string(),
        recipient: "user".to_string(),
        channel: "global".to_string(),
        subject: format!("Decision required: {}", decision.topic),
        body: format!(
            "Requested: {}\nRecommendation: {}\nEvidence: {}",
            decision.requested,
            decision.recommendation,
            decision.evidence.join("; ")
        ),
        refs: vec![decision.id.clone()],
        correlation_id: decision.id.clone(),
        priority: 5,
        requires_ack: true,
        dedupe_key: None,
    };
    crate::messaging::send(store, message)?;
    crate::events::emit(
        store,
        hub,
        Some(&decision.project_id),
        decision.task_id.as_deref(),
        None,
        "decision.opened",
        json!({
            "decisionId": decision.id,
            "topic": decision.topic,
            "class": decision.class.as_str(),
            "needsUser": decision.class.needs_user(),
        }),
    )?;
    store.append_audit(
        Some(&decision.project_id),
        "orchestrator",
        "decision.opened",
        &json!({ "decisionId": decision.id, "class": decision.class.as_str() }),
        now_ms(),
    )?;
    Ok(decision)
}

/// Resolve a decision. `by` is `user` for user-reserved/material, `orchestrator`
/// otherwise. The resolution is permanent and not reopen without new evidence.
pub fn resolve(
    store: &Store,
    hub: &EventHub,
    decision_id: &str,
    resolution: &str,
    rationale: Option<&str>,
    by: &str,
) -> Result<Decision> {
    let decision = store.decision(decision_id)?;
    if decision.status != DecisionStatus::Open {
        return Err(NcagError::conflict(format!(
            "decision {decision_id} is already {}",
            decision.status.as_str()
        )));
    }
    let accepted = resolution.trim();
    if accepted.is_empty() {
        return Err(NcagError::invalid("resolution must not be empty"));
    }
    let status = if accepted == "recommended" || accepted == "recommendation" {
        DecisionStatus::Resolved
    } else {
        DecisionStatus::Overridden
    };
    if decision.class.needs_user() && by != "user" {
        return Err(NcagError::new(
            "user_decision_required",
            format!(
                "decision {} is {} and may only be resolved by the user",
                decision.id,
                decision.class.as_str()
            ),
        ));
    }
    if !store.resolve_decision(decision_id, status, accepted, rationale, by)? {
        return Err(NcagError::conflict(
            "decision changed while resolving; retry",
        ));
    }
    let mut resolved = store.decision(decision_id)?;
    resolved.resolution = Some(accepted.to_string());
    resolved.rationale = rationale.map(str::to_string);
    resolved.decided_by = Some(by.to_string());
    resolved.status = status;
    // Notify every agent on the project: a settled decision is context, not a re-debate.
    let notification = crate::messaging::NewMessage::to_project(
        &decision.project_id,
        decision.task_id.as_deref(),
        MessageKind::Information,
        format!("Decision settled: {}", decision.topic),
        format!(
            "Decision: {accepted}. Rationale: {}. The orchestrator applies this immediately.",
            rationale.unwrap_or("(none provided)")
        ),
    );
    crate::messaging::send(store, notification)?;
    crate::events::emit(
        store,
        hub,
        Some(&decision.project_id),
        decision.task_id.as_deref(),
        None,
        "decision.resolved",
        json!({
            "decisionId": decision.id,
            "resolution": accepted,
            "status": status.as_str(),
            "decidedBy": by,
        }),
    )?;
    store.append_audit(
        Some(&decision.project_id),
        by,
        "decision.resolved",
        &json!({ "decisionId": decision.id, "resolution": accepted }),
        now_ms(),
    )?;
    Ok(resolved)
}

/// A settled decision for the same topic: callers check before re-opening a debate.
pub fn settled_for_topic(store: &Store, project_id: &str, topic: &str) -> Result<Option<Decision>> {
    Ok(store
        .decisions_for_project(project_id)?
        .into_iter()
        .find(|decision| {
            decision.topic == topic
                && matches!(
                    decision.status,
                    DecisionStatus::Resolved | DecisionStatus::Overridden
                )
        }))
}

/// Build the debate NC-AG opens when a user request conflicts with its recommendation.
pub fn architecture_conflict(
    project_id: &str,
    request_id: Option<&str>,
    topic: &str,
    requested: &str,
    recommendation: &str,
    evidence: Vec<String>,
    risks: Vec<String>,
    class: DecisionClass,
) -> DebateRequest {
    DebateRequest {
        project_id: project_id.to_string(),
        request_id: request_id.map(str::to_string),
        task_id: None,
        topic: topic.to_string(),
        requested: requested.to_string(),
        recommendation: recommendation.to_string(),
        evidence,
        risks,
        class,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (Store, EventHub, String) {
        let store = Store::in_memory().unwrap();
        store
            .insert_project("p", "P", "G:/w", "main", None, &Default::default())
            .unwrap();
        (store, EventHub::new(), "p".to_string())
    }

    fn debate(class: DecisionClass) -> DebateRequest {
        DebateRequest {
            project_id: "p".into(),
            request_id: None,
            task_id: None,
            topic: "shared working directory".into(),
            requested: "let agents edit the shared checkout".into(),
            recommendation: "use isolated git worktrees".into(),
            evidence: vec!["two agents editing the same file lost changes in test".into()],
            risks: vec!["concurrent edits can overwrite each other".into()],
            class,
        }
    }

    #[test]
    fn a_debate_needs_evidence_and_both_sides() {
        let (store, hub, _) = setup();
        let mut request = debate(DecisionClass::Material);
        request.evidence.clear();
        assert!(open(&store, &hub, request).is_err());
    }

    #[test]
    fn material_decisions_need_the_user() {
        let (store, hub, _) = setup();
        let decision = open(&store, &hub, debate(DecisionClass::Material)).unwrap();
        assert!(decision.class.needs_user());
        let error = resolve(
            &store,
            &hub,
            &decision.id,
            "requested",
            None,
            "orchestrator",
        )
        .expect_err("orchestrator cannot settle a material decision");
        assert_eq!(error.code, "user_decision_required");
        let resolved = resolve(
            &store,
            &hub,
            &decision.id,
            "requested",
            Some("user preference"),
            "user",
        )
        .unwrap();
        assert_eq!(resolved.status, DecisionStatus::Overridden);
        assert!(resolved.resolution.is_some());
    }

    #[test]
    fn autonomous_decisions_resolve_without_the_user_and_do_not_reopen() {
        let (store, hub, _) = setup();
        let decision = open(&store, &hub, debate(DecisionClass::TechnicalRecommendation)).unwrap();
        resolve(
            &store,
            &hub,
            &decision.id,
            "recommended",
            Some("cheaper"),
            "orchestrator",
        )
        .unwrap();
        let again = resolve(&store, &hub, &decision.id, "requested", None, "user");
        assert_eq!(again.unwrap_err().code, "conflict");
        let settled = settled_for_topic(&store, "p", "shared working directory")
            .unwrap()
            .expect("settled");
        assert_eq!(settled.status, DecisionStatus::Resolved);
    }
}
