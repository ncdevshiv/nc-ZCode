//! Repositories for messages, leases, decisions, evidence, integrations, usage,
//! events and audit reads.

use rusqlite::{params, Row};
use serde_json::Value;

use crate::db::{from_text, now_ms, to_text, Store};
use crate::error::{NcagError, Result};
use crate::types::{
    AuditEntry, Decision, DecisionClass, DecisionOption, DecisionStatus, Event, Evidence,
    EvidenceKind, Integration, Lease, LeaseMode, Message, MessageKind, ProjectUsage, UsageRow,
};

fn message_from_row(row: &Row<'_>) -> rusqlite::Result<Message> {
    let kind_text: String = row.get("kind")?;
    Ok(Message {
        id: row.get("id")?,
        project_id: row.get("project_id")?,
        task_id: row.get("task_id")?,
        kind: MessageKind::parse(&kind_text).unwrap_or(MessageKind::Information),
        sender: row.get("sender")?,
        recipient: row.get("recipient")?,
        channel: row.get("channel")?,
        subject: row.get("subject")?,
        body: row.get("body")?,
        refs: crate::db::json_vec(&row.get::<_, String>("refs_json")?),
        correlation_id: row.get("correlation_id")?,
        priority: row.get("priority")?,
        requires_ack: row.get::<_, i64>("requires_ack")? != 0,
        ack_state: row.get("ack_state")?,
        delivery_state: row.get("delivery_state")?,
        dedupe_key: row.get("dedupe_key")?,
        created_at: row.get("created_at")?,
    })
}

fn lease_from_row(row: &Row<'_>) -> rusqlite::Result<Lease> {
    let mode_text: String = row.get("mode")?;
    Ok(Lease {
        id: row.get("id")?,
        project_id: row.get("project_id")?,
        resource: row.get("resource")?,
        mode: LeaseMode::parse(&mode_text).unwrap_or(LeaseMode::ExclusiveWrite),
        owner_agent: row.get("owner_agent")?,
        owner_task: row.get("owner_task")?,
        fencing_token: row.get::<_, i64>("fencing_token")? as u64,
        state: row.get("state")?,
        acquired_at: row.get("acquired_at")?,
        expires_at: row.get("expires_at")?,
        released_at: row.get("released_at")?,
        release_reason: row.get("release_reason")?,
    })
}

fn decision_from_row(row: &Row<'_>) -> rusqlite::Result<Decision> {
    let class_text: String = row.get("class")?;
    let status_text: String = row.get("status")?;
    let evidence_text: String = row.get("evidence_json")?;
    let options_text: String = row.get("options_json")?;
    let risks_text: String = row.get("risks_json")?;
    Ok(Decision {
        id: row.get("id")?,
        project_id: row.get("project_id")?,
        request_id: row.get("request_id")?,
        task_id: row.get("task_id")?,
        class: DecisionClass::parse(&class_text).unwrap_or(DecisionClass::Autonomous),
        status: DecisionStatus::parse(&status_text).unwrap_or(DecisionStatus::Open),
        topic: row.get("topic")?,
        requested: row.get("requested")?,
        recommendation: row.get("recommendation")?,
        evidence: crate::db::json_vec(&evidence_text),
        options: serde_json::from_str(&options_text).unwrap_or_default(),
        risks: crate::db::json_vec(&risks_text),
        resolution: row.get("resolution")?,
        rationale: row.get("rationale")?,
        decided_by: row.get("decided_by")?,
        created_at: row.get("created_at")?,
        resolved_at: row.get("resolved_at")?,
    })
}

impl Store {
    // ---------------------------------------------------------------- messages

