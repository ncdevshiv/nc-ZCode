//! The session debug ledger (the source of truth of `session/debug`).
//!
//! Ports the observation semantics of TS `bootstrap/src/nc-code-protocol/session-debug.ts` entry by entry:
//! it observes `model_request_*` network facts and produces two kinds of read-only view —
//!
//! - **rounds**: usage, hit rate, generation duration and tok/s for each completed main-turn model request;
//! - **networkEntries**: request/response headers, status code, backoff and stall windows for each physical request (attempt);
//! - **cache**: this session's cumulative input / cacheRead and hit rate.
//!
//! All data comes from `nc-code-provider`'s [`AttemptTrace`] (real transport head + injected clock)
//! and real usage events; nothing is estimated or invented. The limits are the same as TS: rounds 200, network 100,
//! dedupe 2000; the ledger is **in-process** (matching TS's WeakMap observation), so a restart starts from an empty ledger.

use std::collections::{HashSet, VecDeque};

use nc_code_provider::executor::{AttemptTrace, AttemptTraceState};
use nc_code_provider::ModelUsage;
use serde_json::{json, Value};

/// `SESSION_DEBUG_LIMITS` (packages/shared/src/session-debug.ts:3).
pub const ROUNDS_LIMIT: usize = 200;
pub const NETWORK_LIMIT: usize = 100;
pub const DEDUPE_LIMIT: usize = 2000;
const MAX_HEADER_COUNT: usize = 32;
const MAX_HEADER_VALUE_LENGTH: usize = 512;
const MAX_MESSAGE_LENGTH: usize = 2048;

/// The post-completion facts of one main-turn model request.
pub struct RoundInput<'a> {
    pub request_id: &'a str,
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
    pub first_content_at_ms: Option<u64>,
    pub usage: Option<&'a ModelUsage>,
}

/// The network entry's context (identical for every request).
pub struct NetworkContext<'a> {
    pub trace_id: &'a str,
    pub request_id: &'a str,
    pub query_source: &'a str,
    pub provider_id: Option<&'a str>,
    pub model_id: &'a str,
    pub provider_kind: &'a str,
    pub base_url: &'a str,
    pub max_attempts: u32,
}

#[derive(Debug, Default, Clone, PartialEq)]
struct CacheTotals {
    hit_rate_request_count: u64,
    total_input_tokens: u64,
    total_cache_read_tokens: u64,
    hit_rate: Option<f64>,
}

#[derive(Debug, Default)]
pub struct SessionDebugLedger {
    rounds: VecDeque<Value>,
    network: VecDeque<Value>,
    cache: Option<CacheTotals>,
    /// TS `completedRequests`: one requestId produces only one round (a retry's attempt is not counted again).
    completed_requests: HashSet<String>,
    has_unknown_cache_usage: bool,
    entry_seq: u64,
}

impl SessionDebugLedger {
    fn next_entry_key(&mut self) -> String {
        self.entry_seq = self.entry_seq.saturating_add(1);
        format!("debug-{}", self.entry_seq)
    }

    /// Bounded dedupe (TS `SESSION_DEBUG_LIMITS.dedupe`): the second occurrence of the same requestId produces no round.
    fn remember(keys: &mut HashSet<String>, key: &str) -> bool {
        if !keys.insert(key.to_string()) {
            return false;
        }
        if keys.len() > DEDUPE_LIMIT {
            if let Some(oldest) = keys.iter().next().cloned() {
                keys.remove(&oldest);
            }
        }
        true
    }

    /// Record one model request (attempt-level network facts + the post-completion round/cache).
    pub fn record_request(
        &mut self,
        context: &NetworkContext<'_>,
        traces: &[AttemptTrace],
        round: Option<RoundInput<'_>>,
    ) {
        for trace in traces {
            self.record_attempt(context, trace);
        }
        if let Some(round) = round {
            self.record_round(round);
        }
    }

