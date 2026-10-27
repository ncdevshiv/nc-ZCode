//! v4 通道层 RPC 信封：stdio NDJSON 一行一帧。
//!
//! 方法名常量镜像 `packages/shared/src/zcode-protocol-v4/transport.ts` 的
//! `V4_METHODS` / `V4_NOTIFICATIONS`（specs/rust-native-crates.md S3 验收：本
//! increment 只实现其中列出的子集，未实现方法返回 -32601，绝不静默吞掉）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── 方法名（与 V4_METHODS / V4_NOTIFICATIONS 逐字对齐）──

pub const METHOD_CONNECTION_FLOW: &str = "v4/connection/flow";

/// v4 RPC 方法全集（`packages/shared/src/zcode-protocol-v4/transport.ts` 的
/// `V4_METHODS` values，含通知方法 `v4/conversation/frame`）。与命令清单同一纪律：
/// "还剩哪些方法没接线"必须是可测试的事实，而不是 README 里的一句口头承诺。
pub const V4_METHODS_ALL: &[&str] = &[
    "v4/attachment/abort",
    "v4/attachment/begin",
    "v4/attachment/chunk",
    "v4/attachment/commit",
    "v4/attachment/previewSource",
    "v4/attachment/read",
    "v4/command",
    "v4/commands/query",
    "v4/connection/flow",
    "v4/controller/resync",
    "v4/controller/subscribe",
    "v4/controller/unsubscribe",
    "v4/conversation/attachmentRead",
    "v4/conversation/attachmentStat",
    "v4/conversation/backgroundBashOutput",
    "v4/conversation/fileChanges",
    "v4/conversation/fileRewindPreview",
    "v4/conversation/frame",
    "v4/conversation/plans",
    "v4/conversation/resync",
    "v4/conversation/rowsRange",
    "v4/conversation/subscribe",
    "v4/conversation/unsubscribe",
    "v4/conversation/usage",
    "v4/conversation/workflowRunArtifactData",
    "v4/conversation/workflowRunArtifactRead",
    "v4/conversation/workflowRunArtifacts",
    "v4/conversation/workflowRunEvents",
    "v4/conversation/workflowRunNodeResult",
    "v4/conversation/workflowRunWorkspace",
    "v4/conversation/workflowRuns",
    "v4/telemetry/event",
    "v4/usage/stats",
];

/// 尚未接线的 v4 方法 —— 每条写明**缺哪个子系统**。和命令清单一样，这张表不是注释：
/// 接线一个方法时同步删除条目，`is_implemented_method` 与测试据此保持一致。
pub const V4_METHODS_UNIMPLEMENTED: &[(&str, &str)] = &[
    ("v4/attachment/abort", "附件事务（分块上传/预览）子系统"),
    ("v4/attachment/begin", "附件事务（分块上传/预览）子系统"),
    ("v4/attachment/chunk", "附件事务（分块上传/预览）子系统"),
    ("v4/attachment/commit", "附件事务（分块上传/预览）子系统"),
    (
        "v4/attachment/previewSource",
        "附件事务（分块上传/预览）子系统",
    ),
    ("v4/attachment/read", "附件事务（分块上传/预览）子系统"),
    (
        "v4/conversation/attachmentRead",
        "附件子系统（row/index 授权读取）",
    ),
    (
        "v4/conversation/attachmentStat",
        "附件子系统（row/index 授权读取）",
    ),
    (
        "v4/conversation/backgroundBashOutput",
        "后台 bash 会话（bash 工具一次性执行）",
    ),
    (
        "v4/conversation/plans",
        "ExitPlanMode 工具行（本 host 无 plan 工具）",
    ),
    (
        "v4/controller/resync",
        "controller facade 方法别名（桌面尚未调用）",
    ),
    (
        "v4/controller/subscribe",
        "controller facade 方法别名（桌面尚未调用）",
    ),
    (
        "v4/controller/unsubscribe",
        "controller facade 方法别名（桌面尚未调用）",
    ),
    (
        "v4/conversation/workflowRunArtifactData",
        "dynamic workflow run 引擎（决策见 specs/workflow-capability-boundary.md）",
    ),
    (
        "v4/conversation/workflowRunArtifactRead",
        "dynamic workflow run 引擎（决策见 specs/workflow-capability-boundary.md）",
    ),
    (
        "v4/conversation/workflowRunArtifacts",
        "dynamic workflow run 引擎（决策见 specs/workflow-capability-boundary.md）",
    ),
    (
        "v4/conversation/workflowRunEvents",
        "dynamic workflow run 引擎（决策见 specs/workflow-capability-boundary.md）",
    ),
    (
        "v4/conversation/workflowRunNodeResult",
        "dynamic workflow run 引擎（决策见 specs/workflow-capability-boundary.md）",
    ),
    (
        "v4/conversation/workflowRunWorkspace",
        "dynamic workflow run 引擎（决策见 specs/workflow-capability-boundary.md）",
    ),
    (
        "v4/conversation/workflowRuns",
        "dynamic workflow run 引擎（决策见 specs/workflow-capability-boundary.md）",
    ),
    ("v4/telemetry/event", "telemetry 上报通道"),
    (
        "v4/usage/stats",
        "durable usage 聚合库（本 host 只有会话级内存用量）",
    ),
];

