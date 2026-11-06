//! v4 é€šé“å±‚ RPC ä¿¡å°ï¼šstdio NDJSON ä¸€è¡Œä¸€å¸§ã€‚
//!
//! æ–¹æ³•åå¸¸é‡é•œåƒ `packages/shared/src/zcode-protocol-v4/transport.ts` çš„
//! `V4_METHODS` / `V4_NOTIFICATIONS`ï¼ˆspecs/rust-native-crates.md S3 éªŒæ”¶ï¼šæœ¬
//! increment åªå®žçŽ°å…¶ä¸­åˆ—å‡ºçš„å­é›†ï¼Œæœªå®žçŽ°æ–¹æ³•è¿”å›ž -32601ï¼Œç»ä¸é™é»˜åžæŽ‰ï¼‰ã€‚

use serde::{Deserialize, Serialize};
use serde_json::Value;

// â”€â”€ æ–¹æ³•åï¼ˆä¸Ž V4_METHODS / V4_NOTIFICATIONS é€å­—å¯¹é½ï¼‰â”€â”€

pub const METHOD_CONNECTION_FLOW: &str = "v4/connection/flow";

/// v4 RPC request methods (matches `packages/shared/src/zcode-protocol-v4/transport.ts` `V4_METHODS` values).
/// Notifications (`v4/conversation/frame`, `v4/telemetry/local-ttft`, `v4/cua/permission-observation`,
/// `v4/telemetry/event`) are intentionally excluded from this request-method inventory.
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
    "v4/usage/stats",
];

/// å°šæœªæŽ¥çº¿çš„ v4 æ–¹æ³• â€”â€” æ¯æ¡å†™æ˜Ž**ç¼ºå“ªä¸ªå­ç³»ç»Ÿ**ã€‚å’Œå‘½ä»¤æ¸…å•ä¸€æ ·ï¼Œè¿™å¼ è¡¨ä¸æ˜¯æ³¨é‡Šï¼š
/// æŽ¥çº¿ä¸€ä¸ªæ–¹æ³•æ—¶åŒæ­¥åˆ é™¤æ¡ç›®ï¼Œ`is_implemented_method` ä¸Žæµ‹è¯•æ®æ­¤ä¿æŒä¸€è‡´ã€‚
/// v4 request methods present in TS V4_METHODS but intentionally not wired (or infeasible) in this host.
/// Each entry must have an accurate, evidence-backed reason. Empty when the stdio
/// surface is complete; the inventory test pins the exact complement.
pub const V4_METHODS_UNIMPLEMENTED: &[(&str, &str)] = &[];
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
pub const METHOD_CONTROLLER_SUBSCRIBE: &str = "v4/controller/subscribe";
pub const METHOD_CONTROLLER_RESYNC: &str = "v4/controller/resync";
pub const METHOD_CONTROLLER_UNSUBSCRIBE: &str = "v4/controller/unsubscribe";
pub const METHOD_CONVERSATION_ATTACHMENT_READ: &str = "v4/conversation/attachmentRead";
pub const METHOD_CONVERSATION_ATTACHMENT_STAT: &str = "v4/conversation/attachmentStat";

pub const NOTIFICATION_CONVERSATION_FRAME: &str = "v4/conversation/frame";
pub const NOTIFICATION_CONTROLLER_FRAME: &str = "v4/controller/frame";
pub const NOTIFICATION_CONVERSATION_TELEMETRY_FACT: &str = "v4/telemetry/event";
pub const NOTIFICATION_LOCAL_TTFT_FACTS: &str = "v4/telemetry/local-ttft";
pub const NOTIFICATION_CUA_PERMISSION_OBSERVATION: &str = "v4/cua/permission-observation";

// â”€â”€ JSON-RPC å½¢çŠ¶ â”€â”€

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

impl std::fmt::Display for RpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "rpc error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

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
            // è‡ªäº§ç»“æž„ä¸å¯èƒ½åºåˆ—åŒ–å¤±è´¥ï¼›å…œåº•ä¹Ÿå¿…é¡»æ˜¯åˆæ³• JSON è¡Œã€‚
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

    /// æ–¹æ³•æ¸…å•å¿…é¡»æ˜¯å…¨é›†çš„**ç²¾ç¡®è¡¥é›†**ï¼ˆåŒå‘½ä»¤æ¸…å•çš„çºªå¾‹ï¼‰ã€‚
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
