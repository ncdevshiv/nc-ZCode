//! Channel RPC messages — a byte-for-byte port of
//! `packages/rpc/src/channelClient.ts` / `channelServer.ts` / `channels.shared.ts`.
//!
//! One channel message = `encode(header) + encode(body)`:
//! - request: `header = [type, id, channelName, methodName]`, `body = arg`
//! - cancel/unsubscribe: `header = [type, id]`, `body = undefined`
//! - response: `header = [type, id]`, `body = data` (Initialize is only `[200]` + undefined)
//!
//! The numeric values of `RequestType` 100–103 and `ResponseType` 200–204 are a wire contract and
//! must not be reordered. Decoding consumes only the two top-level values, exactly like TS; extra
//! trailing bytes are ignored (the behaviour of TS `BufferReader`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::serialization::{decode_value, encode_value, DecodeError, WireValue};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RequestType {
    Promise = 100,
    PromiseCancel = 101,
    EventListen = 102,
    EventDispose = 103,
}

impl RequestType {
    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            100 => Self::Promise,
            101 => Self::PromiseCancel,
            102 => Self::EventListen,
            103 => Self::EventDispose,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ResponseType {
    Initialize = 200,
    PromiseSuccess = 201,
    PromiseError = 202,
    PromiseErrorObj = 203,
    EventFire = 204,
}

impl ResponseType {
    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            200 => Self::Initialize,
            201 => Self::PromiseSuccess,
            202 => Self::PromiseError,
            203 => Self::PromiseErrorObj,
            204 => Self::EventFire,
            _ => return None,
        })
    }
}

/// The `PromiseError.data` payload of `IRawResponse` (TS `RpcErrorPayload`).
///
/// The field order matches the TS object literal (`stack` before `code`), because `serde_json`
/// runs with `preserve_order` and the JSON text it encodes is byte-identical to TS's.
/// `stack: undefined` is dropped by `JSON.stringify` in TS; here `Option::None` +
/// `skip_serializing_if` reproduces that.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RpcErrorPayload {
    pub message: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<Value>,
}

