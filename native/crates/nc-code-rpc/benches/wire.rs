//! Wire-format microbenchmarks: serialization, framing and channel messages.
//!
//! These are not product-latency claims — they pin the per-byte cost of the Rust port
//! so regressions (extra copies, accidental quadratic buffering) show up in review.
//! Run: `cargo bench -p nc-code-rpc`.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use nc_code_rpc::channel::RpcErrorPayload;
use nc_code_rpc::protocol::{encode_frame, FrameDecoder};
use nc_code_rpc::{decode_value, encode_value, ChannelMessage, WireValue};
use serde_json::json;

const PAYLOAD_64K: usize = 64 * 1024;

fn wire_serialization(c: &mut Criterion) {
    let mut group = c.benchmark_group("serialization");

    let object = WireValue::Json(json!({
        "sessionId": "session-benchmark",
        "workspacePath": "/workspace/zcode",
        "text": "hello world",
        "flags": {"stream": true, "retry": 3},
    }));
    let mut encoded_object = Vec::new();
    encode_value(&object, &mut encoded_object);
    group.throughput(Throughput::Bytes(encoded_object.len() as u64));
    group.bench_function("encode_object", |b| {
        b.iter(|| {
            let mut out = Vec::with_capacity(encoded_object.len());
            encode_value(black_box(&object), &mut out);
            black_box(out);
        });
    });
    group.bench_function("decode_object", |b| {
        b.iter(|| {
            let mut cursor = 0;
            black_box(decode_value(black_box(&encoded_object), &mut cursor).expect("decode"));
        });
    });

    let bytes = WireValue::Bytes(vec![0x5a; PAYLOAD_64K]);
    let mut encoded_bytes = Vec::new();
    encode_value(&bytes, &mut encoded_bytes);
    group.throughput(Throughput::Bytes(PAYLOAD_64K as u64));
    group.bench_function("encode_64k_bytes", |b| {
        b.iter(|| {
            let mut out = Vec::with_capacity(encoded_bytes.len());
            encode_value(black_box(&bytes), &mut out);
            black_box(out);
        });
    });
    group.bench_function("decode_64k_bytes", |b| {
        b.iter(|| {
            let mut cursor = 0;
            black_box(decode_value(black_box(&encoded_bytes), &mut cursor).expect("decode"));
        });
    });
    group.finish();
}

fn framing(c: &mut Criterion) {
    let mut group = c.benchmark_group("framing");

    let payload = vec![0x42; PAYLOAD_64K];
    let frame = encode_frame(&payload);
    group.throughput(Throughput::Bytes(PAYLOAD_64K as u64));
    group.bench_function("encode_64k_frame", |b| {
        b.iter(|| black_box(encode_frame(black_box(&payload))));
    });
    group.bench_function("decode_64k_frame", |b| {
        b.iter(|| {
            let mut decoder = FrameDecoder::default();
            let frames = decoder.push(black_box(&frame)).expect("decode");
            assert_eq!(frames.len(), 1);
            black_box(frames);
        });
    });

    // Eight coalesced frames per push: the steady state of a busy socket.
    let mut coalesced = Vec::new();
    for index in 0..8u32 {
        let mut payload = vec![0u8; 1024];
        payload[..4].copy_from_slice(&index.to_be_bytes());
        coalesced.extend_from_slice(&encode_frame(&payload));
    }
    group.throughput(Throughput::Bytes(coalesced.len() as u64));
    group.bench_function("decode_coalesced_8x1k", |b| {
        b.iter(|| {
            let mut decoder = FrameDecoder::default();
            let frames = decoder.push(black_box(&coalesced)).expect("decode");
            assert_eq!(frames.len(), 8);
            black_box(frames);
        });
    });
    group.finish();
}

fn channel_messages(c: &mut Criterion) {
    let mut group = c.benchmark_group("channel");

    let request = ChannelMessage::Promise {
        id: 42,
        channel: "zcode.conversation".to_string(),
        method: "sendText".to_string(),
        arg: WireValue::Json(json!({"text": "hello", "sessionId": "session-benchmark"})),
    };
    let request_bytes = request.encode();
    group.throughput(Throughput::Bytes(request_bytes.len() as u64));
    group.bench_function("encode_request", |b| {
        b.iter(|| black_box(black_box(&request).encode()));
    });
    group.bench_function("decode_request", |b| {
        b.iter(|| black_box(ChannelMessage::decode(black_box(&request_bytes)).expect("decode")));
    });

    let mut payload = RpcErrorPayload::new("boom", "BoomError");
    payload.code = Some(json!("guard.staleRevision"));
    payload.data = Some(json!({"expected": 12, "actual": 9}));
    let error = ChannelMessage::PromiseError {
        id: 42,
        data: payload.to_value(),
    };
    let error_bytes = error.encode();
    group.bench_function("encode_error_response", |b| {
        b.iter(|| black_box(black_box(&error).encode()));
    });
    group.bench_function("decode_error_response", |b| {
        b.iter(|| black_box(ChannelMessage::decode(black_box(&error_bytes)).expect("decode")));
    });
    group.finish();
}

criterion_group!(benches, wire_serialization, framing, channel_messages);
criterion_main!(benches);
