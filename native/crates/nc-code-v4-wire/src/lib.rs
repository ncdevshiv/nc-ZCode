//! Rust mirror of the V4 wire binary pure functions.
//!
//! Source of truth during TS↔Rust coexistence: `packages/shared/src/nc-code-protocol-v4/`
//! (see `specs/rust-pure-native-roadmap.md` S0). Golden vectors under
//! `native/fixtures/v4-wire/` are generated from that TS source by
//! `scripts/gen-v4-golden-vectors.ts`; this crate must reproduce them exactly
//! (`tests/golden.rs`). Regenerate with `pnpm golden:vectors`, verify drift with
//! `pnpm golden:check`.
//!
//! Semantics mirrored on purpose, quirks included: see `native/README.md` for the
//! divergence boundaries (lenient base64, no lone surrogates, integer-only JSON).

/// V4 physical wire protocol version — mirror of `V4_WIRE_PROTOCOL_VERSION`.
pub const V4_WIRE_PROTOCOL_VERSION: u32 = 3;

/// Mirror of the `PROTOCOL_V4_LIMITS` subset this crate uses. Extend the
/// `protocol-constants.json` fixture in the same change as any new constant.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const LOGICAL_FRAME_ASSEMBLY_MAX_FRAGMENTS: u32 = 1024;
pub const TRANSPORT_ENVELOPE_ID_MAX_CHARS: usize = 256;

const CHANNEL_EVENT_RESPONSE_TYPE: u64 = 204;
const SOCKET_PROTOCOL_HEADER_BYTES: usize = 13;
/// `JSON.stringify({ method: "v4/conversation/frame", params: <wire> })` prefix —
/// key order is the TS object-literal order and is part of the wire contract.
const CLI_NDJSON_PREFIX: &str = r#"{"method":"v4/conversation/frame","params":"#;
/// `String(Number.MAX_SAFE_INTEGER)` — worst-case Channel event id in JSON.
const MAX_EVENT_ID_JSON: &str = "9007199254740991";
const MAX_EVENT_ID_JSON_BYTES: usize = 16;

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// CRC-32 (reflected, init/xorout `0xffffffff`) over the wire bytes, lowercase
/// zero-padded to 8 hex chars — mirror of `crc32WireBytes`.
pub fn crc32_wire_bytes(bytes: &[u8]) -> String {
    let mut crc: u32 = 0xffff_ffff;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (if crc & 1 == 1 { 0xedb8_8320 } else { 0 });
        }
    }
    format!("{:08x}", crc ^ 0xffff_ffff)
}

/// Standard base64 with padding — mirror of `encodeWireBytesBase64`.
pub fn encode_wire_bytes_base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut offset = 0;
    while offset < bytes.len() {
        let a = bytes[offset];
        let has_b = offset + 1 < bytes.len();
        let has_c = offset + 2 < bytes.len();
        let b = if has_b { bytes[offset + 1] } else { 0 };
        let c = if has_c { bytes[offset + 2] } else { 0 };
        out.push(BASE64_ALPHABET[(a >> 2) as usize] as char);
        out.push(BASE64_ALPHABET[usize::from((a & 0x03) << 4) | usize::from(b >> 4)] as char);
        out.push(if has_b {
            BASE64_ALPHABET[usize::from((b & 0x0f) << 2) | usize::from(c >> 6)] as char
        } else {
            '='
        });
        out.push(if has_c {
            BASE64_ALPHABET[usize::from(c & 0x3f)] as char
        } else {
            '='
        });
        offset += 3;
    }
    out
}

fn is_base64_alphabet(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/'
}

/// Validates `topicWireBase64Schema`: min length 4, `^(?:[A-Za-z0-9+/]{4})*`
/// optionally followed by `XX==` or `XXX=`. Lenient where the TS decode is
/// lenient (non-canonical data bits in the final group are accepted).
fn is_valid_wire_base64(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 4 || !bytes.len().is_multiple_of(4) {
        return false;
    }
    let mut offset = 0;
    while offset < bytes.len() {
        let group = &bytes[offset..offset + 4];
        let is_final = offset + 4 == bytes.len();
        let two_pad = is_final && group[2] == b'=' && group[3] == b'=';
        let one_pad = is_final && !two_pad && group[3] == b'=';
        let data_len = if two_pad {
            2
        } else if one_pad {
            3
        } else {
            4
        };
        if !group[..data_len].iter().copied().all(is_base64_alphabet) {
            return false;
        }
        if two_pad || one_pad {
            return true; // padding is only legal as the final group
        }
        offset += 4;
    }
    true
}

