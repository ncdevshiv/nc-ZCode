//! The v4 workflow read surface (port of the seven queries in
//! `packages/shared/src/nc-code-protocol-v4/{transport,workflow-artifacts,workflow-workspace}.ts` plus
//! the same-named methods in `bootstrap/.../v4-gateway.ts`).
//!
//! All seven methods are **read-only, stateless and safe to resend after a timeout** journal reads —
//! deliberately not v4 commands (a command ACK is the closed "mutation result" discriminated union).
//! The cursor is the journal sequence and never expires: the `JournalStorePort` contract guarantees the
//! sequence is append-only and continuous across a resume.
//!
//! Authorization: `sessionId` must be the run's `parentSessionId` (the same discipline as the TS
//! gateway's `ensureHostRecordForJournalRead`); a mismatch answers exactly like "no such run".

use nc_code_workflow::types::{NodeKind, NodeRecord, NodeRecordStatus, RunEvent, RunStatus};
use serde_json::{json, Map, Value};

use crate::rpc::{RpcError, CODE_INVALID_PARAMS};
use crate::workflow_service::v4_payload;
use crate::workflow_service::{EventPage, RunArtifact, RunListItem, WorkflowService};

/// Same value as `PROTOCOL_V4_LIMITS.attachmentChunkMaxBytes` (the ceiling for one v4 chunked read).
const ARTIFACT_CHUNK_MAX_BYTES: usize = 512 * 1024;
/// Node-count ceiling for the workspace listing (`WORKFLOW_WORKSPACE_LIMITS.maxNodes`).
const WORKSPACE_MAX_NODES: usize = 2_000;
/// Ceiling for one node body read (`WORKFLOW_WORKSPACE_LIMITS.resultMaxBytes`).
const NODE_RESULT_MAX_BYTES: usize = 32 * 1024;
const DEFAULT_ARTIFACT_DATA_LIMIT: usize = 200;
const MAX_ARTIFACT_DATA_LIMIT: usize = 500;

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError::new(CODE_INVALID_PARAMS, message)
}

fn require_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid(format!("params.{key}: required non-empty string")))
}

fn optional_u64(params: &Value, key: &str) -> Result<Option<u64>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| invalid(format!("params.{key}: expected non-negative integer"))),
    }
}

fn run_not_found(run_id: &str) -> RpcError {
    RpcError::new(
        crate::rpc::CODE_INVALID_REQUEST,
        format!("workflow run not found for this session: {run_id}"),
    )
}

/// v4 run status words: the five-value vocabulary (`pending|running|completed|errored|stopped`). A
/// terminal word is taken straight from `runStatus`; no terminal status means `running` (the snapshot
/// vocabulary folds pending into running, so the five words are restored here).
fn lifecycle_status(row: &RunListItem) -> &'static str {
    match row.run_status {
        Some(status) => status.as_str(),
        None => RunStatus::Running.as_str(),
    }
}

fn run_summary(row: &RunListItem) -> Value {
    let mut object = Map::new();
    object.insert("runId".into(), json!(row.run_id));
    if let Some(tool_call_id) = &row.tool_call_id {
        object.insert("toolCallId".into(), json!(tool_call_id));
    }
    if !row.label.is_empty() {
        object.insert("label".into(), json!(row.label));
    }
    object.insert("updatedAt".into(), json!(row.updated_at.round() as i64));
    object.insert("status".into(), json!(lifecycle_status(row)));
    if let Some(reason) = row.stop_reason {
        object.insert("stopReason".into(), json!(reason.as_str()));
    }
    if let Some(resumed_from) = &row.resumed_from {
        object.insert("resumedFrom".into(), json!(resumed_from));
    }
    if let Some(superseded_by) = &row.superseded_by {
        object.insert("supersededBy".into(), json!(superseded_by));
    }
    if let Some(code) = &row.failure_code {
        object.insert("failureCode".into(), json!(code));
    }
    if let Some(message) = &row.failure_message {
        object.insert("failureMessage".into(), json!(message));
    }
    object.insert("resumable".into(), json!(row.resumable));
    Value::Object(object)
}

fn artifact_json(artifact: &RunArtifact) -> Value {
    // RunArtifact's serde shape already matches the protocol field by field (versions / itemCount /
    // primary are all there).
    serde_json::to_value(artifact).unwrap_or(Value::Null)
}

