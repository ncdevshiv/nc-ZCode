//! v4 通道层 RPC 信封：stdio NDJSON 一行一帧。
//!
//! 方法名常量镜像 `packages/shared/src/zcode-protocol-v4/transport.ts` 的
//! `V4_METHODS` / `V4_NOTIFICATIONS`（specs/rust-native-crates.md S3 验收：本
//! increment 只实现其中列出的子集，未实现方法返回 -32601，绝不静默吞掉）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── 方法名（与 V4_METHODS / V4_NOTIFICATIONS 逐字对齐）──

pub const METHOD_CONNECTION_FLOW: &str = "v4/connection/flow";
pub const METHOD_CONVERSATION_SUBSCRIBE: &str = "v4/conversation/subscribe";
pub const METHOD_CONVERSATION_RESYNC: &str = "v4/conversation/resync";
pub const METHOD_CONVERSATION_UNSUBSCRIBE: &str = "v4/conversation/unsubscribe";
pub const METHOD_CONVERSATION_ROWS_RANGE: &str = "v4/conversation/rowsRange";
pub const METHOD_COMMAND: &str = "v4/command";

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
