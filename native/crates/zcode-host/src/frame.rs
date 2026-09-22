//! v4 wire 帧发射：topic 帧 → NDJSON 行的完整编码路径。
//!
//! 形状对齐 `packages/shared/src/zcode-protocol-v4/wire.ts` 的
//! `TopicWireFrame`（complete 分支）与 `transport.ts` 的
//! `conversationTopicFrameSchema`；params 即完整 wire 帧（schema strict，不附加
//! 任何自造键）。CRC/base64/VQL 等二进制纯函数在 `zcode-v4-wire`（S0 交付），本
//! 文件的集成点是信封计量闸门。
//!
//! 信封闸门：发射前用 `measure_topic_notification_envelope_bytes` 计量三种真实
//! 载体（CLI NDJSON / Channel socket / mobile relay）的最大字节数，超过
//! `MAX_FRAME_BYTES` 拒绝发射并返回 typed error——这是 TS 侧同一道闸门的移植。

use serde_json::{json, Value};
use zcode_v4_wire::{measure_topic_notification_envelope_bytes, V4_WIRE_PROTOCOL_VERSION};

use crate::rpc::{RpcNotification, NOTIFICATION_CONVERSATION_FRAME};

pub const DELIVERY_KIND_INITIAL: &str = "initial";
pub const DELIVERY_KIND_ONLINE: &str = "online";
pub const DELIVERY_KIND_RECOVERY: &str = "recovery";

#[derive(Debug)]
pub enum FrameError {
    /// 信封超过 `MAX_FRAME_BYTES`（三种载体的最大值）。
    EnvelopeTooLarge { measured: usize, max: usize },
    /// 内帧 JSON 序列化失败（调用方构造了非法 Value——内部错误）。
    Serialize(String),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EnvelopeTooLarge { measured, max } => {
                write!(f, "frame envelope {measured} bytes exceeds max {max}")
            }
            Self::Serialize(message) => write!(f, "frame serialization failed: {message}"),
        }
    }
}

/// 一帧的完整入参（调用方持有 seq 记账）。
#[derive(Debug, Clone)]
pub struct TopicFrameInput<'a> {
    pub topic: &'a str,
    pub subscription_id: &'a str,
    pub delivery_kind: &'a str,
    pub from_seq: u64,
    pub to_seq: u64,
    pub sent_at: i64,
    pub payload: &'a Value,
    pub logical_frame_id: String,
    pub logical_frame_ordinal: u64,
}

/// 内帧（`TopicFrame`）：topic/subscriptionId/区间记账/CLI 时钟/payload。
pub fn build_inner_frame(input: &TopicFrameInput<'_>) -> Result<Value, FrameError> {
    let payload = input.payload;
    let inner = json!({
        "topic": input.topic,
        "subscriptionId": input.subscription_id,
        "fromSeq": input.from_seq,
        "toSeq": input.to_seq,
        "sentAt": input.sent_at,
        "payload": payload,
    });
    // JSON.stringify 语义：键序即字面序；serde_json 默认保序（preserve_order feature
    // 在 workspace 依赖中开启，见 native/Cargo.toml）。
    Ok(inner)
}

/// wire 帧（complete 分支）：wireVersion/kind/deliveryKind/逻辑帧定位/topic/
/// subscriptionId/frame。
pub fn build_wire_frame(input: &TopicFrameInput<'_>) -> Result<Value, FrameError> {
    let inner = build_inner_frame(input)?;
    Ok(json!({
        "wireVersion": V4_WIRE_PROTOCOL_VERSION,
        "kind": "complete",
        "deliveryKind": input.delivery_kind,
        "logicalFrameId": input.logical_frame_id,
        "logicalFrameOrdinal": input.logical_frame_ordinal,
        "topic": input.topic,
        "subscriptionId": input.subscription_id,
        "frame": inner,
    }))
}

/// 编码为 stdout 的一行（`v4/conversation/frame` notification）。
/// 编码为 stdout 的一行（`v4/conversation/frame` notification）。
///
/// params 即完整 wire 帧（`TopicWireFrame` complete 分支，schema strict，不附加
/// 自造键）：wireVersion/kind/deliveryKind/逻辑帧定位/topic/subscriptionId/frame。
pub fn encode_frame_line(input: &TopicFrameInput<'_>) -> Result<String, FrameError> {
    let wire = build_wire_frame(input)?;
    let wire_json =
        serde_json::to_string(&wire).map_err(|error| FrameError::Serialize(error.to_string()))?;

    let measurement = measure_topic_notification_envelope_bytes(&wire_json);
    if measurement.max_bytes > zcode_v4_wire::MAX_FRAME_BYTES {
        return Err(FrameError::EnvelopeTooLarge {
            measured: measurement.max_bytes,
            max: zcode_v4_wire::MAX_FRAME_BYTES,
        });
    }

    Ok(RpcNotification {
        jsonrpc: "2.0",
        method: NOTIFICATION_CONVERSATION_FRAME.to_string(),
        params: wire,
    }
    .to_line())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_input(payload: &Value) -> TopicFrameInput<'_> {
        TopicFrameInput {
            topic: "conversation/s-1",
            subscription_id: "sub-1",
            delivery_kind: DELIVERY_KIND_ONLINE,
            from_seq: 3,
            to_seq: 7,
            sent_at: 1_700_000_000_000,
            payload,
            logical_frame_id: String::from("lf-1"),
            logical_frame_ordinal: 1,
        }
    }

    #[test]
    fn frame_line_is_valid_ndjson_with_v4_method() {
        let payload = json!({"kind": "deltas", "deltas": []});
        let line = encode_frame_line(&sample_input(&payload)).expect("small frame must encode");
        let parsed: Value = serde_json::from_str(&line).expect("must be one JSON line");
        assert_eq!(parsed["method"], json!("v4/conversation/frame"));
        // params 即完整 wire 帧（complete 分支，schema strict 的键集合）。
        let params = &parsed["params"];
        assert_eq!(params["wireVersion"], json!(3));
        assert_eq!(params["kind"], json!("complete"));
        assert_eq!(params["deliveryKind"], json!("online"));
        assert_eq!(params["logicalFrameId"], json!("lf-1"));
        assert_eq!(params["logicalFrameOrdinal"], json!(1));
        assert_eq!(params["topic"], json!("conversation/s-1"));
        assert_eq!(params["subscriptionId"], json!("sub-1"));
        assert_eq!(params["frame"]["fromSeq"], json!(3));
        assert_eq!(params["frame"]["toSeq"], json!(7));
        assert_eq!(params["frame"]["sentAt"], json!(1_700_000_000_000_i64));
        assert_eq!(params["frame"]["payload"]["kind"], json!("deltas"));
        // strict schema：不多不少。
        let keys: Vec<&str> = params
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys.len(), 8, "wire frame key set changed: {keys:?}");
    }

    #[test]
    fn oversize_frame_is_refused_with_typed_error() {
        let huge = "x".repeat(4 * 1024 * 1024);
        let payload = json!({"kind": "deltas", "deltas": [{"op":"row.delta","rowId":1,"path":"text","append": huge}]});
        let error = encode_frame_line(&sample_input(&payload)).expect_err("must refuse");
        match error {
            FrameError::EnvelopeTooLarge { measured, max } => {
                assert!(measured > max);
            }
            other => panic!("unexpected error: {other}"),
        }
    }
}
