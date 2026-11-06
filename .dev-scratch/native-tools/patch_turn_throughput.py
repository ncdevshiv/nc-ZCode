import io

path = 'crates/zcode-host/src/turn.rs'
s = io.open(path, encoding='utf-8').read()

# 1) turn start: reset the streaming window and remember output-token baseline
old = ''' let mut lines = Vec::new();
    session.set_phase("running");
    session.set_active_command(Some(source_command_id.to_string()));'''
new = ''' let mut lines = Vec::new();
    // Throughput measurements must be zeroed on the **this round** boundary: both the window and the usage baseline only cover this round.
    session.stream_timing().reset();
    let timing_baseline_ms = crate::store::now_ms();
    let output_tokens_before = turn_output_tokens(&session);
    session.set_phase("running");
    session.set_active_command(Some(source_command_id.to_string()));'''
assert old in s
s = s.replace(old, new, 1)

# 2) final assistant row gets measured throughput
old = ''' session.set_last_error(failure.clone());
    lines.extend(session.project_assistant_final(
        row_id,
        &response_id,
        &final_text,
        row_state,
        model_label,
    ));'''
new = ''' session.set_last_error(failure.clone());
    let throughput = turn_throughput(&session, output_tokens_before, timing_baseline_ms, &streamed_text);
    lines.extend(session.project_assistant_final(
        row_id,
        &response_id,
        &final_text,
        row_state,
        model_label,
        throughput.as_ref(),
    ));'''
assert old in s
s = s.replace(old, new, 1)

# 3) history failure path: no measured throughput
old = ''' session.set_last_error(Some(info));
    lines.extend(session.project_assistant_final(
        row_id,
        response_id,
        &streamed_text,
        "failed",
        model_label,
    ));'''
new = ''' session.set_last_error(Some(info));
    lines.extend(session.project_assistant_final(
        row_id,
        response_id,
        &streamed_text,"failed",
        model_label,
        None,
    ));'''
assert old in s
s = s.replace(old, new, 1)

# 4) transport wrapper for real chunk timing
old = ''' let mut transport = HttpTransport::new();
    let clock = RealClock;
    session.record_model_request();'''
new = ''' // TimedTransport records the arrival moment of each SSE block: tok/s for assistant lines can only come from here.
    let mut transport = zcode_provider::timing::TimedTransport::new(
        HttpTransport::new(),
        session.stream_timing(),
    );
    let clock = RealClock;
    session.record_model_request();'''
assert old in s
s = s.replace(old, new, 1)

# 5) helpers appended before cumulative_usage
old = '''pub fn cumulative_usage(usage: &ModelUsage) -> Value {'''
new = '''fn turn_output_tokens(session: &Arc<Session>) -> u64 {
    session
        .usage_report()
        .get("outputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Output throughput this round. The window preferentially takes the first block → the last block (`generation`) recorded by the transport;
/// When only one block is received in a round, the window is 0, and the entire round time (`turn`) is returned instead of forging a smaller window.
/// When the provider does not respond to usage, estimate it using 4 characters/token and mark it with `estimated` for magnitude display.
fn turn_throughput(
    session: &Arc<Session>,
    output_tokens_before: u64,
    timing_baseline_ms: i64,
    streamed_text: &str,
) -> Option<Value> {
    let (first_ms, last_ms, chunks) = session.stream_timing().window()?;
    let generation_ms = last_ms.saturating_sub(first_ms);
    let reported = turn_output_tokens(session).saturating_sub(output_tokens_before);
    let (completion_tokens, estimated) = if reported > 0 {
        (reported, false)
    } else {
        (((streamed_text.chars().count() as f64) / 4.0).ceil() as u64, true)
    };
    if completion_tokens == 0 {
        return None;
    }
    let (elapsed_ms, window_source) = if generation_ms > 0 {
        (generation_ms, "generation")
    } else {
        (            crate::store::now_ms()
                .saturating_sub(timing_baseline_ms)
                .max(1) as u64,
            "turn",
        )
    };
    let tokens_per_second =
        f64::round(completion_tokens as f64 * 1000.0 / elapsed_ms as f64 * 10.0) / 10.0;
    Some(json!({
        "completionTokens": completion_tokens,
        "elapsedMs": elapsed_ms,
        "tokensPerSecond": tokens_per_second,
        "chunks": chunks,
        "windowSource": window_source,
        "estimated": estimated,
    }))
}

pub fn cumulative_usage(usage: &ModelUsage) -> Value {'''
assert old in s
s = s.replace(old, new, 1)

io.open(path, 'w', encoding='utf-8', newline='').write(s)
print('turn.rs patched')