    pub fn insert_message(&self, message: &Message) -> Result<bool> {
        let conn = self.raw_lock();
        let changed = conn.execute(
            "INSERT OR IGNORE INTO messages
             (id, project_id, task_id, kind, sender, recipient, channel, subject, body, refs_json,
              correlation_id, priority, requires_ack, ack_state, delivery_state, dedupe_key, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            params![
                message.id,
                message.project_id,
                message.task_id,
                message.kind.as_str(),
                message.sender,
                message.recipient,
                message.channel,
                message.subject,
                message.body,
                to_text(&serde_json::json!(message.refs)),
                message.correlation_id,
                message.priority,
                message.requires_ack as i64,
                message.ack_state,
                message.delivery_state,
                message.dedupe_key,
                message.created_at
            ],
        )?;
        Ok(changed > 0)
    }

    pub fn messages_for_project(&self, project_id: &str, limit: i64) -> Result<Vec<Message>> {
        let conn = self.raw_lock();
        let mut statement = conn.prepare(
            "SELECT * FROM messages WHERE project_id = ?1 ORDER BY created_at DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![project_id, limit], message_from_row)?;
        let mut items: Vec<Message> = rows.filter_map(std::result::Result::ok).collect();
        items.reverse();
        Ok(items)
    }

    pub fn messages_for_agent(&self, project_id: &str, agent_id: &str) -> Result<Vec<Message>> {
        let conn = self.raw_lock();
        let mut statement = conn.prepare(
            "SELECT * FROM messages WHERE project_id = ?1 AND (recipient = ?2 OR sender = ?2)
             ORDER BY created_at ASC LIMIT 200",
        )?;
        let rows = statement.query_map(params![project_id, agent_id], message_from_row)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    pub fn message(&self, id: &str) -> Result<Message> {
        let conn = self.raw_lock();
        let mut statement = conn.prepare("SELECT * FROM messages WHERE id = ?1")?;
        statement
            .query_row([id], message_from_row)
            .ok()
            .ok_or_else(|| NcagError::not_found(format!("message {id} does not exist")))
    }

    pub fn ack_message(&self, id: &str, by: &str) -> Result<bool> {
        let changed = {
            let conn = self.raw_lock();
            conn.execute(
                "UPDATE messages SET ack_state = 'acknowledged' WHERE id = ?1 AND requires_ack = 1
                 AND ack_state = 'pending'",
                params![id],
            )?
        };
        if changed > 0 {
            // The connection guard is dropped first: auditing opens the same connection.
            self.append_audit(
                None,
                by,
                "message.acknowledged",
                &serde_json::json!({ "messageId": id }),
                now_ms(),
            )?;
        }
        Ok(changed > 0)
    }

    pub fn mark_delivered(&self, id: &str, state: &str) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "UPDATE messages SET delivery_state = ?2 WHERE id = ?1",
            params![id, state],
        )?;
        Ok(())
    }

    // ---------------------------------------------------------------- leases

    pub fn active_lease(&self, project_id: &str, resource: &str) -> Result<Option<Lease>> {
        let conn = self.raw_lock();
        let mut statement = conn.prepare(
            "SELECT * FROM leases WHERE project_id = ?1 AND resource = ?2 AND state = 'active'
             ORDER BY acquired_at ASC LIMIT 1",
        )?;
        let found = statement
            .query_row(params![project_id, resource], lease_from_row)
            .ok();
        Ok(found)
    }

    pub fn leases_for_project(&self, project_id: &str) -> Result<Vec<Lease>> {
        let conn = self.raw_lock();
        let mut statement =
            conn.prepare("SELECT * FROM leases WHERE project_id = ?1 ORDER BY acquired_at DESC")?;
        let rows = statement.query_map([project_id], lease_from_row)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    pub fn active_leases(&self) -> Result<Vec<Lease>> {
        let conn = self.raw_lock();
        let mut statement =
            conn.prepare("SELECT * FROM leases WHERE state = 'active' ORDER BY acquired_at ASC")?;
        let rows = statement.query_map([], lease_from_row)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    pub fn insert_lease(&self, lease: &Lease) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "INSERT INTO leases
             (id, project_id, resource, mode, owner_agent, owner_task, fencing_token, state,
              acquired_at, expires_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                lease.id,
                lease.project_id,
                lease.resource,
                lease.mode.as_str(),
                lease.owner_agent,
                lease.owner_task,
                lease.fencing_token as i64,
                lease.state,
                lease.acquired_at,
                lease.expires_at
            ],
        )?;
        Ok(())
    }

    pub fn release_lease(&self, id: &str, reason: &str) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "UPDATE leases SET state = 'released', released_at = ?2, release_reason = ?3
             WHERE id = ?1 AND state = 'active'",
            params![id, now_ms(), reason],
        )?;
        Ok(())
    }

    pub fn renew_lease(&self, id: &str, expires_at: i64) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "UPDATE leases SET expires_at = ?2 WHERE id = ?1 AND state = 'active'",
            params![id, expires_at],
        )?;
        Ok(())
    }

    pub fn expire_leases(&self) -> Result<usize> {
        let now = now_ms();
        let conn = self.raw_lock();
        let changed = conn.execute(
            "UPDATE leases SET state = 'expired', released_at = ?1, release_reason = 'expired'
             WHERE state = 'active' AND expires_at <= ?1",
            params![now],
        )?;
        Ok(changed)
    }

    pub fn next_fencing_token(&self) -> Result<u64> {
        let conn = self.raw_lock();
        let max: i64 = conn.query_row(
            "SELECT COALESCE(MAX(fencing_token), 0) FROM leases",
            [],
            |row| row.get(0),
        )?;
        Ok((max as u64) + 1)
    }

    // ---------------------------------------------------------------- decisions

    pub fn insert_decision(&self, decision: &Decision) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "INSERT INTO decisions
             (id, project_id, request_id, task_id, class, status, topic, requested, recommendation,
              evidence_json, options_json, risks_json, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                decision.id,
                decision.project_id,
                decision.request_id,
                decision.task_id,
                decision.class.as_str(),
                decision.status.as_str(),
                decision.topic,
                decision.requested,
                decision.recommendation,
                to_text(&serde_json::json!(decision.evidence)),
                to_text(&serde_json::to_value(&decision.options)?),
                to_text(&serde_json::json!(decision.risks)),
                decision.created_at
            ],
        )?;
        Ok(())
    }

    pub fn decision(&self, id: &str) -> Result<Decision> {
        let conn = self.raw_lock();
        let mut statement = conn.prepare("SELECT * FROM decisions WHERE id = ?1")?;
        statement
            .query_row([id], decision_from_row)
            .ok()
            .ok_or_else(|| NcagError::not_found(format!("decision {id} does not exist")))
    }

    pub fn decisions_for_project(&self, project_id: &str) -> Result<Vec<Decision>> {
        let conn = self.raw_lock();
        let mut statement =
            conn.prepare("SELECT * FROM decisions WHERE project_id = ?1 ORDER BY created_at DESC")?;
        let rows = statement.query_map([project_id], decision_from_row)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    pub fn all_decisions(&self) -> Result<Vec<Decision>> {
        let conn = self.raw_lock();
        let mut statement = conn.prepare("SELECT * FROM decisions ORDER BY created_at DESC")?;
        let rows = statement.query_map([], decision_from_row)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    pub fn open_decisions(&self) -> Result<Vec<Decision>> {
        let conn = self.raw_lock();
        let mut statement =
            conn.prepare("SELECT * FROM decisions WHERE status = 'open' ORDER BY created_at ASC")?;
        let rows = statement.query_map([], decision_from_row)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    pub fn resolve_decision(
        &self,
        id: &str,
        status: DecisionStatus,
        resolution: &str,
        rationale: Option<&str>,
        decided_by: &str,
    ) -> Result<bool> {
        let conn = self.raw_lock();
        let changed = conn.execute(
            "UPDATE decisions SET status = ?2, resolution = ?3, rationale = ?4, decided_by = ?5,
             resolved_at = ?6 WHERE id = ?1 AND status = 'open'",
            params![
                id,
                status.as_str(),
                resolution,
                rationale,
                decided_by,
                now_ms()
            ],
        )?;
        Ok(changed > 0)
    }

    // ---------------------------------------------------------------- evidence

    pub fn insert_evidence(&self, evidence: &Evidence) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "INSERT INTO evidence
             (id, project_id, task_id, kind, criterion, command, result, detail, artifact,
              produced_by, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                evidence.id,
                evidence.project_id,
                evidence.task_id,
                evidence.kind.as_str(),
                evidence.criterion,
                evidence.command,
                evidence.result,
                evidence.detail,
                evidence.artifact,
                evidence.produced_by,
                evidence.created_at
            ],
        )?;
        Ok(())
    }

    pub fn evidence_for_task(&self, task_id: &str) -> Result<Vec<Evidence>> {
        let conn = self.raw_lock();
        let mut statement =
            conn.prepare("SELECT * FROM evidence WHERE task_id = ?1 ORDER BY created_at ASC")?;
        let rows = statement.query_map([task_id], evidence_from_row)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    pub fn evidence_for_project(&self, project_id: &str) -> Result<Vec<Evidence>> {
        let conn = self.raw_lock();
        let mut statement =
            conn.prepare("SELECT * FROM evidence WHERE project_id = ?1 ORDER BY created_at ASC")?;
        let rows = statement.query_map([project_id], evidence_from_row)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    // ---------------------------------------------------------------- integrations

    pub fn insert_integration(&self, integration: &Integration) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "INSERT INTO integrations
             (id, project_id, task_id, base_commit, commit_sha, changed_json, conflicts_json,
              tests_json, outcome, detail, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                integration.id,
                integration.project_id,
                integration.task_id,
                integration.base_commit,
                integration.commit_sha,
                to_text(&serde_json::json!(integration.changed)),
                to_text(&serde_json::json!(integration.conflicts)),
                to_text(&serde_json::json!(integration.tests)),
                integration.outcome,
                integration.detail,
                integration.created_at
            ],
        )?;
        Ok(())
    }

    pub fn integrations_for_project(&self, project_id: &str) -> Result<Vec<Integration>> {
        let conn = self.raw_lock();
        let mut statement = conn
            .prepare("SELECT * FROM integrations WHERE project_id = ?1 ORDER BY created_at ASC")?;
        let rows = statement.query_map([project_id], integration_from_row)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    // ---------------------------------------------------------------- usage

    pub fn record_usage(&self, usage: &UsageRow) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "INSERT INTO usage
             (id, project_id, task_id, agent_id, provider, model, input_tokens, output_tokens,
              cost_usd, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                uuid::Uuid::new_v4().to_string(),
                usage.project_id,
                usage.task_id,
                usage.agent_id,
                usage.provider,
                usage.model,
                usage.input_tokens as i64,
                usage.output_tokens as i64,
                usage.cost_usd,
                now_ms()
            ],
        )?;
        Ok(())
    }

    pub fn usage_for_project(&self, project_id: &str) -> Result<ProjectUsage> {
        let conn = self.raw_lock();
        let mut statement = conn.prepare(
            "SELECT COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0),
             COALESCE(SUM(cost_usd),0), COUNT(*) FROM usage WHERE project_id = ?1",
        )?;
        let usage = statement.query_row([project_id], |row| {
            Ok(ProjectUsage {
                input_tokens: row.get::<_, i64>(0)? as u64,
                output_tokens: row.get::<_, i64>(1)? as u64,
                cost_usd: row.get(2)?,
                calls: row.get::<_, i64>(3)? as u64,
            })
        })?;
        Ok(usage)
    }

    pub fn usage_rows(&self, project_id: &str) -> Result<Vec<UsageRow>> {
        let conn = self.raw_lock();
        let mut statement = conn.prepare(
            "SELECT project_id, task_id, agent_id, provider, model, input_tokens, output_tokens,
             cost_usd, created_at FROM usage WHERE project_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = statement.query_map([project_id], |row| {
            Ok(UsageRow {
                project_id: row.get(0)?,
                task_id: row.get(1)?,
                agent_id: row.get(2)?,
                provider: row.get(3)?,
                model: row.get(4)?,
                input_tokens: row.get::<_, i64>(5)? as u64,
                output_tokens: row.get::<_, i64>(6)? as u64,
                cost_usd: row.get(7)?,
                created_at: row.get(8)?,
            })
        })?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    // ---------------------------------------------------------------- events / audit

    pub fn append_event(
        &self,
        project_id: Option<&str>,
        task_id: Option<&str>,
        agent_id: Option<&str>,
        kind: &str,
        payload: &Value,
    ) -> Result<Event> {
        let at = now_ms();
        let conn = self.raw_lock();
        conn.execute(
            "INSERT INTO events (project_id, task_id, agent_id, kind, payload_json, created_at)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![project_id, task_id, agent_id, kind, to_text(payload), at],
        )?;
        let id: i64 = conn.last_insert_rowid();
        Ok(Event {
            id,
            project_id: project_id.map(str::to_string),
            task_id: task_id.map(str::to_string),
            agent_id: agent_id.map(str::to_string),
            kind: kind.to_string(),
            payload: payload.clone(),
            created_at: at,
        })
    }

    pub fn events_after(&self, after_id: i64, limit: i64) -> Result<Vec<Event>> {
        let conn = self.raw_lock();
        let mut statement = conn.prepare(
            "SELECT id, project_id, task_id, agent_id, kind, payload_json, created_at
             FROM events WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![after_id, limit], |row| {
            Ok(Event {
                id: row.get(0)?,
                project_id: row.get(1)?,
                task_id: row.get(2)?,
                agent_id: row.get(3)?,
                kind: row.get(4)?,
                payload: from_text(&row.get::<_, String>(5)?),
                created_at: row.get(6)?,
            })
        })?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    pub fn latest_event_id(&self) -> Result<i64> {
        let conn = self.raw_lock();
        Ok(
            conn.query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |row| {
                row.get(0)
            })?,
        )
    }

    pub fn recent_audit(&self, limit: i64) -> Result<Vec<AuditEntry>> {
        let conn = self.raw_lock();
        let mut statement = conn.prepare(
            "SELECT id, project_id, actor, action, detail_json, prev_hash, hash, created_at
             FROM audit ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map(params![limit], |row| {
            Ok(AuditEntry {
                id: row.get(0)?,
                project_id: row.get(1)?,
                actor: row.get(2)?,
                action: row.get(3)?,
                detail: from_text(&row.get::<_, String>(4)?),
                prev_hash: row.get(5)?,
                hash: row.get(6)?,
                created_at: row.get(7)?,
            })
        })?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }
}

fn evidence_from_row(row: &Row<'_>) -> rusqlite::Result<Evidence> {
    let kind_text: String = row.get("kind")?;
    Ok(Evidence {
        id: row.get("id")?,
        project_id: row.get("project_id")?,
        task_id: row.get("task_id")?,
        kind: EvidenceKind::parse(&kind_text).unwrap_or(EvidenceKind::Command),
        criterion: row.get("criterion")?,
        command: row.get("command")?,
        result: row.get("result")?,
        detail: row.get("detail")?,
        artifact: row.get("artifact")?,
        produced_by: row.get("produced_by")?,
        created_at: row.get("created_at")?,
    })
}

fn integration_from_row(row: &Row<'_>) -> rusqlite::Result<Integration> {
    Ok(Integration {
        id: row.get("id")?,
        project_id: row.get("project_id")?,
        task_id: row.get("task_id")?,
        base_commit: row.get("base_commit")?,
        commit_sha: row.get("commit_sha")?,
        changed: crate::db::json_vec(&row.get::<_, String>("changed_json")?),
        conflicts: crate::db::json_vec(&row.get::<_, String>("conflicts_json")?),
        tests: crate::db::json_vec(&row.get::<_, String>("tests_json")?),
        outcome: row.get("outcome")?,
        detail: row.get("detail")?,
        created_at: row.get("created_at")?,
    })
}

/// Build a decision option for a debate record.
pub fn option(
    id: &str,
    label: &str,
    pros: &[&str],
    cons: &[&str],
    risks: &[&str],
) -> DecisionOption {
    DecisionOption {
        id: id.to_string(),
        label: label.to_string(),
        pros: pros.iter().map(|value| value.to_string()).collect(),
        cons: cons.iter().map(|value| value.to_string()).collect(),
        risks: risks.iter().map(|value| value.to_string()).collect(),
    }
}
