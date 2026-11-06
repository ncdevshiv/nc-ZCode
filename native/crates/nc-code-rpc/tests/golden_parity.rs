//! Cross-language parity: every vector in `golden.json` is produced by the **real**
//! TypeScript implementation (`packages/rpc/src`) via `generate_golden.mjs` and must be
//! reproduced byte-for-byte by this crate. Regenerating the file is the only way to
//! change the wire format — that is the point.

use nc_code_rpc::channel::RpcErrorPayload;
use nc_code_rpc::protocol::{
    encode_frame, encode_frame_with, Frame, FrameDecoder, HEADER_SIZE, PROTOCOL_MESSAGE_REGULAR,
};
use nc_code_rpc::serialization::nested_uint8array_json;
use nc_code_rpc::{ChannelMessage, WireValue};
use serde_json::{json, Value};

const GOLDEN: &str = include_str!("golden.json");

fn golden() -> Value {
    serde_json::from_str(GOLDEN).expect("golden.json must be valid JSON")
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn from_hex(text: &str) -> Vec<u8> {
    assert!(
        text.len().is_multiple_of(2),
        "hex string must have an even length"
    );
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("valid hex"))
        .collect()
}

/// TS `serialize` dispatch for a value parsed out of JSON: integers become `Int`,
/// strings `String`, arrays `Array`, objects the JSON fallback.
fn wire_from_json(value: &Value) -> WireValue {
    match value {
        Value::String(text) => WireValue::String(text.clone()),
        Value::Number(number) => match number.as_i64() {
            Some(integer) if i32::try_from(integer).is_ok() => {
                WireValue::Int(integer as i32 as u32)
            }
            _ => WireValue::Json(value.clone()),
        },
        Value::Array(items) => WireValue::Array(items.iter().map(wire_from_json).collect()),
        other => WireValue::Json(other.clone()),
    }
}

#[test]
fn serialization_vectors_match_the_typescript_wire() {
    let golden = golden();
    let vectors = golden["serialization"]
        .as_array()
        .expect("serialization array");
    assert_eq!(vectors.len(), 17, "golden file changed; review the diff");
    for vector in vectors {
        let kind = vector["kind"].as_str().expect("kind");
        let expected = vector["serialized"].as_str().expect("serialized");
        let value = match kind {
            "undefined" => WireValue::Undefined,
            "string" => {
                WireValue::String(vector["value"].as_str().expect("string value").to_string())
            }
            "int" => {
                let number = vector["value"].as_i64().expect("int value");
                WireValue::Int(number as i32 as u32)
            }
            "float" | "object" => WireValue::Json(vector["value"].clone()),
            "nested-buffer" => {
                // Node Buffer.toJSON wins over the replacer: `{type,data}` JSON shape.
                let bytes = from_hex(vector["hex"].as_str().expect("hex"));
                WireValue::Json(json!({"payload": {"type": "Buffer", "data": bytes}}))
            }
            "array" => wire_from_json(&vector["value"]),
            "bytes" => WireValue::VsBuffer(from_hex(vector["hex"].as_str().expect("hex"))),
            "uint8array" => WireValue::Bytes(from_hex(vector["hex"].as_str().expect("hex"))),
            "nested-uint8array" => WireValue::Json(json!({
                "payload": nested_uint8array_json(&from_hex(
                    vector["hex"].as_str().expect("hex"),
                )),
            })),
            other => panic!("unexpected serialization vector kind: {other}"),
        };
        let mut encoded = Vec::new();
        nc_code_rpc::encode_value(&value, &mut encoded);
        assert_eq!(to_hex(&encoded), expected, "serialization vector {kind}");

        let mut cursor = 0;
        let decoded =
            nc_code_rpc::decode_value(&encoded, &mut cursor).expect("encoded bytes must decode");
        assert_eq!(cursor, encoded.len(), "decoder must consume the vector");
        assert_eq!(decoded, value, "round trip for {kind}");
    }
}

