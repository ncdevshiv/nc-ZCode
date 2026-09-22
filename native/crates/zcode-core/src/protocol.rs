//! v4 命令协议基础类型：镜像 `packages/shared/src/zcode-protocol-v4/command.ts`
//! 的信封 / ACK / 裁决依赖面，以及 `PROTOCOL_V4_LIMITS` 中 inbox 实际使用的限额。
//!
//! 边界说明（有据）：
//! - 信封级字段校验按 zod 语义移植；**payload 的逐类型 zod schema 不在此移植**——
//!   schema 归 `packages/shared` 所有（roadmap S0 共存期 source of truth），
//!   本 crate 对 payload 只透传 `serde_json::Value`。
//! - `ttft` 字段仅透传不校验（`localTtftContextSchema` 归 shared；zod 会拒绝非法
//!   ttft，这里接受——已记录为移植差异）。

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// createSession 与 null sessionId query 归全局桶（TS `GLOBAL_BUCKET`）。
pub const GLOBAL_BUCKET: &str = "@global";

/// `PROTOCOL_V4_LIMITS.idempotencyTablePerSession`：每桶 settled 幂等表容量（LRU）。
pub const IDEMPOTENCY_TABLE_PER_SESSION: usize = 512;

// ── 稳定 reasonCode 词表（严格对齐 TS 字面量） ──
pub const REASON_INVALID_PAYLOAD: &str = "proto.invalidPayload";
pub const REASON_SESSION_NOT_FOUND: &str = "proto.sessionNotFound";
pub const REASON_MISSING_BASE_REVISION: &str = "proto.missingBaseRevision";
pub const REASON_STALE_LOG_EPOCH: &str = "proto.staleLogEpoch";
pub const REASON_STALE_REVISION: &str = "proto.staleRevision";
/// 设计新增（TS inbox 无 pending 取消路径，见 crate README）：排队 admission 被取消。
pub const REASON_ADMISSION_CANCELLED: &str = "fault.command.admissionCancelled";

/// TS `parseCommandEnvelope` 对 CAS 信合缺失字段的合成错误文案。
pub const PARSE_CAS_MESSAGE: &str = "CAS commands require baseRevision and baseLogEpoch";

pub const CREATE_SESSION_TYPE: &str = "createSession";

// ── commandTypeSchema 全集（= commandPayloadSchemas 的 key，34 项） ──
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

/// TS `COMMANDS_REQUIRING_BASE_REVISION`：CAS 命令必带 baseRevision。
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

/// TS `ROW_TARGETING_COMMANDS`：还必须携带 baseLogEpoch 做行级 epoch CAS。
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

// ── CommandAck（camelCase + 省略 undefined，wire 形状对齐 commandAckSchema） ──

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

/// TS `CommandFinal`：settle 回填的终态字段（缺省字段不覆盖 admission ACK）。
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

/// TS `{ ...decision.ack, ...final }` 展开语义：final 缺省字段保留 base，出现则覆盖。
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

// ── CommandKey / 查询结果 ──

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandKey {
    /// null 只属于全局/createSession 幂等桶；session 命令必须携带 sessionId。
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

// ── 信封 ──

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandEnvelope {
    #[serde(rename = "type")]
    pub command_type: String,
    pub command_id: String,
    pub client_id: String,
    /// createSession 时为 null。
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_revision: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_log_epoch: Option<String>,
    /// 未校验的 payload（逐类型 schema 归 shared）。
    #[serde(default)]
    pub payload: Value,
    /// 客户端时钟，仅遥测；服务端不用于任何裁决（TS `issuedAt: z.number()`）。
    pub issued_at: f64,
    /// 透传不校验，见模块 doc。
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

/// 镜像 `parseCommandEnvelope`：信封字段校验 + CAS 字段存在性检查。
/// 失败映射到 `proto.invalidPayload`（文案与 zod 不同，reasonCode 相同——见 README 差异表）。
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
            // zod z.number() 接受非整数；revision 契约为整数，非整数直接判非法。
            None => return err("baseRevision: expected integer number"),
        },
        _ => return err("baseRevision: expected number"),
    };
    // zod `z.string().trim().min(1)`：trim 是变换，存的是裁剪后的值。
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
    // zod 对象默认剥未知键：payload/ttft 原样透传（payload 逐类型校验不在本 crate）。
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

/// TS `extractCommandId`：解析失败时尽力取 commandId（非字符串 → ""）。
pub fn extract_command_id(raw: &Value) -> String {
    raw.get("commandId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

// ── Host 回调面（TS `CommandInboxHost`；Option 方法 → 默认实现） ──

/// guard 裁决结果：allow / stale / reject / noop（镜 TS `GuardDecision`）。
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

/// inbox → host 的回调面。`default` 对应 TS 的可选成员（缺省放行/未命中）。
///
/// 约束（与 TS 单线程回调一致）：实现**不得重入** inbox 方法——回调在
/// inbox 状态锁内执行。
pub trait CommandInboxHost: Send + Sync {
    /// 会话当前 revision；未知会话返回 `None`（createSession 用 0，不经此回调）。
    fn get_revision(&self, session_id: &str) -> Option<i64>;

    /// 会话当前投影代际；CAS 先校验 epoch 再校验 revision。
    fn get_log_epoch(&self, session_id: &str) -> Option<String>;

    /// row-targeting 命令的 entity/action 同源 resolver 裁决。
    fn validate_row_target(&self, _envelope: &CommandEnvelope) -> GuardDecision {
        GuardDecision::Allow
    }

    /// 业务 guard（product-protocol guard id）。缺省一律放行。
    fn guard(&self, _envelope: &CommandEnvelope) -> GuardDecision {
        GuardDecision::Allow
    }

    /// 持久化回源，顺序即优先级：transcript → timeline → child → discarded。
    /// TS 返回 Promise 可 reject（→ `fault.command.queryUnavailable`）；Rust 侧
    /// 该回调不可失败，queryUnavailable 分支仅作为在途事实被 clearSession 清除时的兑底保留。
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
