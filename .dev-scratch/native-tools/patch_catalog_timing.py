import io

path = 'crates/zcode-services-provider/src/catalog.rs'
s = io.open(path, encoding='utf-8').read()

s = s.replace('''use zcode_provider::transport::{RealClock, TransportRequest};
use zcode_provider::ModelStreamEvent;''','''use zcode_provider::timing::{StreamTiming, TimedTransport};
use zcode_provider::transport::{RealClock, TransportRequest};
use zcode_provider::ModelStreamEvent;''')

s = s.replace(''' text_chars: usize,
    error: Option<Value>,
}'',''' text_chars: usize,
    /// The actual streaming window (first block → last block) and number of blocks recorded by transport.
    generation_ms: u64,
    chunk_count: u64,
    error: Option<Value>,
}''')

s = s.replace(''' let started = Instant::now();''',''' let started = Instant::now();
    let timing = StreamTiming::new();''')

s = s.replace(''' let mut transport = HttpTransport::new();
    let execution = executor::execute(&mut transport, &RealClock, &stream_request).await;''',''' // Use TimedTransport to wrap the real transport: the TPS window must come from the block arrival time,
    // Instead of "after the event vector returns" (when all events have landed, the measured value is always 0).
    let mut transport = TimedTransport::new(HttpTransport::new(), timing.clone());
    let execution = executor::execute(&mut transport, &RealClock, &stream_request).await;''')

s = s.replace(''' let latency_ms = started.elapsed().as_millis() as u64;''',''' let latency_ms = started.elapsed().as_millis() as u64;
    let (generation_ms, chunk_count) = timing
        .window()
        .map(|(first, last, count)| (last.saturating_sub(first), count))
        .unwrap_or((0, 0));''')

s = s.replace(''' text_chars,
        error,
    })'','''' text_chars,
        generation_ms,
        chunk_count,
        error,
    })''')

s = s.replace(''' let mut result = json!({
        "modelId": model_id,
        "connectivity": {
            "ok": connectivity.ok,"latencyMs": connectivity.latency_ms,
            "firstTokenMs": connectivity.first_event_ms,
            "completionTokens": connectivity.completion_tokens,
            "textChars": connectivity.text_chars,
            "error": connectivity.error,
        }
    });''',''' let mut result = json!({
        "modelId": model_id,
        "connectivity": {
            "ok": connectivity.ok,
            "latencyMs": connectivity.latency_ms,
            "generationMs": connectivity.generation_ms,
            "chunks": connectivity.chunk_count,
            "completionTokens": connectivity.completion_tokens,
            "textChars": connectivity.text_chars,
            "error": connectivity.error,
        }
    });''')

s = s.replace(''' // Generate window = first token → end. The entire response cannot be separated when it falls on the same millisecond
                // (This will happen with local/extremely fast endpoints), degenerate to the entire request window and label windowSource truthfully,
                // Instead of forging a "generation time".
                let generation_ms = match run.first_event_ms {
                    Some(first) => run.latency_ms.saturating_sub(first),
                    None => 0,
                };
                let (window_ms, window_source) = if generation_ms > 0 {
                    (generation_ms, "generation")
                } else {
                    (run.latency_ms.max(1), "request")
                };''',''' // The generation window comes from the block arrival time of the transport; the entire response cannot be separated when it falls on the same millisecond
                // (This is true for native/extremely fast endpoints), fall back to the full request window and label windowSource accordingly.
                let (window_ms, window_source) = if run.generation_ms > 0 {
                    (run.generation_ms, "generation")
                } else {
                    (run.latency_ms.max(1), "request")
                };''')

io.open(path, 'w', encoding='utf-8', newline='').write(s)
print('catalog timing wired')path = 'crates/zcode-services-provider/Cargo.toml'
s = io.open(path, encoding='utf-8').read()
if 'tokio = { workspace = true, features = ["test-util"] }' not in s:
    s = s.replace('[dev-dependencies]', '[dev-dependencies]\n')
io.open(path, 'w', encoding='utf-8', newline='').write(s)