    fn record_attempt(&mut self, context: &NetworkContext<'_>, trace: &AttemptTrace) {
        // A cancellation before any connection was established has no network fact to record (it would otherwise show a request that was never sent).
        if trace.status.is_none() && trace.state == AttemptTraceState::Cancelled {
            return;
        }
        let request_headers = bounded_headers(&trace.request_headers);
        let response_headers = bounded_headers(&trace.response_headers);
        let started_key = self.next_entry_key();
        let started = self.network_entry(
            context,
            &started_key,
            trace,
            "model_request_started",
            json!({
                "requestHeaders": request_headers,
                "responseHeaders": json!({}),
                "requestHeaderCount": trace.request_headers.len(),
                "responseHeaderCount": 0,
            }),
        );
        self.push_network(started);

        let Some(ended_at) = trace.ended_at_ms else {
            return;
        };
        let duration = ended_at.saturating_sub(trace.started_at_ms);
        match trace.state {
            AttemptTraceState::Completed => {
                let key = self.next_entry_key();
                let entry = self.network_entry(
                    context,
                    &key,
                    trace,
                    "model_request_completed",
                    json!({
                        "recordedAt": ended_at,
                        "durationMs": duration,
                        "requestHeaders": request_headers,
                        "responseHeaders": response_headers,
                        "requestHeaderCount": trace.request_headers.len(),
                        "responseHeaderCount": trace.response_headers.len(),
                    }),
                );
                self.push_network(entry);
            }
            AttemptTraceState::Failed => {
                let stalled = trace.failure_kind == Some("stream_idle_timeout");
                let mut extra = json!({
                    "recordedAt": ended_at,
                    "durationMs": duration,
                    "requestHeaders": request_headers,
                    "responseHeaders": response_headers,
                    "requestHeaderCount": trace.request_headers.len(),
                    "responseHeaderCount": trace.response_headers.len(),
                });
                if let Some(kind) = trace.failure_kind {
                    extra["reason"] = json!(kind);
                }
                if let Some(message) = trace.failure_message.as_deref() {
                    extra["message"] = json!(bounded_message(message));
                }
                if let Some(timeout_ms) = trace.stalled_timeout_ms {
                    extra["timeoutMs"] = json!(timeout_ms);
                }
                let status_type = if stalled {
                    "model_stream_stalled"
                } else {
                    "model_request_failed"
                };
                let key = self.next_entry_key();
                let entry = self.network_entry(context, &key, trace, status_type, extra);
                self.push_network(entry);
            }
            AttemptTraceState::Started | AttemptTraceState::Cancelled => {}
        }
        if let Some(delay_ms) = trace.retry_delay_ms {
            let key = self.next_entry_key();
            let entry = self.network_entry(
                context,
                &key,
                trace,
                "model_retry_scheduled",
                json!({
                    "recordedAt": ended_at,
                    "attempt": trace.attempt,
                    "maxAttempts": context.max_attempts,
                    "nextAttempt": trace.attempt.saturating_add(1),
                    "retryable": true,
                    "delayMs": delay_ms,
                }),
            );
            self.push_network(entry);
        }
    }

    fn network_entry(
        &self,
        context: &NetworkContext<'_>,
        event_key: &str,
        trace: &AttemptTrace,
        status_type: &str,
        extra: Value,
    ) -> Value {
        let mut entry = json!({
            "eventKey": event_key,
            "traceId": context.trace_id,
            "recordedAt": trace.started_at_ms,
            "statusType": status_type,
            "requestId": context.request_id,
            "modelId": context.model_id,
            "providerKind": context.provider_kind,
            "transport": "http",
            "baseURL": context.base_url,
            "querySource": context.query_source,
            "attempt": trace.attempt,
            "maxAttempts": context.max_attempts,
        });
        if let Some(provider_id) = context.provider_id {
            entry["providerId"] = json!(provider_id);
        }
        if let Some(status) = trace.status {
            entry["statusCode"] = json!(status);
        }
        if let Some(extra) = extra.as_object() {
            for (key, value) in extra {
                entry[key] = value.clone();
            }
        }
        entry
    }

    fn push_network(&mut self, entry: Value) {
        self.network.push_back(entry);
        while self.network.len() > NETWORK_LIMIT {
            self.network.pop_front();
        }
    }

