//! Golden-vector conformance: fixtures under `native/fixtures/v4-wire/` are
//! generated from the TS source of truth (`pnpm golden:vectors`) and must be
//! reproduced exactly by this crate. Any failure here means TS↔Rust wire
//! divergence — fix the mirror (or regenerate deliberately), never the fixture,
//! unless the TS source changed on purpose.

use serde_json::Value;
use zcode_v4_wire::*;

const CONSTANTS_FIXTURE: &str = include_str!("../../../fixtures/v4-wire/protocol-constants.json");
const BINARY_FIXTURE: &str = include_str!("../../../fixtures/v4-wire/binary-vectors.json");
const ENVELOPE_FIXTURE: &str = include_str!("../../../fixtures/v4-wire/envelope-measurements.json");

#[test]
fn constants_match_shared_source() {
    let fixture: Value = serde_json::from_str(CONSTANTS_FIXTURE).expect("valid fixture json");
    assert_eq!(
        fixture["wireVersion"].as_u64().expect("wireVersion") as u32,
        V4_WIRE_PROTOCOL_VERSION,
        "V4_WIRE_PROTOCOL_VERSION drifted from packages/shared"
    );
    let limits = &fixture["limits"];
    assert_eq!(
        limits["maxFrameBytes"].as_u64().unwrap() as usize,
        MAX_FRAME_BYTES
    );
    assert_eq!(
        limits["logicalFrameAssemblyMaxFragments"].as_u64().unwrap() as u32,
        LOGICAL_FRAME_ASSEMBLY_MAX_FRAGMENTS
    );
    assert_eq!(
        limits["transportEnvelopeIdMaxChars"].as_u64().unwrap() as usize,
        TRANSPORT_ENVELOPE_ID_MAX_CHARS
    );
}

#[test]
fn binary_vectors_match_ts_wire_functions() {
    let fixture: Value = serde_json::from_str(BINARY_FIXTURE).expect("valid fixture json");
    let cases = fixture["cases"].as_array().expect("cases array");
    assert!(!cases.is_empty(), "fixture must contain cases");
    for case in cases {
        let name = case["name"].as_str().unwrap_or("<unnamed>");
        let bytes: Vec<u8> = case["bytes"]
            .as_array()
            .unwrap_or_else(|| panic!("case {name}: bytes array"))
            .iter()
            .map(|v| v.as_u64().expect("byte value") as u8)
            .collect();
        assert_eq!(
            crc32_wire_bytes(&bytes),
            case["crc32"].as_str().expect("crc32"),
            "case {name}: crc32 mismatch"
        );
        let encoded = encode_wire_bytes_base64(&bytes);
        assert_eq!(
            encoded,
            case["encoded"].as_str().expect("encoded"),
            "case {name}: base64 encode mismatch"
        );
        // Decode expectation recorded from actual TS behavior (null only for the
        // empty input, which the TS `.min(4)` schema rejects on decode).
        let expected: Option<Vec<u8>> = case["decoded"].as_array().map(|items| {
            items
                .iter()
                .map(|v| v.as_u64().expect("byte value") as u8)
                .collect()
        });
        assert_eq!(
            decode_wire_base64(&encoded),
            expected,
            "case {name}: decode mismatch (TS leniency mirror broken)"
        );
    }
}

#[test]
fn base64_decode_edge_cases_match_ts_leniency() {
    let fixture: Value = serde_json::from_str(BINARY_FIXTURE).expect("valid fixture json");
    let cases = fixture["decodeEdgeCases"]
        .as_array()
        .expect("decodeEdgeCases array");
    assert!(!cases.is_empty(), "fixture must contain decode edge cases");
    for case in cases {
        let name = case["name"].as_str().unwrap_or("<unnamed>");
        let value = case["value"].as_str().expect("value");
        let expected: Option<Vec<u8>> = case["decoded"].as_array().map(|items| {
            items
                .iter()
                .map(|v| v.as_u64().expect("byte value") as u8)
                .collect()
        });
        assert_eq!(
            decode_wire_base64(value),
            expected,
            "case {name}: decode edge mismatch (TS leniency mirror broken)"
        );
    }
}

#[test]
fn envelope_measurement_matches_ts_codec() {
    let fixture: Value = serde_json::from_str(ENVELOPE_FIXTURE).expect("valid fixture json");
    let cases = fixture["cases"].as_array().expect("cases array");
    assert!(!cases.is_empty(), "fixture must contain cases");
    for case in cases {
        let name = case["name"].as_str().unwrap_or("<unnamed>");
        let wire_json = case["wireJson"].as_str().expect("wireJson");

        // Serializer parity: parse + re-serialize (key order preserved) must be
        // byte-identical to JSON.stringify output — this is the conformance net
        // for serde vs JS number/escaping behavior within the fixture scope.
        let parsed: Value =
            serde_json::from_str(wire_json).unwrap_or_else(|e| panic!("case {name}: parse {e}"));
        let reencoded =
            serde_json::to_string(&parsed).unwrap_or_else(|e| panic!("case {name}: reencode {e}"));
        assert_eq!(
            reencoded, wire_json,
            "case {name}: serde re-serialization diverged from JSON.stringify"
        );
        assert_eq!(
            utf8_json_byte_length(wire_json),
            case["wireJsonBytes"].as_u64().expect("wireJsonBytes") as usize,
            "case {name}: wireJsonBytes mismatch"
        );

        let measured = measure_topic_notification_envelope_bytes(wire_json);
        let expected = &case["measurement"];
        assert_eq!(
            measured.cli_ndjson_bytes,
            expected["cliNdjsonBytes"].as_u64().unwrap() as usize,
            "case {name}: cliNdjsonBytes mismatch"
        );
        assert_eq!(
            measured.channel_socket_bytes,
            expected["channelSocketBytes"].as_u64().unwrap() as usize,
            "case {name}: channelSocketBytes mismatch"
        );
        assert_eq!(
            measured.mobile_relay_bytes,
            expected["mobileRelayBytes"].as_u64().unwrap() as usize,
            "case {name}: mobileRelayBytes mismatch"
        );
        assert_eq!(
            measured.max_bytes,
            expected["maxBytes"].as_u64().unwrap() as usize,
            "case {name}: maxBytes mismatch"
        );
    }
}
