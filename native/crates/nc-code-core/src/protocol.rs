//! v4 command protocol base type: Mirror `packages/shared/src/nc-code-protocol-v4/command.ts`
//! The envelope / ACK / ruling dependency side, and the limits actually used by the inbox in `PROTOCOL_V4_LIMITS`.
//!
//! Boundary description (with evidence):
//! - Envelope-level field verification is transplanted according to zod semantics; **payload's type-by-type zod schema is not transplanted here**——
//! Schema is owned by `packages/shared` (roadmap S0 coexistence period source of truth),
//! This crate only transparently transmits `serde_json::Value` to the payload.
//! - The `ttft` field is only transparently transmitted without verification (`localTtftContextSchema` is shared; zod will reject illegal
//! ttft, accepted here - documented as porting differences).

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// createSession and null sessionId query belong to the global bucket (TS `GLOBAL_BUCKET`).
pub const GLOBAL_BUCKET: &str = "@global";

/// `PROTOCOL_V4_LIMITS.idempotencyTablePerSession`: settled idempotent table capacity per bucket (LRU).
pub const IDEMPOTENCY_TABLE_PER_SESSION: usize = 512;

// ── Stable reasonCode vocabulary (strictly align TS literals) ──
pub const REASON_INVALID_PAYLOAD: &str = "proto.invalidPayload";
pub const REASON_SESSION_NOT_FOUND: &str = "proto.sessionNotFound";
pub const REASON_MISSING_BASE_REVISION: &str = "proto.missingBaseRevision";
pub const REASON_STALE_LOG_EPOCH: &str = "proto.staleLogEpoch";
pub const REASON_STALE_REVISION: &str = "proto.staleRevision";
/// New design addition (TS inbox has no pending cancellation path, see crate README): queued admission is cancelled.
pub const REASON_ADMISSION_CANCELLED: &str = "fault.command.admissionCancelled";

/// TS `parseCommandEnvelope` Synthetic error text for missing fields in CAS messages.
pub const PARSE_CAS_MESSAGE: &str = "CAS commands require baseRevision and baseLogEpoch";

pub const CREATE_SESSION_TYPE: &str = "createSession";

// ── Complete set of commandTypeSchema (= key of commandPayloadSchemas, 34 items) ──
pub const COMMAND_TYPES: &[&str] = &[
    "createSession",
    "createSelectionSideSession",
    "sendText",
    "sendGoalCommand",
    "stop",
    "compact",
    "forkAssistant",
    "applyFileRewind",
    "editUserQuery",
    "retryTurn",
    "setAssistantFeedback",
    "sendQueuedNow",
    "editQueueItem",
    "reorderQueueItem",
    "deleteQueueItem",
    "setAutoDrain",
    "resolveInteraction",
    "respondWorkspaceHookReview",
    "toggleWorkspaceHookReviewItem",
    "revokeWorkspaceHookTrust",
    "requestWorkspaceHookReview",
    "snoozeInteractionAutoResolution",
    "switchModelConfig",
    "switchCollaborationMode",
    "setFollowupMode",
    "pauseGoal",
    "resumeGoal",
    "cancelBackgroundWork",
    "resumeWorkflowRun",
    "startSavedWorkflow",
    "amendWorkflowRunSettings",
    "renameSession",
    "deleteSession",
    "discardSharedContext",
];

/// TS `COMMANDS_REQUIRING_BASE_REVISION`: CAS command must carry baseRevision.
pub const COMMANDS_REQUIRING_BASE_REVISION: &[&str] = &[
    "applyFileRewind",
    "forkAssistant",
    "editUserQuery",
    "retryTurn",
    "setAssistantFeedback",
    "sendQueuedNow",
    "editQueueItem",
    "reorderQueueItem",
    "deleteQueueItem",
    "setAutoDrain",
    "switchModelConfig",
    "switchCollaborationMode",
    "setFollowupMode",
    "pauseGoal",
    "resumeGoal",
];

/// TS `ROW_TARGETING_COMMANDS`: baseLogEpoch must also be carried for row-level epoch CAS.
pub const ROW_TARGETING_COMMANDS: &[&str] = &[
    "applyFileRewind",
    "forkAssistant",
    "editUserQuery",
    "retryTurn",
    "setAssistantFeedback",
];

pub fn is_known_command_type(command_type: &str) -> bool {
    COMMAND_TYPES.contains(&command_type)
}

pub fn requires_base_revision(command_type: &str) -> bool {
    COMMANDS_REQUIRING_BASE_REVISION.contains(&command_type)
}

pub fn is_row_targeting(command_type: &str) -> bool {
    ROW_TARGETING_COMMANDS.contains(&command_type)
}