impl RpcErrorPayload {
    pub fn new(message: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            name: name.into(),
            ..Self::default()
        }
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChannelMessage {
    Promise {
        id: u32,
        channel: String,
        method: String,
        arg: WireValue,
    },
    PromiseCancel {
        id: u32,
    },
    EventListen {
        id: u32,
        channel: String,
        method: String,
        arg: WireValue,
    },
    EventDispose {
        id: u32,
    },
    Initialize,
    PromiseSuccess {
        id: u32,
        data: WireValue,
    },
    PromiseError {
        id: u32,
        data: Value,
    },
    PromiseErrorObj {
        id: u32,
        data: WireValue,
    },
    EventFire {
        id: u32,
        data: WireValue,
    },
}

impl ChannelMessage {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Self::Promise {
                id,
                channel,
                method,
                arg,
            } => {
                encode_target_header(RequestType::Promise, *id, channel, method, out);
                encode_value(arg, out);
            }
            Self::EventListen {
                id,
                channel,
                method,
                arg,
            } => {
                encode_target_header(RequestType::EventListen, *id, channel, method, out);
                encode_value(arg, out);
            }
            Self::PromiseCancel { id } => {
                encode_id_header(RequestType::PromiseCancel, *id, out);
                encode_value(&WireValue::Undefined, out);
            }
            Self::EventDispose { id } => {
                encode_id_header(RequestType::EventDispose, *id, out);
                encode_value(&WireValue::Undefined, out);
            }
            Self::Initialize => {
                encode_response_header(ResponseType::Initialize, None, out);
                encode_value(&WireValue::Undefined, out);
            }
            Self::PromiseSuccess { id, data } => {
                encode_response_header(ResponseType::PromiseSuccess, Some(*id), out);
                encode_value(data, out);
            }
            Self::PromiseError { id, data } => {
                encode_response_header(ResponseType::PromiseError, Some(*id), out);
                encode_value(&WireValue::Json(data.clone()), out);
            }
            Self::PromiseErrorObj { id, data } => {
                encode_response_header(ResponseType::PromiseErrorObj, Some(*id), out);
                encode_value(data, out);
            }
            Self::EventFire { id, data } => {
                encode_response_header(ResponseType::EventFire, Some(*id), out);
                encode_value(data, out);
            }
        }
    }

    /// Decode one channel message. Extra trailing bytes are ignored, exactly as in TS.
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = 0;
        let header = decode_value(bytes, &mut cursor)?;
        let body = decode_value(bytes, &mut cursor)?;
        Self::from_parts(header, body)
    }

    pub fn from_parts(header: WireValue, body: WireValue) -> Result<Self, DecodeError> {
        let WireValue::Array(items) = &header else {
            return Err(DecodeError::new("channel header must be an array"));
        };
        let Some(WireValue::Int(type_tag)) = items.first() else {
            return Err(DecodeError::new("channel header[0] must be an integer"));
        };
        let type_tag = *type_tag;
        let type_byte = u8::try_from(type_tag).ok();
        if let Some(response_type) = type_byte.and_then(ResponseType::from_u8) {
            return match response_type {
                ResponseType::Initialize => Ok(Self::Initialize),
                ResponseType::PromiseSuccess => {
                    let id = id_field(items)?;
                    Ok(Self::PromiseSuccess { id, data: body })
                }
                ResponseType::PromiseError => {
                    let id = id_field(items)?;
                    let WireValue::Json(data) = body else {
                        return Err(DecodeError::new("PromiseError body must be a JSON object"));
                    };
                    if !data.is_object() {
                        return Err(DecodeError::new("PromiseError body must be a JSON object"));
                    }
                    Ok(Self::PromiseError { id, data })
                }
                ResponseType::PromiseErrorObj => {
                    let id = id_field(items)?;
                    Ok(Self::PromiseErrorObj { id, data: body })
                }
                ResponseType::EventFire => {
                    let id = id_field(items)?;
                    Ok(Self::EventFire { id, data: body })
                }
            };
        }
        let request_type = type_byte
            .and_then(RequestType::from_u8)
            .ok_or_else(|| DecodeError::new(format!("unknown channel type: {type_tag}")))?;
        match request_type {
            RequestType::Promise => Ok(Self::Promise {
                id: id_field(items)?,
                channel: string_field(items, 2, "channelName")?,
                method: string_field(items, 3, "methodName")?,
                arg: body,
            }),
            RequestType::EventListen => Ok(Self::EventListen {
                id: id_field(items)?,
                channel: string_field(items, 2, "channelName")?,
                method: string_field(items, 3, "methodName")?,
                arg: body,
            }),
            RequestType::PromiseCancel => Ok(Self::PromiseCancel {
                id: id_field(items)?,
            }),
            RequestType::EventDispose => Ok(Self::EventDispose {
                id: id_field(items)?,
            }),
        }
    }
}

fn encode_target_header(
    request_type: RequestType,
    id: u32,
    channel: &str,
    method: &str,
    out: &mut Vec<u8>,
) {
    encode_value(
        &WireValue::Array(vec![
            WireValue::Int(request_type as u32),
            WireValue::Int(id),
            WireValue::String(channel.to_string()),
            WireValue::String(method.to_string()),
        ]),
        out,
    );
}

fn encode_id_header(request_type: RequestType, id: u32, out: &mut Vec<u8>) {
    encode_value(
        &WireValue::Array(vec![
            WireValue::Int(request_type as u32),
            WireValue::Int(id),
        ]),
        out,
    );
}

fn encode_response_header(response_type: ResponseType, id: Option<u32>, out: &mut Vec<u8>) {
    let header = match id {
        Some(id) => WireValue::Array(vec![
            WireValue::Int(response_type as u32),
            WireValue::Int(id),
        ]),
        None => WireValue::Array(vec![WireValue::Int(response_type as u32)]),
    };
    encode_value(&header, out);
}

fn id_field(items: &[WireValue]) -> Result<u32, DecodeError> {
    let Some(WireValue::Int(id)) = items.get(1) else {
        return Err(DecodeError::new("channel header[1] must be an integer id"));
    };
    Ok(*id)
}

