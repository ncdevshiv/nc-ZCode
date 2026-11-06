import io

# 1) catalog: drop the now-unused event-iteration timestamp field
path = 'crates/zcode-services-provider/src/catalog.rs'
s = io.open(path, encoding='utf-8').read()
s = s.replace('''struct MeasuredRun {
    ok: bool,
    latency_ms: u64,
    first_event_ms: Option<u64>,
    completion_tokens: Option<u64>,''','''struct MeasuredRun {
    ok: bool,
    latency_ms: u64,
    completion_tokens: Option<u64>,''')
s = s.replace('''    let mut first_event_ms = None;
    let mut completion_tokens = None;''','''    let mut completion_tokens = None;''')
s = s.replace('''            ModelStreamEvent::TextDelta { text, .. } => {
                if first_event_ms.is_none() {
                    first_event_ms = Some(started.elapsed().as_millis() as u64);
                }
                text_chars += text.chars().count();
            }
            ModelStreamEvent::ReasoningDelta { .. } => {
                if first_event_ms.is_none() {
                    first_event_ms = Some(started.elapsed().as_millis() as u64);
                }
                reasoning = true;
            }''','''            ModelStreamEvent::TextDelta { text, .. } => {
                text_chars += text.chars().count();
            }
            ModelStreamEvent::ReasoningDelta { .. } => {
                reasoning = true;
            }''')
s = s.replace('''    Ok(MeasuredRun {
        ok,
        latency_ms,
        first_event_ms,
        completion_tokens,''','''    Ok(MeasuredRun {
        ok,
        latency_ms,
        completion_tokens,''')
io.open(path, 'w', encoding='utf-8', newline='').write(s)
print('catalog cleaned')

# 2) host session: own the stream timing recorder
path = 'crates/zcode-host/src/session.rs'
s = io.open(path, encoding='utf-8').read()
s = s.replace(''' /// The latest activity time (refreshed when revision is moved forward; `updatedAt` of meta.json).
    last_activity_at: AtomicI64,''',''' /// The last activity moment (refreshed when the revision is moved forward; `updatedAt` of meta.json).
    last_activity_at: AtomicI64,
    /// The arrival time of this round-robin block (transport record; source of fact for assistant line `throughput`).
    stream_timing: zcode_provider::timing::StreamTiming,'''')
s = s.replace(''' last_activity_at: AtomicI64::new(if init.updated_at > 0 {''',''' stream_timing: zcode_provider::timing::StreamTiming::new(),
            last_activity_at: AtomicI64::new(if init.updated_at > 0 {''')
s = s.replace(''' pub fn last_activity_at(&self) -> i64 {
        self.last_activity_at.load(Ordering::Relaxed)
    }'',''' pub fn last_activity_at(&self) -> i64 {
        self.last_activity_at.load(Ordering::Relaxed)
    }

    /// Streaming window recorder (turn to start reset, close to read).
    pub fn stream_timing(&self) -> zcode_provider::timing::StreamTiming {
        self.stream_timing.clone()
    }''')
io.open(path, 'w', encoding='utf-8', newline='').write(s)
print('session timing added')