    fn record_round(&mut self, round: RoundInput<'_>) {
        if !Self::remember(&mut self.completed_requests, round.request_id) {
            return;
        }
        let usage = round.usage;
        let input_tokens = usage.and_then(|usage| usage.input_tokens);
        let output_tokens = usage.and_then(|usage| usage.output_tokens);
        let cache_read_tokens = usage.and_then(|usage| usage.cache_read_tokens);
        let duration = round.ended_at_ms.saturating_sub(round.started_at_ms);
        let time_to_first = round
            .first_content_at_ms
            .map(|first| first.saturating_sub(round.started_at_ms));
        let generation_duration_ms = match time_to_first {
            Some(first) if duration > first => Some(duration - first),
            _ => None,
        };
        let request_index = self
            .cache
            .as_ref()
            .map(|cache| cache.hit_rate_request_count)
            .unwrap_or(0)
            + 1;
        let total_input = self
            .cache
            .as_ref()
            .map(|cache| cache.total_input_tokens)
            .unwrap_or(0)
            + input_tokens.unwrap_or(0);
        let total_cache_read = self
            .cache
            .as_ref()
            .map(|cache| cache.total_cache_read_tokens)
            .unwrap_or(0)
            + cache_read_tokens.unwrap_or(0);
        if input_tokens.is_none() || cache_read_tokens.is_none() {
            self.has_unknown_cache_usage = true;
        }
        let hit_rate = if !self.has_unknown_cache_usage && total_input > 0 {
            Some(total_cache_read as f64 / total_input as f64)
        } else {
            None
        };
        self.cache = Some(CacheTotals {
            hit_rate_request_count: request_index,
            total_input_tokens: total_input,
            total_cache_read_tokens: total_cache_read,
            hit_rate,
        });

        let total_tokens = usage.and_then(|usage| usage.total_tokens).or_else(|| {
            match (input_tokens, output_tokens) {
                (Some(input), Some(output)) => Some(input + output),
                _ => None,
            }
        });
        let round_hit_rate = match (input_tokens, cache_read_tokens) {
            (Some(input), Some(cache_read)) if input > 0 => Some(cache_read as f64 / input as f64),
            _ => None,
        };
        let tokens_per_second = output_tokens
            .zip(generation_duration_ms)
            .filter(|(_, duration)| *duration > 0)
            .map(|(tokens, duration)| tokens as f64 * 1000.0 / duration as f64)
            .filter(|value| value.is_finite());
        let event_key = self.next_entry_key();
        self.rounds.push_back(json!({
            "eventKey": event_key,
            "requestId": round.request_id,
            "requestIndex": request_index,
            "recordedAt": round.ended_at_ms,
            "usage": usage_json(
                input_tokens,
                output_tokens,
                total_tokens,
                usage.and_then(|usage| usage.reasoning_tokens),
                cache_read_tokens,
                usage.and_then(|usage| usage.cache_write_tokens),
            ),
            "hitRate": round_hit_rate,
            "generationDurationMs": generation_duration_ms,
            "tokensPerSecond": tokens_per_second,
        }));
        while self.rounds.len() > ROUNDS_LIMIT {
            self.rounds.pop_front();
        }
    }

    /// The `sessionDebugSnapshotSchema` shape (strict: no extra keys).
    pub fn snapshot(&self, session_id: &str) -> Value {
        let cache = match &self.cache {
            Some(cache) => json!({
                "hitRateRequestCount": cache.hit_rate_request_count,
                "totalInputTokens": cache.total_input_tokens,
                "totalCacheReadTokens": cache.total_cache_read_tokens,
                "hitRate": cache.hit_rate,
            }),
            None => Value::Null,
        };
        json!({
            "sessionId": session_id,
            "rounds": self.rounds.iter().cloned().collect::<Vec<_>>(),
            "networkEntries": self.network.iter().cloned().collect::<Vec<_>>(),
            "cache": cache,
        })
    }
}

fn usage_json(
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    cached_write_input_tokens: Option<u64>,
) -> Value {
    // A key is written only when it exists (TS's object literal likewise omits undefined keys).
    let mut usage = serde_json::Map::new();
    for (key, value) in [
        ("inputTokens", input_tokens),
        ("outputTokens", output_tokens),
        ("totalTokens", total_tokens),
        ("reasoningTokens", reasoning_tokens),
        ("cachedInputTokens", cached_input_tokens),
        ("cachedWriteInputTokens", cached_write_input_tokens),
    ] {
        if let Some(value) = value {
            usage.insert(key.to_string(), json!(value));
        }
    }
    Value::Object(usage)
}