fn string_field(items: &[WireValue], index: usize, label: &str) -> Result<String, DecodeError> {
    let Some(WireValue::String(value)) = items.get(index) else {
        return Err(DecodeError::new(format!(
            "channel header[{index}] ({label}) must be a string"
        )));
    };
    Ok(value.clone())
}

/// Client Promise request: `[100, id, channelName, methodName]` + arg.
pub fn encode_request(id: u32, channel: &str, method: &str, arg: &WireValue) -> Vec<u8> {
    ChannelMessage::Promise {
        id,
        channel: channel.to_string(),
        method: method.to_string(),
        arg: arg.clone(),
    }
    .encode()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decode_hex(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).expect("hex"))
            .collect()
    }

    fn encode_hex(message: &ChannelMessage) -> String {
        message
            .encode()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    #[test]
    fn promise_request_matches_the_typescript_header_layout() {
        let message = encode_request(1, "ch", "run", &WireValue::Json(json!({"x": 1})));
        // [4,4][int 100][int 1]["ch"]["run"] + {"x":1}
        assert_eq!(
            message,
            decode_hex("04040664060101026368010372756e05077b2278223a317d")
        );
    }

    #[test]
    fn initialize_is_one_element_header_plus_undefined() {
        assert_eq!(
            encode_hex(&ChannelMessage::Initialize),
            "040106c80100".to_string()
        );
    }

    #[test]
    fn messages_round_trip_through_decode() {
        let messages = [
            ChannelMessage::Promise {
                id: 7,
                channel: "golden".to_string(),
                method: "echo".to_string(),
                arg: WireValue::Json(json!({"a": 1})),
            },
            ChannelMessage::EventListen {
                id: 8,
                channel: "golden".to_string(),
                method: "tick".to_string(),
                arg: WireValue::Undefined,
            },
            ChannelMessage::PromiseCancel { id: 9 },
            ChannelMessage::EventDispose { id: 10 },
            ChannelMessage::Initialize,
            ChannelMessage::PromiseSuccess {
                id: 11,
                data: WireValue::Undefined,
            },
            ChannelMessage::PromiseError {
                id: 12,
                data: RpcErrorPayload::new("boom", "BoomError").to_value(),
            },
            ChannelMessage::PromiseErrorObj {
                id: 13,
                data: WireValue::String("not-an-error".to_string()),
            },
            ChannelMessage::EventFire {
                id: 14,
                data: WireValue::Array(vec![WireValue::Int(1)]),
            },
        ];
        for message in messages {
            let decoded = ChannelMessage::decode(&message.encode()).expect("decode");
            assert_eq!(decoded, message);
        }
    }

    #[test]
    fn error_payload_keeps_typescript_key_order_and_drops_unset_fields() {
        let mut payload = RpcErrorPayload::new("boom", "BoomError");
        payload.code = Some(json!("golden.error"));
        payload.data = Some(json!({"why": "because"}));
        let bytes = ChannelMessage::PromiseError {
            id: 3,
            data: payload.to_value(),
        }
        .encode();
        let text = String::from_utf8_lossy(&bytes).to_string();
        assert!(
            text.ends_with(
                "{\"message\":\"boom\",\"name\":\"BoomError\",\"code\":\"golden.error\",\"data\":{\"why\":\"because\"}}"
            ),
            "{text}"
        );
        assert!(!text.contains("stack"), "unset stack must be omitted");
    }

    #[test]
    fn malformed_headers_are_errors_not_panics() {
        for bytes in [
            decode_hex("04010664"),       // Promise without id/channel/method
            decode_hex("040206640601"),   // Promise missing channel/method
            decode_hex("040106ff0101"),   // unknown request type 255
            decode_hex("04020101640100"), // non-integer response id
            decode_hex("040106c9010401"), // PromiseError body is not JSON
            decode_hex("0100"),           // header is not an array
        ] {
            assert!(
                ChannelMessage::decode(&bytes).is_err(),
                "must reject {bytes:02x?}"
            );
        }
    }
}