/// The summary of one workspace row: the numbers that can be reported without reading the body (the
/// same measurement basis as TS `workflow-workspace.ts`).
fn workspace_summary(node: &NodeRecord) -> Value {
    let mut summary = Map::new();
    let mut result_bytes = 0usize;
    match (node.status, node.result.as_ref()) {
        (NodeRecordStatus::Completed, Some(Value::Array(items))) => {
            summary.insert("resultCount".into(), json!(items.len()));
            result_bytes = serde_json::to_string(items)
                .map(|text| text.len())
                .unwrap_or(0);
        }
        (NodeRecordStatus::Completed, Some(Value::Object(result)))
            if node.kind == NodeKind::WorldRun =>
        {
            if let Some(exit_code) = result.get("exitCode").and_then(Value::as_i64) {
                summary.insert("exitCode".into(), json!(exit_code));
            }
            let stdout_bytes = result
                .get("stdout")
                .and_then(Value::as_str)
                .map(|text| text.len())
                .unwrap_or(0);
            let stderr_bytes = result
                .get("stderr")
                .and_then(Value::as_str)
                .map(|text| text.len())
                .unwrap_or(0);
            summary.insert("stdoutBytes".into(), json!(stdout_bytes));
            summary.insert("stderrBytes".into(), json!(stderr_bytes));
            result_bytes = serde_json::to_string(result)
                .map(|text| text.len())
                .unwrap_or(0);
        }
        (NodeRecordStatus::Completed, Some(value)) => {
            result_bytes = match value {
                Value::String(text) => text.len(),
                other => serde_json::to_string(other)
                    .map(|text| text.len())
                    .unwrap_or(0),
            };
        }
        _ => {}
    }
    let mut out = Map::new();
    out.insert("resultBytes".into(), json!(result_bytes));
    out.extend(summary);
    Value::Object(out)
}

fn error_json(node: &NodeRecord) -> Option<Value> {
    let error = node.error.as_ref()?;
    Some(json!({"code": error.code.as_str(), "message": error.message}))
}

fn workspace_node_json(
    node: &NodeRecord,
    created_at: Option<f64>,
    updated_at: Option<f64>,
) -> Value {
    let mut object = Map::new();
    object.insert("siteId".into(), json!(node.site_id));
    object.insert("ordinal".into(), json!(node.ordinal));
    object.insert(
        "kind".into(),
        json!(match node.kind {
            NodeKind::WorldRun => "world-run",
            _ => "world-read",
        }),
    );
    if let Some(input) = &node.input {
        object.insert("op".into(), json!(input.op));
        object.insert("args".into(), json!(input.args));
        if input.truncated == Some(true) {
            object.insert("inputTruncated".into(), json!(true));
        }
    }
    object.insert(
        "status".into(),
        json!(match node.status {
            NodeRecordStatus::Running => "running",
            NodeRecordStatus::Completed => "completed",
            NodeRecordStatus::Failed => "failed",
        }),
    );
    if let Some(error) = error_json(node) {
        object.insert("error".into(), error);
    }
    object.insert("summary".into(), workspace_summary(node));
    object.insert(
        "createdAt".into(),
        json!(created_at.unwrap_or(0.0).round() as i64),
    );
    object.insert(
        "updatedAt".into(),
        json!(updated_at.unwrap_or(0.0).round() as i64),
    );
    Value::Object(object)
}

/// Structured tail-trimming to a byte budget: strings keep their head, arrays/objects lose trailing
/// members, recursing parent-then-child. The budget is approximate (2 bytes of overhead per member)
/// and only guarantees the result stays within its order of magnitude, which is what totalBytes records.
fn shrink_to_budget(value: &Value, budget: &mut usize) -> Value {
    const MEMBER_OVERHEAD: usize = 2;
    match value {
        Value::String(text) => {
            let available = budget.saturating_sub(2);
            let mut taken = String::new();
            let mut used = 0usize;
            for character in text.chars() {
                let width = character.len_utf8();
                if used + width > available {
                    break;
                }
                used += width;
                taken.push(character);
            }
            *budget = budget.saturating_sub(used + 2);
            Value::String(taken)
        }
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                if *budget <= MEMBER_OVERHEAD {
                    break;
                }
                *budget -= MEMBER_OVERHEAD;
                out.push(shrink_to_budget(item, budget));
            }
            Value::Array(out)
        }
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, item) in map {
                if *budget <= key.len() + MEMBER_OVERHEAD {
                    break;
                }
                *budget -= key.len() + MEMBER_OVERHEAD;
                out.insert(key.clone(), shrink_to_budget(item, budget));
            }
            Value::Object(out)
        }
        other => {
            let length = serde_json::to_string(other)
                .map(|text| text.len())
                .unwrap_or(0);
            *budget = budget.saturating_sub(length);
            other.clone()
        }
    }
}