/// Lenient decode mirroring `decodeWireBase64`: `None` on schema violation,
/// otherwise the byte payload (padding-truncated).
pub fn decode_wire_base64(value: &str) -> Option<Vec<u8>> {
    if !is_valid_wire_base64(value) {
        return None;
    }
    let index = |ch: u8| -> Option<usize> { BASE64_ALPHABET.iter().position(|&c| c == ch) };
    let padding = if value.ends_with("==") {
        2
    } else if value.ends_with('=') {
        1
    } else {
        0
    };
    let mut output = vec![0u8; (value.len() / 4) * 3 - padding];
    let bytes = value.as_bytes();
    let mut write_offset = 0;
    let mut offset = 0;
    while offset < bytes.len() {
        let a = index(bytes[offset])?;
        let b = index(bytes[offset + 1])?;
        let c = if bytes[offset + 2] == b'=' {
            0
        } else {
            index(bytes[offset + 2])?
        };
        let d = if bytes[offset + 3] == b'=' {
            0
        } else {
            index(bytes[offset + 3])?
        };
        let combined = (a << 18) | (b << 12) | (c << 6) | d;
        if write_offset < output.len() {
            output[write_offset] = (combined >> 16) as u8;
            write_offset += 1;
        }
        if write_offset < output.len() {
            output[write_offset] = ((combined >> 8) & 0xff) as u8;
            write_offset += 1;
        }
        if write_offset < output.len() {
            output[write_offset] = (combined & 0xff) as u8;
            write_offset += 1;
        }
        offset += 4;
    }
    Some(output)
}

/// VQL (variable-length quantity) byte length — mirror of `vqlByteLength`.
/// Callers pass sizes/ids that fit the TS 32-bit `>>> 7` regime.
pub fn vql_byte_length(value: u64) -> usize {
    let mut bytes = 1;
    let mut remaining = value >> 7;
    while remaining > 0 {
        bytes += 1;
        remaining >>= 7;
    }
    bytes
}

/// UTF-8 byte length of an already-encoded JSON string — mirror of
/// `utf8JsonByteLength` for values produced by `JSON.stringify`.
pub fn utf8_json_byte_length(json: &str) -> usize {
    json.len()
}

/// Envelope measurement result — mirror of the return shape of
/// `measureTopicNotificationEnvelopeBytes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopicNotificationEnvelopeBytes {
    pub cli_ndjson_bytes: usize,
    pub channel_socket_bytes: usize,
    pub mobile_relay_bytes: usize,
    pub max_bytes: usize,
}

/// Measures a topic notification against the three real production carriers,
/// given the exact `JSON.stringify` bytes of the wire frame. Arithmetic mirror
/// of `measureTopicNotificationEnvelopeBytes` — all inputs are integer sizes, so
/// no JSON serializer is involved on this path.
pub fn measure_topic_notification_envelope_bytes(
    wire_json: &str,
) -> TopicNotificationEnvelopeBytes {
    let cli_ndjson_bytes =
        CLI_NDJSON_PREFIX.len() + wire_json.len() + 1 /* closing brace */ + 1 /* trailing \n */;

    let wire_json_bytes = wire_json.len();
    let max_event_id_serialized_bytes =
        1 + vql_byte_length(MAX_EVENT_ID_JSON_BYTES as u64) + MAX_EVENT_ID_JSON_BYTES;
    let channel_header_bytes = 1
        + vql_byte_length(2)
        + 1
        + vql_byte_length(CHANNEL_EVENT_RESPONSE_TYPE)
        + max_event_id_serialized_bytes;
    let channel_payload_bytes =
        channel_header_bytes + 1 + vql_byte_length(wire_json_bytes as u64) + wire_json_bytes;
    let channel_socket_bytes = channel_payload_bytes + SOCKET_PROTOCOL_HEADER_BYTES;

    let data_base64_bytes = 4 * channel_payload_bytes.div_ceil(3);
    let transport_id = "x".repeat(TRANSPORT_ENVELOPE_ID_MAX_CHARS);
    let mobile_relay_fixed = format!(
        concat!(
            r#"{{"type":"data","payload":{{"zcode_type":"rpc-frame","bridgeSessionId":"#,
            "\"{id}\",",
            r#""bridgeGeneration":{max},"recoveryId":"#,
            "\"{id}\",",
            r#""seq":{max},"dataBase64":""}},"#,
            r#""client_ts":{max},"server_ts":{max}}}"#
        ),
        id = transport_id,
        max = MAX_EVENT_ID_JSON,
    );
    let mobile_relay_bytes = mobile_relay_fixed.len() + data_base64_bytes;

    TopicNotificationEnvelopeBytes {
        cli_ndjson_bytes,
        channel_socket_bytes,
        mobile_relay_bytes,
        max_bytes: cli_ndjson_bytes
            .max(channel_socket_bytes)
            .max(mobile_relay_bytes),
    }
}