fn bounded_headers(headers: &[(String, String)]) -> Value {
    let mut out = serde_json::Map::new();
    for (name, value) in headers.iter().take(MAX_HEADER_COUNT) {
        out.insert(
            name.chars().take(MAX_HEADER_VALUE_LENGTH).collect(),
            json!(value
                .chars()
                .take(MAX_HEADER_VALUE_LENGTH)
                .collect::<String>()),
        );
    }
    Value::Object(out)
}

fn bounded_message(message: &str) -> String {
    message.chars().take(MAX_MESSAGE_LENGTH).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_provider::executor::AttemptTraceState;

    fn context() -> NetworkContext<'static> {
        NetworkContext {
            trace_id: "epoch-1",
            request_id: "req-1",
            query_source: "main_turn",
            provider_id: Some("provider-a"),
            model_id: "model-a",
            provider_kind: "openai-compatible",
            base_url: "https://example.test/v1",
            max_attempts: 3,
        }
    }

    fn trace(state: AttemptTraceState) -> AttemptTrace {
        AttemptTrace {
            attempt: 1,
            started_at_ms: 1_000,
            status: Some(200),
            request_headers: vec![("content-type".to_string(), "application/json".to_string())],
            response_headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
            first_content_at_ms: Some(1_120),
            ended_at_ms: Some(1_400),
            state,
            failure_kind: None,
            failure_message: None,
            stalled_timeout_ms: None,
            retry_delay_ms: None,
        }
    }

    fn usage(input: Option<u64>, output: Option<u64>, cache_read: Option<u64>) -> ModelUsage {
        ModelUsage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: None,
            cache_read_tokens: cache_read,
            cache_write_tokens: None,
            reasoning_tokens: None,
            server_tool_use: None,
        }
    }

    #[test]
    fn empty_ledger_is_the_strict_empty_snapshot() {
        let ledger = SessionDebugLedger::default();
        let snapshot = ledger.snapshot("s-1");
        assert_eq!(snapshot["sessionId"], json!("s-1"));
        assert_eq!(snapshot["rounds"], json!([]));
        assert_eq!(snapshot["networkEntries"], json!([]));
        assert_eq!(snapshot["cache"], Value::Null);
    }

    #[test]
    fn completed_request_records_started_completed_and_round_math() {
        let mut ledger = SessionDebugLedger::default();
        let usage = usage(Some(1000), Some(40), Some(600));
        ledger.record_request(
            &context(),
            &[trace(AttemptTraceState::Completed)],
            Some(RoundInput {
                request_id: "req-1",
                started_at_ms: 1_000,
                ended_at_ms: 1_400,
                first_content_at_ms: Some(1_120),
                usage: Some(&usage),
            }),
        );
        let snapshot = ledger.snapshot("s-1");
        let entries = snapshot["networkEntries"].as_array().expect("entries");
        assert_eq!(entries.len(), 2, "started + completed");
        assert_eq!(entries[0]["statusType"], json!("model_request_started"));
        assert_eq!(entries[1]["statusType"], json!("model_request_completed"));
        assert_eq!(entries[1]["durationMs"], json!(400));
        assert_eq!(
            entries[1]["responseHeaders"]["content-type"],
            json!("text/event-stream")
        );
        let rounds = snapshot["rounds"].as_array().expect("rounds");
        assert_eq!(rounds.len(), 1);
        let round = &rounds[0];
        assert_eq!(round["usage"]["inputTokens"], json!(1000));
        assert_eq!(round["usage"]["outputTokens"], json!(40));
        assert_eq!(round["usage"]["totalTokens"], json!(1040));
        assert_eq!(round["hitRate"], json!(0.6));
        // duration(400) - timeToFirst(120) = 280ms generation → 40*1000/280
        assert_eq!(round["generationDurationMs"], json!(280));
        let tps = round["tokensPerSecond"].as_f64().expect("tps");
        assert!((tps - 142.857).abs() < 0.01, "tps={tps}");
        let cache = &snapshot["cache"];
        assert_eq!(cache["hitRateRequestCount"], json!(1));
        assert_eq!(cache["totalInputTokens"], json!(1000));
        assert_eq!(cache["totalCacheReadTokens"], json!(600));
        assert_eq!(cache["hitRate"], json!(0.6));
    }

    #[test]
    fn unknown_cache_usage_keeps_cache_hit_rate_null() {
        let mut ledger = SessionDebugLedger::default();
        let usage = usage(None, Some(10), Some(500));
        ledger.record_request(
            &context(),
            &[],
            Some(RoundInput {
                request_id: "req-1",
                started_at_ms: 1_000,
                ended_at_ms: 1_200,
                first_content_at_ms: None,
                usage: Some(&usage),
            }),
        );
        let snapshot = ledger.snapshot("s-1");
        assert_eq!(snapshot["cache"]["hitRate"], Value::Null);
        let round = &snapshot["rounds"][0];
        assert_eq!(round["hitRate"], Value::Null);
        assert_eq!(round["generationDurationMs"], Value::Null);
        assert_eq!(round["tokensPerSecond"], Value::Null);
    }

    #[test]
    fn failed_and_stalled_attempts_record_reason_and_retry_schedule() {
        let mut ledger = SessionDebugLedger::default();
        let mut first = trace(AttemptTraceState::Failed);
        first.status = Some(500);
        first.failure_kind = Some("http_status");
        first.failure_message = Some("provider returned 500".to_string());
        first.retry_delay_ms = Some(8);
        let mut second = trace(AttemptTraceState::Failed);
        second.attempt = 2;
        second.status = None;
        second.failure_kind = Some("stream_idle_timeout");
        second.stalled_timeout_ms = Some(1234);
        second.failure_message = Some("Model stream stalled: no event received for 1234ms.".into());
        ledger.record_request(&context(), &[first, second], None);
        let snapshot = ledger.snapshot("s-1");
        let entries = snapshot["networkEntries"].as_array().expect("entries");
        let status_types: Vec<&str> = entries
            .iter()
            .filter_map(|entry| entry["statusType"].as_str())
            .collect();
        assert_eq!(
            status_types,
            vec![
                "model_request_started",
                "model_request_failed",
                "model_retry_scheduled",
                "model_request_started",
                "model_stream_stalled",
            ]
        );
        let failed = &entries[1];
        assert_eq!(failed["statusCode"], json!(500));
        assert_eq!(failed["reason"], json!("http_status"));
        assert_eq!(failed["message"], json!("provider returned 500"));
        let retry = &entries[2];
        assert_eq!(retry["delayMs"], json!(8));
        assert_eq!(retry["nextAttempt"], json!(2));
        let stalled = &entries[4];
        assert_eq!(stalled["timeoutMs"], json!(1234));
    }

    #[test]
    fn rounds_and_network_entries_are_bounded() {
        let mut ledger = SessionDebugLedger::default();
        let usage = usage(Some(10), Some(2), Some(5));
        for index in 0..(ROUNDS_LIMIT + 10) {
            let mut t = trace(AttemptTraceState::Completed);
            t.attempt = index as u32 + 1;
            ledger.record_request(
                &context(),
                &[t],
                Some(RoundInput {
                    request_id: "req-1",
                    started_at_ms: 1_000,
                    ended_at_ms: 1_400,
                    first_content_at_ms: Some(1_100),
                    usage: Some(&usage),
                }),
            );
        }
        // The same requestId records only one round (completedRequests dedupes).
        assert_eq!(
            ledger
                .snapshot("s")
                .get("rounds")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
        for index in 0..120 {
            let mut t = trace(AttemptTraceState::Started);
            t.attempt = index;
            t.ended_at_ms = None;
            t.started_at_ms = index as u64;
            ledger.record_attempt(&context(), &t);
        }
        let entries = ledger
            .snapshot("s")
            .get("networkEntries")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        assert!(
            entries <= NETWORK_LIMIT,
            "network entries bounded: {entries}"
        );
    }
}