#[test]
fn frame_vectors_match_the_typescript_wire() {
    let golden = golden();
    let vectors = golden["frames"].as_array().expect("frames array");
    assert_eq!(vectors.len(), 4);
    for vector in vectors {
        let payload_hex = vector["payload"].as_str().expect("payload");
        let expected = vector["frame"].as_str().expect("frame");
        let payload = from_hex(payload_hex);
        let frame = encode_frame(&payload);
        assert_eq!(to_hex(&frame), expected, "frame for payload {payload_hex}");
        assert_eq!(frame.len(), HEADER_SIZE + payload.len());

        // Byte-at-a-time fragmentation must reconstruct exactly one frame.
        let mut decoder = FrameDecoder::default();
        let mut frames = Vec::new();
        for byte in &frame {
            frames.extend(
                decoder
                    .push(std::slice::from_ref(byte))
                    .expect("fragment must decode"),
            );
        }
        assert_eq!(frames.len(), 1, "payload {payload_hex}");
        assert_eq!(
            frames[0],
            Frame {
                message_type: PROTOCOL_MESSAGE_REGULAR,
                id: 0,
                ack: 0,
                payload,
            }
        );
    }
    // A non-zero id/ack frame also round-trips (PersistentProtocol uses them).
    let framed = encode_frame_with(2, 0x0102_0304, 0x0506_0708, b"x");
    let frames = FrameDecoder::default().push(&framed).expect("decode");
    assert_eq!(frames[0].message_type, 2);
    assert_eq!(frames[0].id, 0x0102_0304);
    assert_eq!(frames[0].ack, 0x0506_0708);
}

#[test]
fn channel_vectors_match_the_typescript_client_and_server() {
    let golden = golden();
    let vectors = golden["channel"].as_array().expect("channel array");
    assert_eq!(
        vectors.len(),
        14,
        "golden channel traffic changed; review the diff"
    );
    for vector in vectors {
        let label = vector["label"].as_str().expect("label");
        let expected = vector["bytes"].as_str().expect("bytes");
        let message = match label {
            "initialize" => ChannelMessage::Initialize,
            "promise.request" => ChannelMessage::Promise {
                id: 0,
                channel: "golden".to_string(),
                method: "echo".to_string(),
                arg: WireValue::Json(json!({"a": 1})),
            },
            "promise.success" => ChannelMessage::PromiseSuccess {
                id: 0,
                data: WireValue::Json(json!({"echoed": {"a": 1}})),
            },
            "promise.undefined.request" => ChannelMessage::Promise {
                id: 1,
                channel: "golden".to_string(),
                method: "undefinedResult".to_string(),
                arg: WireValue::Undefined,
            },
            "promise.undefined.success" => ChannelMessage::PromiseSuccess {
                id: 1,
                data: WireValue::Undefined,
            },
            "promise.error.request" => ChannelMessage::Promise {
                id: 2,
                channel: "golden".to_string(),
                method: "error".to_string(),
                arg: WireValue::Undefined,
            },
            "promise.error.response" => {
                let mut payload = RpcErrorPayload::new("boom", "BoomError");
                payload.code = Some(json!("golden.error"));
                payload.data = Some(json!({"why": "because"}));
                ChannelMessage::PromiseError {
                    id: 2,
                    data: payload.to_value(),
                }
            }
            "promise.errorObj.request" => ChannelMessage::Promise {
                id: 3,
                channel: "golden".to_string(),
                method: "errorObj".to_string(),
                arg: WireValue::Undefined,
            },
            "promise.errorObj.response" => ChannelMessage::PromiseErrorObj {
                id: 3,
                data: WireValue::Json(json!({"custom": true, "n": 7})),
            },
            "promise.cancel.request" => ChannelMessage::Promise {
                id: 4,
                channel: "golden".to_string(),
                method: "never".to_string(),
                arg: WireValue::Json(json!({"wait": true})),
            },
            "promise.cancel.cancel" => ChannelMessage::PromiseCancel { id: 4 },
            "event.listen.request" => ChannelMessage::EventListen {
                id: 5,
                channel: "golden".to_string(),
                method: "tick".to_string(),
                arg: WireValue::Json(json!({"topic": "t"})),
            },
            "event.fire.response" => ChannelMessage::EventFire {
                id: 5,
                data: WireValue::Json(json!({"n": 1})),
            },
            "event.dispose.request" => ChannelMessage::EventDispose { id: 5 },
            other => panic!("unexpected channel vector label: {other}"),
        };
        let encoded = message.encode();
        assert_eq!(to_hex(&encoded), expected, "channel vector {label}");
        let decoded = ChannelMessage::decode(&from_hex(expected)).expect("decode vector");
        assert_eq!(decoded, message, "channel decode for {label}");
    }
}

#[test]
fn channel_wire_decode_ignores_trailing_bytes_like_typescript() {
    let message = ChannelMessage::Promise {
        id: 9,
        channel: "golden".to_string(),
        method: "echo".to_string(),
        arg: WireValue::Undefined,
    };
    let mut bytes = message.encode();
    bytes.extend_from_slice(&[0xde, 0xad]);
    assert_eq!(ChannelMessage::decode(&bytes).unwrap(), message);
}