fn node_result_json(node: &NodeRecord, max_bytes: usize) -> Value {
    let serialized = node
        .result
        .as_ref()
        .map(|value| serde_json::to_string(value).unwrap_or_default())
        .unwrap_or_default();
    let total_bytes = serialized.len();
    let mut object = Map::new();
    object.insert(
        "status".into(),
        json!(match node.status {
            NodeRecordStatus::Running => "running",
            NodeRecordStatus::Completed => "completed",
            NodeRecordStatus::Failed => "failed",
        }),
    );
    let mut truncated = false;
    if node.status == NodeRecordStatus::Completed {
        if let Some(result) = &node.result {
            if total_bytes <= max_bytes {
                object.insert("result".into(), result.clone());
            } else {
                let mut budget = max_bytes;
                object.insert("result".into(), shrink_to_budget(result, &mut budget));
                truncated = true;
            }
        }
    }
    if let Some(error) = error_json(node) {
        object.insert("error".into(), error);
    }
    object.insert("truncated".into(), json!(truncated));
    object.insert("totalBytes".into(), json!(total_bytes));
    Value::Object(object)
}

impl WorkflowService {
    /// Authorization over `sessionId` × `runId`: the run must exist and its parent session must equal `sessionId`.
    fn authorized_run(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> Option<crate::workflow_service::RunSnapshot> {
        let snapshot = self.get_run(run_id)?;
        if snapshot.parent_session_id.as_deref() != Some(session_id) {
            return None;
        }
        Some(snapshot)
    }

    /// `v4/conversation/workflowRuns`: this session's run enumeration (most recently updated first).
    pub(crate) fn v4_workflow_runs(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = require_str(params, "sessionId")?;
        let limit = optional_u64(params, "limit")?;
        let limit = limit
            .map(|value| {
                value.clamp(
                    1,
                    crate::workflow_service::reads::MAX_LIST_RUNS_LIMIT as u64,
                ) as usize
            })
            .unwrap_or(crate::workflow_service::reads::DEFAULT_LIST_RUNS_LIMIT);
        let rows = self.list_runs_limited(Some(session_id), Some(limit));
        Ok(json!({"runs": rows.iter().map(run_summary).collect::<Vec<_>>()}))
    }

    /// `v4/conversation/workflowRunEvents`: event pagination (cursor = journal sequence).
    pub(crate) fn v4_workflow_run_events(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = require_str(params, "sessionId")?;
        let run_id = require_str(params, "runId")?;
        let after = optional_u64(params, "afterSequence")?;
        let limit = optional_u64(params, "limit")?;
        if self.authorized_run(session_id, run_id).is_none() {
            // An unknown or unauthorized run is not a fault: an empty page with hasMore=false is the
            // fact (the same posture TS takes for an out-of-range cursor).
            return Ok(json!({"events": [], "hasMore": false}));
        }
        let page: EventPage = match limit {
            Some(limit) => self.list_events(run_id, after, Some(limit as usize)),
            None => self.list_events(run_id, after, None),
        };
        Ok(v4_payload::event_page_to_v4(&page))
    }

    /// `v4/conversation/workflowRunArtifacts`: the user-facing artifact listing (all versions).
    pub(crate) fn v4_workflow_run_artifacts(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = require_str(params, "sessionId")?;
        let run_id = require_str(params, "runId")?;
        if self.authorized_run(session_id, run_id).is_none() {
            return Ok(json!({"artifacts": []}));
        }
        let artifacts: Vec<Value> = self
            .list_artifacts(run_id)
            .iter()
            .map(artifact_json)
            .collect();
        Ok(json!({"artifacts": artifacts}))
    }

    /// `v4/conversation/workflowRunArtifactData`: the item page of a preset board (the tagged report rows).
    pub(crate) fn v4_workflow_run_artifact_data(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = require_str(params, "sessionId")?;
        let run_id = require_str(params, "runId")?;
        let artifact_id = require_str(params, "artifactId")?;
        let after = optional_u64(params, "afterSequence")?;
        let limit = optional_u64(params, "limit")?
            .map(|value| value.clamp(1, MAX_ARTIFACT_DATA_LIMIT as u64) as usize)
            .unwrap_or(DEFAULT_ARTIFACT_DATA_LIMIT);
        if self.authorized_run(session_id, run_id).is_none() {
            return Ok(json!({"items": [], "hasMore": false}));
        }
        // Fetch one extra row to decide hasMore: when the page is exactly exhausted, no empty extra page is fetched.
        let want = limit + 1;
        let mut items: Vec<Value> = Vec::new();
        let mut cursor = after;
        loop {
            let page = self.list_events(run_id, cursor, None);
            if page.events.is_empty() {
                break;
            }
            for stored in &page.events {
                cursor = Some(stored.sequence);
                if let RunEvent::Report {
                    instance,
                    item,
                    artifact_id: Some(tag),
                } = &stored.event
                {
                    if tag == artifact_id {
                        items.push(json!({
                            "sequence": stored.sequence,
                            "siteId": instance.site_id,
                            "ordinal": instance.ordinal,
                            "item": item,
                        }));
                        if items.len() >= want {
                            break;
                        }
                    }
                }
            }
            if items.len() >= want || !page.has_more {
                break;
            }
        }
        let has_more = items.len() > limit;
        items.truncate(limit);
        Ok(json!({"items": items, "hasMore": has_more}))
    }

    /// `v4/conversation/workflowRunArtifactRead`: chunked read of a content artifact's bytes (≤512 KiB per chunk).
    pub(crate) fn v4_workflow_run_artifact_read(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = require_str(params, "sessionId")?;
        let run_id = require_str(params, "runId")?;
        let artifact_id = require_str(params, "artifactId")?;
        let version = optional_u64(params, "version")?
            .ok_or_else(|| invalid("params.version: required"))? as u32;
        let offset =
            optional_u64(params, "offset")?.ok_or_else(|| invalid("params.offset: required"))?;
        let limit = optional_u64(params, "limit")?
            .filter(|value| *value >= 1 && *value <= ARTIFACT_CHUNK_MAX_BYTES as u64)
            .ok_or_else(|| invalid("params.limit: out of range"))?;
        if self.authorized_run(session_id, run_id).is_none() {
            return Err(run_not_found(run_id));
        }
        let record = self
            .artifact_bytes(run_id, artifact_id, version)
            .ok_or_else(|| run_not_found(&format!("{run_id}/{artifact_id}@{version}")))?;
        let total = record.bytes.len();
        let offset = usize::try_from(offset).map_err(|_| invalid("params.offset: too large"))?;
        if offset > total {
            return Err(invalid("params.offset: exceeds artifact size"));
        }
        let end = total.min(offset + limit as usize);
        let chunk = &record.bytes[offset..end];
        Ok(json!({
            "dataBase64": crate::tools::read_image::base64_encode(chunk),
            "mediaType": record.content_type.unwrap_or_else(|| "application/octet-stream".to_string()),
            "totalBytes": total,
            "nextOffset": if end < total { json!(end) } else { Value::Null },
        }))
    }

    /// `v4/conversation/workflowRunWorkspace`: the light row listing of world-read/world-run nodes (no bodies).
    pub(crate) fn v4_workflow_run_workspace(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = require_str(params, "sessionId")?;
        let run_id = require_str(params, "runId")?;
        if self.authorized_run(session_id, run_id).is_none() {
            return Ok(json!({"nodes": []}));
        }
        let times: std::collections::HashMap<(String, u32), (f64, f64)> = self
            .node_times(run_id)
            .into_iter()
            .map(|row| {
                (
                    (row.site_id, row.ordinal),
                    (row.time_created, row.time_updated),
                )
            })
            .collect();
        let nodes: Vec<NodeRecord> = self
            .run_nodes(run_id)
            .into_iter()
            .filter(|node| matches!(node.kind, NodeKind::WorldRead | NodeKind::WorldRun))
            .collect();
        let truncated = nodes.len() > WORKSPACE_MAX_NODES;
        let rows: Vec<Value> = nodes
            .iter()
            .take(WORKSPACE_MAX_NODES)
            .map(|node| {
                let (created, updated) = times
                    .get(&(node.site_id.clone(), node.ordinal))
                    .copied()
                    .unwrap_or((0.0, 0.0));
                workspace_node_json(node, Some(created), Some(updated))
            })
            .collect();
        let mut object = Map::new();
        object.insert("nodes".into(), json!(rows));
        if truncated {
            object.insert("truncated".into(), json!(true));
        }
        Ok(Value::Object(object))
    }

    /// `v4/conversation/workflowRunNodeResult`: one node's body (structurally bounded).
    pub(crate) fn v4_workflow_run_node_result(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = require_str(params, "sessionId")?;
        let run_id = require_str(params, "runId")?;
        let site_id = require_str(params, "siteId")?;
        let ordinal = optional_u64(params, "ordinal")?
            .ok_or_else(|| invalid("params.ordinal: required"))? as u32;
        let max_bytes = optional_u64(params, "maxBytes")?
            .map(|value| value.clamp(1, NODE_RESULT_MAX_BYTES as u64) as usize)
            .unwrap_or(NODE_RESULT_MAX_BYTES);
        if self.authorized_run(session_id, run_id).is_none() {
            return Err(run_not_found(run_id));
        }
        let node = self
            .run_nodes(run_id)
            .into_iter()
            .find(|node| node.site_id == site_id && node.ordinal == ordinal)
            .ok_or_else(|| run_not_found(&format!("{run_id}/{site_id}@{ordinal}")))?;
        Ok(node_result_json(&node, max_bytes))
    }
}