pub fn is_implemented_method(method: &str) -> bool {
    V4_METHODS_ALL.contains(&method)
        && !V4_METHODS_UNIMPLEMENTED
            .iter()
            .any(|(name, _)| *name == method)
}
pub const METHOD_CONVERSATION_SUBSCRIBE: &str = "v4/conversation/subscribe";
pub const METHOD_CONVERSATION_RESYNC: &str = "v4/conversation/resync";
pub const METHOD_CONVERSATION_UNSUBSCRIBE: &str = "v4/conversation/unsubscribe";
pub const METHOD_CONVERSATION_ROWS_RANGE: &str = "v4/conversation/rowsRange";
pub const METHOD_COMMAND: &str = "v4/command";
pub const METHOD_COMMANDS_QUERY: &str = "v4/commands/query";

pub const NOTIFICATION_CONVERSATION_FRAME: &str = "v4/conversation/frame";

// ── JSON-RPC 形状 ──

#[derive(Debug, Clone, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct RpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

pub const CODE_INVALID_REQUEST: i64 = -32600;
pub const CODE_METHOD_NOT_FOUND: i64 = -32601;
pub const CODE_INVALID_PARAMS: i64 = -32602;
pub const CODE_INTERNAL: i64 = -32603;

impl RpcResponse {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, error: RpcError) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(error),
        }
    }

    pub fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            // 自产结构不可能序列化失败；兜底也必须是合法 JSON 行。
            String::from(r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"response serialization failed"}}"#)
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RpcNotification {
    pub jsonrpc: &'static str,
    pub method: String,
    pub params: Value,
}

impl RpcNotification {
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| String::from("{}"))
    }
}

pub fn parse_request_line(line: &str) -> Result<RpcRequest, RpcError> {
    let value: Value = serde_json::from_str(line)
        .map_err(|error| RpcError::new(CODE_INVALID_REQUEST, format!("invalid JSON: {error}")))?;
    let request: RpcRequest = serde_json::from_value(value).map_err(|error| {
        RpcError::new(
            CODE_INVALID_REQUEST,
            format!("invalid request envelope: {error}"),
        )
    })?;
    if request.jsonrpc != "2.0" {
        return Err(RpcError::new(
            CODE_INVALID_REQUEST,
            "jsonrpc must be \"2.0\"",
        ));
    }
    Ok(request)
}

#[cfg(test)]
mod method_inventory_tests {
    use super::*;
    use std::collections::BTreeSet;

    /// 方法清单必须是全集的**精确补集**（同命令清单的纪律）。
    #[test]
    fn method_inventory_is_the_exact_complement_of_the_v4_surface() {
        let unimplemented: BTreeSet<&str> = V4_METHODS_UNIMPLEMENTED
            .iter()
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(
            unimplemented.len(),
            V4_METHODS_UNIMPLEMENTED.len(),
            "duplicate entry in V4_METHODS_UNIMPLEMENTED"
        );
        let all: BTreeSet<&str> = V4_METHODS_ALL.iter().copied().collect();
        assert_eq!(
            all.len(),
            V4_METHODS_ALL.len(),
            "duplicate method in V4_METHODS_ALL"
        );
        for name in &unimplemented {
            assert!(all.contains(name), "{name} is not a known v4 method");
            let reason = V4_METHODS_UNIMPLEMENTED
                .iter()
                .find(|(entry, _)| entry == name)
                .map(|(_, reason)| *reason)
                .unwrap_or_default();
            assert!(!reason.is_empty(), "{name} is unrouted without a reason");
            assert!(
                !is_implemented_method(name),
                "{name} is documented as unimplemented but reports implemented"
            );
        }
        for name in &all {
            if !unimplemented.contains(name) {
                assert!(
                    is_implemented_method(name),
                    "{name} is neither implemented nor documented as missing"
                );
            }
        }
    }
}