// ── CommandAck (camelCase + omit undefined, wire shape alignment commandAckSchema) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    Accepted,
    Rejected,
    Stale,
    Duplicate,
    Noop,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandAck {
    pub command_id: String,
    pub status: CommandStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub revision_at_decision: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_enabled: Option<bool>,
}

impl CommandAck {
    pub fn new(command_id: impl Into<String>, status: CommandStatus, revision: i64) -> Self {
        Self {
            command_id: command_id.into(),
            status,
            reason_code: None,
            message: None,
            revision_at_decision: revision,
            result: None,
            memory_enabled: None,
        }
    }
}

/// TS `CommandFinal`: final state field for settlement backfill (the default field does not cover admission ACK).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandFinal {
    pub status: CommandStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_enabled: Option<bool>,
}

impl CommandFinal {
    pub fn accepted() -> Self {
        Self {
            status: CommandStatus::Accepted,
            reason_code: None,
            message: None,
            result: None,
            memory_enabled: None,
        }
    }

    pub fn with_reason(status: CommandStatus, reason_code: impl Into<String>) -> Self {
        Self {
            status,
            reason_code: Some(reason_code.into()),
            message: None,
            result: None,
            memory_enabled: None,
        }
    }
}

/// TS `{ ...decision.ack, ...final }` Expansion semantics: The final default field retains base and will be overwritten if it occurs.
pub fn merge_final(base: &CommandAck, final_: &CommandFinal) -> CommandAck {
    CommandAck {
        command_id: base.command_id.clone(),
        status: final_.status,
        reason_code: final_
            .reason_code
            .clone()
            .or_else(|| base.reason_code.clone()),
        message: final_.message.clone().or_else(|| base.message.clone()),
        revision_at_decision: base.revision_at_decision,
        result: final_.result.clone().or_else(|| base.result.clone()),
        memory_enabled: final_.memory_enabled.or(base.memory_enabled),
    }
}

// ── CommandKey / Query results ──

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandKey {
    /// null only belongs to the global/createSession idempotent bucket; the session command must carry sessionId.
    pub session_id: Option<String>,
    pub command_id: String,
}

impl CommandKey {
    pub fn new(session_id: Option<&str>, command_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.map(str::to_owned),
            command_id: command_id.into(),
        }
    }
}

// ── Envelope ──

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandEnvelope {
    #[serde(rename = "type")]
    pub command_type: String,
    pub command_id: String,
    pub client_id: String,
    /// null when creatingSession.
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_revision: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_log_epoch: Option<String>,
    /// Unverified payload (returned to shared by type schema).
    #[serde(default)]
    pub payload: Value,
    /// Client clock, telemetry only; server not used for any arbitration (TS `issuedAt: z.number()`).
    pub issued_at: f64,
    /// Transparent transmission without verification, see module doc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParseEnvelopeError {
    pub message: String,
}

impl fmt::Display for ParseEnvelopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ParseEnvelopeError {}

fn err<T>(message: &str) -> Result<T, ParseEnvelopeError> {
    Err(ParseEnvelopeError {
        message: message.to_owned(),
    })
}

/// Mirror `parseCommandEnvelope`: envelope field validation + CAS field existence check.
/// Failure is mapped to `proto.invalidPayload` (copy is different from zod, reasonCode is the same - see README difference table).
pub fn parse_command_envelope(raw: &Value) -> Result<CommandEnvelope, ParseEnvelopeError> {
    let obj = match raw.as_object() {
        Some(obj) => obj,
        None => return err("invalid envelope: expected object"),
    };
    let command_id = match obj.get("commandId").and_then(Value::as_str) {
        Some(v) => v,
        None => return err("commandId: required string"),
    };
    let client_id = match obj.get("clientId").and_then(Value::as_str) {
        Some(v) => v,
        None => return err("clientId: required string"),
    };
    let session_id = match obj.get("sessionId") {
        Some(Value::String(s)) => Some(s.as_str()),
        Some(Value::Null) => None,
        _ => return err("sessionId: required string or null"),
    };
    let command_type = match obj.get("type").and_then(Value::as_str) {
        Some(v) => v,
        None => return err("type: required string"),
    };
    if !is_known_command_type(command_type) {
        return err("type: unrecognized command type");
    }
    let base_revision = match obj.get("baseRevision") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => match n.as_i64() {
            Some(v) => Some(v),
            // zod z.number() accepts non-integer numbers; the revision contract is an integer, and non-integer numbers are directly illegal.
            None => return err("baseRevision: expected integer number"),
        },
        _ => return err("baseRevision: expected number"),
    };
    // zod `z.string().trim().min(1)`: trim is a transformation, and the trimmed value is stored.
    let base_log_epoch = match obj.get("baseLogEpoch") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => {
            let trimmed = crate::identity::js_trim(s);
            if trimmed.is_empty() {
                return err("baseLogEpoch: must be non-empty after trim");
            }
            Some(trimmed.to_owned())
        }
        _ => return err("baseLogEpoch: expected string"),
    };
    let issued_at = match obj.get("issuedAt").and_then(Value::as_f64) {
        Some(v) => v,
        None => return err("issuedAt: required number"),
    };
    // The zod object strips unknown keys by default: payload/ttft is transparently transmitted as it is (payload type-by-type verification is not in this crate).
    let payload = obj.get("payload").cloned().unwrap_or(Value::Null);
    let ttft = obj.get("ttft").cloned();

    if requires_base_revision(command_type) && base_revision.is_none() {
        return Err(ParseEnvelopeError {
            message: PARSE_CAS_MESSAGE.to_owned(),
        });
    }
    if is_row_targeting(command_type) && base_log_epoch.is_none() {
        return Err(ParseEnvelopeError {
            message: PARSE_CAS_MESSAGE.to_owned(),
        });
    }

    Ok(CommandEnvelope {
        command_type: command_type.to_owned(),
        command_id: command_id.to_owned(),
        client_id: client_id.to_owned(),
        session_id: session_id.map(str::to_owned),
        base_revision,
        base_log_epoch,
        payload,
        issued_at,
        ttft,
    })
}

