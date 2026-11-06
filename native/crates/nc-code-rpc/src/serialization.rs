//! Binary serialization — a byte-for-byte port of `packages/rpc/src/serialization.ts`.
//!
//! Format: `[1 byte type tag][VQL-encoded length][data]`; each channel message =
//! `encode(header) + encode(body)`, where the header is usually `[RequestType, id, channel, method]`.
//!
//! Type tags (`DataType`):
//! - 0 Undefined
//! - 1 String (UTF-8)
//! - 2 Buffer (bytes, deserialized into a `Uint8Array`) → [`WireValue::Bytes`]
//! - 3 VSBuffer (bytes, deserialized into a `VSBuffer`) → [`WireValue::VsBuffer`]
//! - 4 Array (recursed element by element)
//! - 5 Object (**JSON text**; nested binary is marked with `__zcode_rpc_nested_uint8array_v1`)
//! - 6 Int (VQL-encoded unsigned 32-bit)
//!
//! Two quirks that must be copied verbatim (pinned by the golden vectors):
//! 1. Negative integers are first turned into u32 using JS `>>>` semantics and then VQL-encoded
//!    (-7 → `f9ffffff0f`); the peer decodes that as 4294967289 — TS itself never round-trips
//!    negative numbers, so the Rust side does not "fix" it.
//! 2. Non-integer numbers go through the JSON fallback (1.5 → `05 03 312e35`); no float tag is introduced.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum WireValue {
    Undefined,
    String(String),
    /// `DataType.Buffer` (tag 2): the TS client decodes it back into a **Uint8Array**.
    /// A `Uint8Array` in a service return value (e.g. `file.readFileRange`) must use this variant.
    Bytes(Vec<u8>),
    /// `DataType.VSBuffer` (tag 3): the TS client decodes it back into a **VSBuffer**.
    /// Used by the internal RPC transport; do not use it when a service returns a Uint8Array,
    /// otherwise the caller receives a VSBuffer.
    VsBuffer(Vec<u8>),
    Int(u32),
    Array(Vec<WireValue>),
    /// The parsed JSON text of a DataType.Object. Nested
    /// `{"__zcode_rpc_nested_uint8array_v1": true, "base64": "..."}` is kept as-is
    /// (lossless and forwardable; a decoder needing the bytes within the same process reads base64).
    Json(Value),
}

