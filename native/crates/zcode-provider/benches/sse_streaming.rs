//! Benchmark: SSE 流解析热路径（每个模型 token 都要过这里）。
//!
//! 覆盖 `SseParser`（纯 SSE 帧切分）与 `JsonSseStream`（帧 + JSON 反序列化），
//! 用 1KB 分块喂入一段贴近真实的 Anthropic 风格流（200 个 text_delta）。

use criterion::{criterion_group, criterion_main, Criterion};
use zcode_provider::sse::{JsonSseStream, SseParser};

fn sample_body(n_deltas: usize) -> Vec<u8> {
    let mut body = String::new();
    body.push_str("event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n");
    body.push_str("event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n");
    for i in 0..n_deltas {
        body.push_str(&format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"chunk{i} \"}}}}\n\n"
        ));
    }
    body.push_str(
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    );
    body.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    body.into_bytes()
}

fn bench_sse_parse(c: &mut Criterion) {
    let body = sample_body(200);
    c.bench_function("sse_parse_200_deltas_1k_chunks", |b| {
        b.iter(|| {
            let mut parser = SseParser::new();
            let mut count = 0usize;
            for chunk in body.chunks(1024) {
                count += parser.feed(chunk).len();
            }
            count += parser.finish().len();
            assert!(count >= 200, "expected >=200 events, got {count}");
        })
    });
}

fn bench_json_sse_parse(c: &mut Criterion) {
    let body = sample_body(200);
    c.bench_function("json_sse_parse_200_deltas_1k_chunks", |b| {
        b.iter(|| {
            let mut stream = JsonSseStream::new();
            let mut count = 0usize;
            for chunk in body.chunks(1024) {
                count += stream.feed(chunk).expect("valid json frames").len();
            }
            count += stream.finish().expect("valid json frames").len();
            assert!(count >= 200, "expected >=200 frames, got {count}");
        })
    });
}

criterion_group!(benches, bench_sse_parse, bench_json_sse_parse);
criterion_main!(benches);