/// TS `extractCommandId`: When parsing fails, try to get the commandId (non-string → "").
pub fn extract_command_id(raw: &Value) -> String {
    raw.get("commandId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

// ── Host callback surface (TS `CommandInboxHost`; Option method → ​​default implementation) ──

/// guard decision results: allow / stale / reject / noop (mirror TS `GuardDecision`).
#[derive(Debug, Clone, PartialEq)]
pub enum GuardDecision {
    Allow,
    Stale {
        reason_code: String,
        message: Option<String>,
    },
    Reject {
        reason_code: String,
        message: Option<String>,
    },
    Noop {
        reason_code: String,
        result: Option<Value>,
    },
}

impl GuardDecision {
    pub fn allow() -> Self {
        GuardDecision::Allow
    }

    pub fn stale(reason_code: impl Into<String>) -> Self {
        GuardDecision::Stale {
            reason_code: reason_code.into(),
            message: None,
        }
    }

    pub fn reject(reason_code: impl Into<String>) -> Self {
        GuardDecision::Reject {
            reason_code: reason_code.into(),
            message: None,
        }
    }

    pub fn noop(reason_code: impl Into<String>) -> Self {
        GuardDecision::Noop {
            reason_code: reason_code.into(),
            result: None,
        }
    }
}

/// The callback surface of inbox → host. `default` corresponds to the optional member of TS (default pass/miss).
///
/// Constraints (consistent with TS single-threaded callbacks): Implement **not reentrant** inbox method - callback in
/// Execute within inbox state lock.
pub trait CommandInboxHost: Send + Sync {
    /// The current revision of the session; unknown session returns `None` (createSession uses 0, without this callback).
    fn get_revision(&self, session_id: &str) -> Option<i64>;

    /// The current projection generation of the session; CAS first verifies the epoch and then the revision.
    fn get_log_epoch(&self, session_id: &str) -> Option<String>;

    /// The entity/action origin resolver of the row-targeting command determines.
    fn validate_row_target(&self, _envelope: &CommandEnvelope) -> GuardDecision {
        GuardDecision::Allow
    }

    /// Business guard (product-protocol guard id). By default, all requests are allowed.
    fn guard(&self, _envelope: &CommandEnvelope) -> GuardDecision {
        GuardDecision::Allow
    }

    /// Persist back to the source, the order is the priority: transcript → timeline → child → discarded.
    /// TS returns a Promise that can be rejected (→ `fault.command.queryUnavailable`); Rust side
    /// This callback cannot fail, and the queryUnavailable branch is only retained as a buffer when the in-transit fact is cleared by clearSession.
    fn lookup_transcript_command(&self, _key: &CommandKey) -> Option<CommandAck> {
        None
    }

    fn lookup_timeline_command(&self, _key: &CommandKey) -> Option<CommandAck> {
        None
    }

    fn lookup_child_command(&self, _key: &CommandKey) -> Option<CommandAck> {
        None
    }

    fn lookup_discarded_command(&self, _key: &CommandKey) -> Option<CommandAck> {
        None
    }

    /// TS `host.now?.() ?? Date.now()`。
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// The CAS and row-targeting tables gate dispatch; a wrong membership lets a
    /// stale client mutate state without a base revision.
    #[test]
    fn command_classification_matches_the_v4_tables() {
        assert!(is_known_command_type("sendText"));
        assert!(!is_known_command_type("notACommand"));
        assert!(requires_base_revision("switchModelConfig"));
        assert!(!requires_base_revision("sendText"));
        assert!(is_row_targeting("editUserQuery"));
        assert!(!is_row_targeting("sendText"));
    }
}