impl WireValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            WireValue::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<u32> {
        match self {
            WireValue::Int(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_json(&self) -> Option<&Value> {
        match self {
            WireValue::Json(value) => Some(value),
            _ => None,
        }
    }
}

pub fn encode_value(value: &WireValue, out: &mut Vec<u8>) {
    match value {
        WireValue::Undefined => out.push(0),
        WireValue::String(text) => {
            out.push(1);
            write_vql(out, text.len() as u32);
            out.extend_from_slice(text.as_bytes());
        }
        WireValue::Bytes(bytes) => {
            out.push(2);
            write_vql(out, bytes.len() as u32);
            out.extend_from_slice(bytes);
        }
        WireValue::VsBuffer(bytes) => {
            out.push(3);
            write_vql(out, bytes.len() as u32);
            out.extend_from_slice(bytes);
        }
        WireValue::Int(number) => {
            out.push(6);
            write_vql(out, *number);
        }
        WireValue::Array(items) => {
            out.push(4);
            write_vql(out, items.len() as u32);
            for item in items {
                encode_value(item, out);
            }
        }
        WireValue::Json(value) => {
            let text = serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
            out.push(5);
            write_vql(out, text.len() as u32);
            out.extend_from_slice(text.as_bytes());
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError {
    pub message: String,
}

impl DecodeError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub fn decode_value(input: &[u8], cursor: &mut usize) -> Result<WireValue, DecodeError> {
    let Some(&tag) = input.get(*cursor) else {
        return Err(DecodeError::new("unexpected end of buffer (type tag)"));
    };
    *cursor += 1;
    match tag {
        0 => Ok(WireValue::Undefined),
        1 => {
            let length = read_vql(input, cursor)? as usize;
            let bytes = take(input, cursor, length)?;
            String::from_utf8(bytes.to_vec())
                .map(WireValue::String)
                .map_err(|error| DecodeError::new(format!("invalid utf-8 string: {error}")))
        }
        2 => {
            let length = read_vql(input, cursor)? as usize;
            Ok(WireValue::Bytes(take(input, cursor, length)?.to_vec()))
        }
        3 => {
            let length = read_vql(input, cursor)? as usize;
            Ok(WireValue::VsBuffer(take(input, cursor, length)?.to_vec()))
        }
        4 => {
            let length = read_vql(input, cursor)? as usize;
            let mut items = Vec::with_capacity(length.min(1024));
            for _ in 0..length {
                items.push(decode_value(input, cursor)?);
            }
            Ok(WireValue::Array(items))
        }
        5 => {
            let length = read_vql(input, cursor)? as usize;
            let bytes = take(input, cursor, length)?;
            let text = std::str::from_utf8(bytes)
                .map_err(|error| DecodeError::new(format!("invalid utf-8 json: {error}")))?;
            serde_json::from_str(text)
                .map(WireValue::Json)
                .map_err(|error| DecodeError::new(format!("invalid json object: {error}")))
        }
        6 => Ok(WireValue::Int(read_vql(input, cursor)?)),
        other => Err(DecodeError::new(format!("unknown data type tag: {other}"))),
    }
}

fn take<'a>(input: &'a [u8], cursor: &mut usize, length: usize) -> Result<&'a [u8], DecodeError> {
    let end = cursor
        .checked_add(length)
        .ok_or_else(|| DecodeError::new("length overflow"))?;
    if end > input.len() {
        return Err(DecodeError::new("unexpected end of buffer (payload)"));
    }
    let slice = &input[*cursor..end];
    *cursor = end;
    Ok(slice)
}

/// VQL (7 bits per byte, high bit = more bytes follow), matching TS `writeInt32VQL`:
/// 0 → `00`, 127 → `7f`, 128 → `8001`, u32::MAX → `ffffffff0f`.
pub fn write_vql(out: &mut Vec<u8>, value: u32) {
    if value == 0 {
        out.push(0);
        return;
    }
    let mut value = value;
    while value != 0 {
        let mut byte = (value & 0b0111_1111) as u8;
        value >>= 7;
        if value > 0 {
            byte |= 0b1000_0000;
        }
        out.push(byte);
    }
}

pub fn read_vql(input: &[u8], cursor: &mut usize) -> Result<u32, DecodeError> {
    let mut value: u64 = 0;
    let mut shift = 0u32;
    loop {
        let Some(&byte) = input.get(*cursor) else {
            return Err(DecodeError::new("unexpected end of buffer (vql)"));
        };
        *cursor += 1;
        value |= u64::from(byte & 0b0111_1111) << shift;
        if byte & 0b1000_0000 == 0 {
            if value > u64::from(u32::MAX) {
                return Err(DecodeError::new("vql value exceeds u32"));
            }
            return Ok(value as u32);
        }
        shift += 7;
        if shift >= 35 {
            return Err(DecodeError::new("vql too long"));
        }
    }
}

/// The marker key for a nested `Uint8Array` inside the Object JSON fallback (TS
/// `RPC_NESTED_UINT8_ARRAY_MARKER`). `WireValue::Json` performs no automatic restore: a decoder
/// calls [`nested_uint8array_bytes`] when it needs the bytes, and an encoder uses
/// [`nested_uint8array_json`] to produce the same marker object as TS.
pub const RPC_NESTED_UINT8_ARRAY_MARKER: &str = "__zcode_rpc_nested_uint8array_v1";
pub const RPC_NESTED_UINT8_ARRAY_BASE64_KEY: &str = "base64";

/// Build the marker object of TS `encodeRpcJsonValue`: `{marker: true, base64: "..."}`.
pub fn nested_uint8array_json(bytes: &[u8]) -> Value {
    let mut object = serde_json::Map::new();
    object.insert(RPC_NESTED_UINT8_ARRAY_MARKER.to_string(), Value::Bool(true));
    object.insert(
        RPC_NESTED_UINT8_ARRAY_BASE64_KEY.to_string(),
        Value::String(base64_encode(bytes)),
    );
    Value::Object(object)
}

/// Recognize and restore the TS nested-byte marker; returns `None` when the shape does not match
/// or the base64 is invalid. Consistent with TS `isRpcEncodedUint8Array`: the object must have
/// **exactly** two keys.
pub fn nested_uint8array_bytes(value: &Value) -> Option<Vec<u8>> {
    let object = value.as_object()?;
    if object.len() != 2 {
        return None;
    }
    if object.get(RPC_NESTED_UINT8_ARRAY_MARKER)? != &Value::Bool(true) {
        return None;
    }
    let encoded = object.get(RPC_NESTED_UINT8_ARRAY_BASE64_KEY)?.as_str()?;
    base64_decode(encoded)
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map(u32::from);
        let b2 = chunk.get(2).copied().map(u32::from);
        out.push(BASE64_ALPHABET[(b0 >> 2) as usize] as char);
        out.push(BASE64_ALPHABET[(((b0 & 0b11) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
        match b1 {
            Some(b1) => {
                out.push(
                    BASE64_ALPHABET[(((b1 & 0b1111) << 2) | (b2.unwrap_or(0) >> 6)) as usize]
                        as char,
                );
                if b2.is_some() {
                    out.push(BASE64_ALPHABET[(b2.unwrap_or(0) & 0b11_1111) as usize] as char);
                } else {
                    out.push('=');
                }
            }
            None => out.push_str("=="),
        }
    }
    out
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, chunk) in bytes.chunks(4).enumerate() {
        let last = index == bytes.len() / 4 - 1;
        let padding = chunk.iter().rev().take_while(|byte| **byte == b'=').count();
        if padding > 2 || (padding > 0 && !last) {
            return None;
        }
        let mut values = [0u32; 4];
        for (position, byte) in chunk.iter().enumerate() {
            if *byte == b'=' {
                if position < 4 - padding {
                    return None;
                }
                values[position] = 0;
                continue;
            }
            let decoded = BASE64_ALPHABET
                .iter()
                .position(|candidate| candidate == byte)?;
            values[position] = decoded as u32;
        }
        let combined = (values[0] << 18) | (values[1] << 12) | (values[2] << 6) | values[3];
        out.push((combined >> 16) as u8);
        if padding < 2 {
            out.push((combined >> 8) as u8);
        }
        if padding < 1 {
            out.push(combined as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn encode(value: &WireValue) -> Vec<u8> {
        let mut out = Vec::new();
        encode_value(value, &mut out);
        out
    }

    fn decode(input: &[u8]) -> WireValue {
        let mut cursor = 0;
        let value = decode_value(input, &mut cursor).expect("decode");
        assert_eq!(cursor, input.len(), "decoder must consume the whole buffer");
        value
    }

    #[test]
    fn vql_matches_the_typescript_encoding() {
        let mut out = Vec::new();
        write_vql(&mut out, 0);
        write_vql(&mut out, 127);
        write_vql(&mut out, 128);
        write_vql(&mut out, u32::MAX);
        assert_eq!(
            out,
            vec![0x00, 0x7f, 0x80, 0x01, 0xff, 0xff, 0xff, 0xff, 0x0f]
        );
        let mut cursor = 0;
        assert_eq!(read_vql(&out, &mut cursor).unwrap(), 0);
        assert_eq!(read_vql(&out, &mut cursor).unwrap(), 127);
        assert_eq!(read_vql(&out, &mut cursor).unwrap(), 128);
        assert_eq!(read_vql(&out, &mut cursor).unwrap(), u32::MAX);
    }

    #[test]
    fn values_round_trip_through_the_wire_format() {
        for value in [
            WireValue::Undefined,
            WireValue::String(String::new()),
            WireValue::String("中文✓".to_string()),
            WireValue::Bytes(vec![0, 255, 16]),
            WireValue::VsBuffer(vec![1, 2]),
            WireValue::Int(0),
            WireValue::Int(2147483647),
            WireValue::Array(vec![
                WireValue::Int(1),
                WireValue::String("two".to_string()),
                WireValue::Json(json!({"three": 3})),
            ]),
            WireValue::Json(json!({"a": 1, "b": "two", "c": [true, null]})),
        ] {
            let bytes = encode(&value);
            assert_eq!(decode(&bytes), value, "round trip for {value:?}");
        }
    }

    #[test]
    fn negative_ints_use_the_js_unsigned_bit_pattern() {
        // TS: (data | 0) === data → writeInt32VQL(value >>> 7).
        let bytes = encode(&WireValue::Int((-7i32) as u32));
        assert_eq!(bytes, vec![0x06, 0xf9, 0xff, 0xff, 0xff, 0x0f]);
        assert_eq!(decode(&bytes), WireValue::Int(4_294_967_289));
    }

    #[test]
    fn truncated_buffers_are_errors_not_panics() {
        for input in [vec![1, 0x05], vec![4, 0x02, 0x06], vec![5, 0x03, b'{']] {
            let mut cursor = 0;
            assert!(decode_value(&input, &mut cursor).is_err(), "{input:?}");
        }
    }

    #[test]
    fn nested_uint8array_markers_match_the_typescript_shape() {
        for bytes in [
            &b""[..],
            &[0u8][..],
            &[1, 2][..],
            &[0, 255, 16, 127, 128][..],
        ] {
            let marker = nested_uint8array_json(bytes);
            assert_eq!(nested_uint8array_bytes(&marker).as_deref(), Some(bytes));
        }
        assert_eq!(
            nested_uint8array_json(&[1, 2]),
            json!({ "__zcode_rpc_nested_uint8array_v1": true,                "base64": "AQI=",
            })
        );
    }

    #[test]
    fn base64_is_standard_padded_and_strict() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(&[0, 255, 16]), "AP8Q");
        assert_eq!(base64_decode("AP8Q"), Some(vec![0, 255, 16]));
        for invalid in ["A", "AA=A", "=AAA", "AA==AAAA", "A!!!"] {
            assert_eq!(base64_decode(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn malformed_nested_markers_are_rejected() {
        for value in [
            json!({"__zcode_rpc_nested_uint8array_v1": true}),
            json!({"__zcode_rpc_nested_uint8array_v1": false, "base64": "AQI="}),
            json!({"__zcode_rpc_nested_uint8array_v1": true, "base64": "AQI=", "x": 1}),
            json!({"__zcode_rpc_nested_uint8array_v1": true, "base64": "!!!"}),
        ] {
            assert_eq!(nested_uint8array_bytes(&value), None, "{value}");
        }
    }

    #[test]
    fn nested_uint8array_roundtrip_empty_bytes() {
        let bytes = vec![];
        let value = nested_uint8array_json(&bytes);
        let recovered = nested_uint8array_bytes(&value).expect("should recover empty bytes");
        assert_eq!(recovered, bytes);
    }

    #[test]
    fn nested_uint8array_roundtrip_large_bytes() {
        let bytes: Vec<u8> = (0..256).map(|i| (i % 251) as u8).collect();
        let value = nested_uint8array_json(&bytes);
        let recovered = nested_uint8array_bytes(&value).expect("should recover large bytes");
        assert_eq!(recovered, bytes);
    }

    #[test]
    fn nested_uint8array_roundtrip_binary_data() {
        let bytes = vec![0x00, 0xFF, 0x01, 0xFE, 0x80, 0x7F];
        let value = nested_uint8array_json(&bytes);
        let recovered = nested_uint8array_bytes(&value).expect("should recover binary data");
        assert_eq!(recovered, bytes);
    }
}
