//! Cross-session app-usage aggregation (`v4/usage/stats`).
//!
//! Port of `apps/zcode-cli/packages/bootstrap/src/zcode-protocol/usage-stats-builder.ts`
//! with the same field semantics:
//! - `totalTokens = input + output` (cache tokens stay a breakdown, never added to the
//!   denominator of the hit rate — TS comment in the builder);
//! - `cacheHitRate = cacheRead / input` (falls back to `cacheRead / (creation+read)` only
//!   when the input total is 0);
//! - day buckets are **local** day indexes (`(at + tzOffset) / DAY`), so streaks and the
//!   heatmap follow the caller's timeZone, not UTC;
//! - `levelFor`: 0 for no usage, else 1–4 by share of the busiest day (25/50/75 %).
//!
//! The input is the durable per-turn usage facts written by the host
//! (`nc-code-host::turn::persist_usage_fact` → `TranscriptEntry::Usage`); nothing here
//! invents a number.

use chrono::{NaiveDate, Offset, TimeZone, Utc};
use chrono_tz::Tz;
use serde_json::{json, Value};

const DAY_MS: i64 = 86_400_000;

/// One per-turn usage fact (the durable record the host appends).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageFact {
    pub session_id: String,
    pub model_id: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub tool_call_count: u64,
    pub tool_error_count: u64,
    pub tool_total_ms: u64,
    pub started_at: i64,
    pub ended_at: i64,
    pub duration_ms: i64,
    pub model_request_count: u64,
    pub model_error_count: u64,
    /// Tool rows of this turn (name + status + duration), so the tool ranking does
    /// not need a second scan of the transcript.
    pub tools: Vec<UsageToolCall>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageToolCall {
    pub name: String,
    pub error: bool,
    pub duration_ms: u64,
}

impl UsageFact {
    /// Parse one transcript `usage` entry (`{"t":"usage","fact":{...}}` payload).
    /// Unknown/missing fields fall back to 0 — a fact is only trusted for what it
    /// actually recorded (no inferred token counts).
    pub fn from_value(fact: &Value) -> Option<Self> {
        let session_id = fact.get("sessionId")?.as_str()?.to_string();
        let number = |key: &str| fact.get(key).and_then(Value::as_u64).unwrap_or(0);
        let signed = |key: &str| fact.get(key).and_then(Value::as_i64).unwrap_or(0);
        let tools = fact
            .get("tools")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|tool| {
                        Some(UsageToolCall {
                            name: tool.get("name")?.as_str()?.to_string(),
                            error: tool.get("error").and_then(Value::as_bool).unwrap_or(false),
                            duration_ms: tool
                                .get("durationMs")
                                .and_then(Value::as_u64)
                                .unwrap_or(0),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(Self {
            session_id,
            model_id: fact
                .get("modelId")
                .and_then(Value::as_str)
                .map(str::to_string),
            input_tokens: number("inputTokens"),
            output_tokens: number("outputTokens"),
            reasoning_tokens: number("reasoningTokens"),
            cache_read_tokens: number("cacheReadTokens"),
            cache_creation_tokens: number("cacheCreationTokens"),
            tool_call_count: number("toolCallCount"),
            tool_error_count: number("toolErrorCount"),
            tool_total_ms: number("toolTotalMs"),
            started_at: signed("startedAt"),
            ended_at: signed("endedAt"),
            duration_ms: signed("durationMs"),
            model_request_count: number("modelRequestCount"),
            model_error_count: number("modelErrorCount"),
            tools,
        })
    }
}

/// Time-zone resolution: an unknown/blank name falls back to UTC (the TS builder's
/// `resolveTzOffsetMs` also degrades to 0 rather than failing the whole snapshot).
fn resolve_tz_offset_ms(time_zone: &str, at_ms: i64) -> i64 {
    let Ok(tz) = time_zone.parse::<Tz>() else {
        return 0;
    };
    let Some(instant) = Utc.timestamp_millis_opt(at_ms).single() else {
        return 0;
    };
    i64::from(
        tz.offset_from_utc_datetime(&instant.naive_utc())
            .fix()
            .local_minus_utc(),
    ) * 1000
}

fn day_index(at_ms: i64, tz_offset_ms: i64) -> i64 {
    (at_ms + tz_offset_ms) / DAY_MS
}

fn day_index_to_date(day_index: i64) -> String {
    // `day_index` counts days since 1970-01-01 (the epoch), so anchor there before adding.
    let Some(epoch) = NaiveDate::from_ymd_opt(1970, 1, 1) else {
        return String::new();
    };
    let Some(date) = epoch.checked_add_signed(chrono::Duration::days(day_index)) else {
        return String::new();
    };
    date.format("%Y-%m-%d").to_string()
}

fn level_for(tokens: u64, max: u64) -> u8 {
    if tokens == 0 || max == 0 {
        return 0;
    }
    let ratio = tokens as f64 / max as f64;
    if ratio > 0.75 {
        4
    } else if ratio > 0.5 {
        3
    } else if ratio > 0.25 {
        2
    } else {
        1
    }
}

fn now_utc_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// Build the `appUsageSnapshotSchema` payload.
///
/// `range` is `all` / `7d` / `30d`; `since` is the first instant considered (already
/// computed by the caller from the range) and `until` the last (usually now).
pub fn build_app_usage_snapshot(
    facts: &[UsageFact],
    range: &str,
    time_zone: &str,
    since_ms: i64,
    until_ms: i64,
) -> Value {
    let tz_offset = resolve_tz_offset_ms(time_zone, until_ms);
    let generated_at = now_utc_ms();

    let mut totals = Totals::default();
    let mut day_tokens = std::collections::BTreeMap::<i64, DayBucket>::new();
    let mut day_models =
        std::collections::BTreeMap::<i64, std::collections::BTreeMap<String, u64>>::new();
    let mut model_usage = std::collections::BTreeMap::<String, ModelBucket>::new();
    let mut tool_usage = std::collections::BTreeMap::<String, ToolBucket>::new();
    let mut sessions = std::collections::BTreeSet::<String>::new();
    let mut longest_session_ms: i64 = 0;
    let mut turn_durations: Vec<i64> = Vec::new();

    for fact in facts {
        if fact.ended_at < since_ms || fact.started_at > until_ms {
            continue;
        }
        sessions.insert(fact.session_id.clone());
        let input = fact.input_tokens;
        let output = fact.output_tokens;
        let total = input.saturating_add(output);
        totals.input_tokens += input;
        totals.output_tokens += output;
        totals.reasoning_tokens += fact.reasoning_tokens;
        totals.cache_read_tokens += fact.cache_read_tokens;
        totals.cache_creation_tokens += fact.cache_creation_tokens;
        totals.turns += 1;
        totals.model_requests += fact.model_request_count;
        totals.model_errors += fact.model_error_count;
        totals.tool_calls += fact.tool_call_count;
        totals.tool_errors += fact.tool_error_count;
        if fact.duration_ms > 0 {
            turn_durations.push(fact.duration_ms);
        }
        let day = day_index(fact.ended_at.max(fact.started_at), tz_offset);
        let bucket = day_tokens.entry(day).or_default();
        bucket.tokens += total;
        bucket.turns += 1;
        bucket.tools += fact.tool_call_count;
        if let Some(model) = &fact.model_id {
            *day_models
                .entry(day)
                .or_default()
                .entry(model.clone())
                .or_insert(0) += total;
            let entry = model_usage.entry(model.clone()).or_default();
            entry.input += input;
            entry.output += output;
            entry.tokens += total;
            entry.requests += fact.model_request_count;
        }
        for tool in &fact.tools {
            let entry = tool_usage.entry(tool.name.clone()).or_default();
            entry.calls += 1;
            if tool.error {
                entry.errors += 1;
            }
            entry.total_ms += tool.duration_ms;
            entry.durations.push(tool.duration_ms as i64);
        }
    }
    longest_session_ms = turn_durations
        .iter()
        .copied()
        .max()
        .unwrap_or(longest_session_ms);

    let cache_denom = if totals.input_tokens > 0 {
        totals.input_tokens
    } else {
        totals.cache_creation_tokens + totals.cache_read_tokens
    };
    let cache_hit_rate = if cache_denom > 0 {
        totals.cache_read_tokens as f64 / cache_denom as f64
    } else {
        0.0
    };
    let model_error_rate = if totals.model_requests > 0 {
        totals.model_errors as f64 / totals.model_requests as f64
    } else {
        0.0
    };
    let tool_error_rate = if totals.tool_calls > 0 {
        totals.tool_errors as f64 / totals.tool_calls as f64
    } else {
        0.0
    };
    let avg_turn_duration_ms = if turn_durations.is_empty() {
        Value::Null
    } else {
        json!(turn_durations.iter().sum::<i64>() as f64 / turn_durations.len() as f64)
    };

    let end_day = day_index(until_ms, tz_offset);
    let start_day = if range == "all" {
        day_tokens
            .keys()
            .copied()
            .chain(day_models.keys().copied())
            .min()
            .unwrap_or(end_day)
    } else {
        day_index(since_ms, tz_offset)
    };

    // Streaks: walk backwards from the end day. `current` stops at the first gap;
    // `longest` is the longest uninterrupted run anywhere in the window.
    let mut active_days = 0u64;
    let mut current_streak = 0u64;
    let mut longest_streak = 0u64;
    let mut running = 0u64;
    let mut broken = false;
    for di in (start_day..=end_day).rev() {
        let tokens = day_tokens.get(&di).map(|bucket| bucket.tokens).unwrap_or(0);
        if tokens > 0 {
            active_days += 1;
            running += 1;
            longest_streak = longest_streak.max(running);
            if !broken {
                current_streak += 1;
            }
        } else {
            broken = true;
            running = 0;
        }
    }

    let max_tokens = day_tokens
        .values()
        .map(|bucket| bucket.tokens)
        .max()
        .unwrap_or(0);

    // Heatmap: 7-day weeks from start to end, the trailing week padded with nulls.
    let mut weeks: Vec<Value> = Vec::new();
    let mut week: Vec<Value> = Vec::new();
    for di in start_day..=end_day {
        let bucket = day_tokens.get(&di);
        week.push(json!({
            "date": day_index_to_date(di),
            "level": level_for(bucket.map(|b| b.tokens).unwrap_or(0), max_tokens),
            "totalTokens": bucket.map(|b| b.tokens).unwrap_or(0),
            "turnCount": bucket.map(|b| b.turns).unwrap_or(0),
            "toolCallCount": bucket.map(|b| b.tools).unwrap_or(0),
        }));
        if week.len() == 7 {
            weeks.push(json!({"weekIndex": weeks.len(), "days": week}));
            week = Vec::new();
        }
    }
    if !week.is_empty() {
        while week.len() < 7 {
            week.push(Value::Null);
        }
        weeks.push(json!({"weekIndex": weeks.len(), "days": week}));
    }

    let daily_model_usage: Vec<Value> = (start_day..=end_day)
        .map(|di| {
            let models = day_models
                .get(&di)
                .map(|inner| {
                    inner
                        .iter()
                        .map(|(model_id, total_tokens)| {
                            json!({"modelId": model_id, "totalTokens": total_tokens})
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            json!({"date": day_index_to_date(di), "models": models})
        })
        .collect();

    let total_model_tokens: u64 = model_usage.values().map(|bucket| bucket.tokens).sum();
    let models: Vec<Value> = model_usage
        .iter()
        .map(|(model_id, bucket)| {
            json!({
                "modelId": model_id,
                "totalTokens": bucket.tokens,
                "inputTokens": bucket.input,
                "outputTokens": bucket.output,
                "requestCount": bucket.requests,
                "share": if total_model_tokens > 0 {
                    bucket.tokens as f64 / total_model_tokens as f64
                } else {
                    0.0
                },
            })
        })
        .collect();
    let favorite_model = model_usage
        .iter()
        .max_by_key(|(_, bucket)| bucket.tokens)
        .map(|(model_id, bucket)| {
            json!({
                "modelId": model_id,
                "totalTokens": bucket.tokens,
                "share": if total_model_tokens > 0 {
                    bucket.tokens as f64 / total_model_tokens as f64
                } else {
                    0.0
                },
            })
        })
        .unwrap_or(Value::Null);

    let tools: Vec<Value> = tool_usage
        .iter()
        .map(|(tool_name, bucket)| {
            let avg = if bucket.durations.is_empty() {
                Value::Null
            } else {
                json!(bucket.durations.iter().sum::<i64>() as f64 / bucket.durations.len() as f64)
            };
            json!({
                "toolName": tool_name,
                "callCount": bucket.calls,
                "errorCount": bucket.errors,
                "errorRate": if bucket.calls > 0 {
                    bucket.errors as f64 / bucket.calls as f64
                } else {
                    0.0
                },
                "avgDurationMs": avg,
            })
        })
        .collect();

    json!({
        "range": range,
        "generatedAt": generated_at,
        "timeZone": time_zone,
        "source": "agent-db",
        "summary": {
            "totalTokens": totals.input_tokens.saturating_add(totals.output_tokens),
            "inputTokens": totals.input_tokens,
            "outputTokens": totals.output_tokens,
            "reasoningTokens": totals.reasoning_tokens,
            "cacheCreationTokens": totals.cache_creation_tokens,
            "cacheReadTokens": totals.cache_read_tokens,
            "cacheHitRate": cache_hit_rate,
            "totalSessions": sessions.len() as u64,
            "totalTurns": totals.turns,
            "toolCallCount": totals.tool_calls,
            "toolErrorRate": tool_error_rate,
            "modelErrorRate": model_error_rate,
            "avgTimeToFirstTokenMs": Value::Null,
            "avgTurnDurationMs": avg_turn_duration_ms,
            "activeDays": active_days,
            "currentStreakDays": current_streak,
            "longestSessionMs": longest_session_ms,
            "longestStreakDays": longest_streak,
            "peakDayTokens": max_tokens,
            "favoriteModel": favorite_model,
        },
        "heatmap": {
            "startDate": if day_tokens.is_empty() { Value::Null } else { json!(day_index_to_date(start_day)) },
            "endDate": if day_tokens.is_empty() { Value::Null } else { json!(day_index_to_date(end_day)) },
            "maxTokens": max_tokens,
            "weeks": weeks,
        },
        "dailyModelUsage": daily_model_usage,
        "models": models,
        "tools": tools,
    })
}

#[derive(Default)]
struct Totals {
    input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    turns: u64,
    model_requests: u64,
    model_errors: u64,
    tool_calls: u64,
    tool_errors: u64,
}

#[derive(Default)]
struct DayBucket {
    tokens: u64,
    turns: u64,
    tools: u64,
}

#[derive(Default)]
struct ModelBucket {
    tokens: u64,
    input: u64,
    output: u64,
    requests: u64,
}

#[derive(Default)]
struct ToolBucket {
    calls: u64,
    errors: u64,
    total_ms: u64,
    durations: Vec<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(
        session: &str,
        model: Option<&str>,
        input: u64,
        output: u64,
        end_ms: i64,
        tools: Vec<UsageToolCall>,
    ) -> UsageFact {
        UsageFact {
            session_id: session.to_string(),
            model_id: model.map(str::to_string),
            input_tokens: input,
            output_tokens: output,
            reasoning_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_call_count: tools.len() as u64,
            tool_error_count: tools.iter().filter(|t| t.error).count() as u64,
            tool_total_ms: tools.iter().map(|t| t.duration_ms).sum(),
            started_at: end_ms - 1000,
            ended_at: end_ms,
            duration_ms: 1000,
            model_request_count: 1,
            model_error_count: 0,
            tools,
        }
    }

    #[test]
    fn aggregates_tokens_models_and_tools_across_sessions() {
        let facts = vec![
            fact(
                "s1",
                Some("deepseek"),
                100,
                50,
                1_700_000_000_000,
                vec![UsageToolCall {
                    name: "Read".into(),
                    error: false,
                    duration_ms: 40,
                }],
            ),
            fact(
                "s2",
                Some("deepseek"),
                200,
                100,
                1_700_000_000_000,
                vec![
                    UsageToolCall {
                        name: "Read".into(),
                        error: true,
                        duration_ms: 60,
                    },
                    UsageToolCall {
                        name: "Bash".into(),
                        error: false,
                        duration_ms: 120,
                    },
                ],
            ),
        ];
        // The two facts land on the same UTC day.
        let snapshot =
            build_app_usage_snapshot(&facts, "all", "UTC", 1_700_000_000_000, 1_700_000_000_000);
        let summary = &snapshot["summary"];
        assert_eq!(summary["totalTokens"], json!(450));
        assert_eq!(summary["inputTokens"], json!(300));
        assert_eq!(summary["totalSessions"], json!(2));
        assert_eq!(summary["totalTurns"], json!(2));
        assert_eq!(summary["toolCallCount"], json!(3));
        assert_eq!(summary["favoriteModel"]["modelId"], json!("deepseek"));
        assert_eq!(snapshot["models"][0]["requestCount"], json!(2));
        // Read: 2 calls, 1 error -> 0.5; Bash: 1 call, 0 errors.
        let read = snapshot["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["toolName"] == json!("Read"))
            .unwrap();
        assert_eq!(read["callCount"], json!(2));
        assert_eq!(read["errorCount"], json!(1));
        assert!((read["errorRate"].as_f64().unwrap() - 0.5).abs() < f64::EPSILON);
        // Heatmap is a real bucket for that day, not an empty week list.
        assert_eq!(snapshot["heatmap"]["maxTokens"], json!(450));
        assert_eq!(
            snapshot["heatmap"]["weeks"][0]["days"][0]["level"],
            json!(4)
        );
    }

    #[test]
    fn range_filters_and_streaks_follow_local_days() {
        // 09:00 IST on 2026-11-06 and 09:00 IST on 2026-11-08 (different dates, one gap).
        let day = 86_400_000i64;
        let base = 1_781_000_000_000i64; // arbitrary epoch base
        let facts = vec![
            UsageFact {
                ended_at: base,
                started_at: base - 1000,
                ..fact("s1", Some("m"), 10, 10, base, vec![])
            },
            UsageFact {
                ended_at: base + 2 * day,
                started_at: base + 2 * day - 1000,
                ..fact("s1", Some("m"), 10, 10, base + 2 * day, vec![])
            },
        ];
        let snapshot =
            build_app_usage_snapshot(&facts, "all", "Asia/Kolkata", base, base + 2 * day);
        let summary = &snapshot["summary"];
        assert_eq!(summary["activeDays"], json!(2));
        assert_eq!(
            summary["currentStreakDays"],
            json!(1),
            "today is active, yesterday is not"
        );
        assert_eq!(summary["longestStreakDays"], json!(1));
        // 7d keeps only the last day's fact.
        let snapshot =
            build_app_usage_snapshot(&facts, "7d", "Asia/Kolkata", base + day, base + 2 * day);
        assert_eq!(snapshot["summary"]["totalTurns"], json!(1));
    }

    #[test]
    fn zero_usage_yields_contract_shape_without_inventing_numbers() {
        let snapshot = build_app_usage_snapshot(&[], "all", "UTC", 0, 1_700_000_000_000);
        let summary = &snapshot["summary"];
        assert_eq!(summary["totalTokens"], json!(0));
        assert_eq!(summary["totalSessions"], json!(0));
        assert_eq!(summary["favoriteModel"], Value::Null);
        assert_eq!(snapshot["heatmap"]["maxTokens"], json!(0));
        assert_eq!(snapshot["heatmap"]["startDate"], Value::Null);
        assert!(snapshot["heatmap"]["weeks"].is_array());
        assert_eq!(snapshot["source"], json!("agent-db"));
        assert_eq!(snapshot["timeZone"], json!("UTC"));
    }

    #[test]
    fn cache_hit_rate_uses_input_denominator() {
        let mut one = fact("s1", Some("m"), 100, 10, 1_700_000_000_000, vec![]);
        one.cache_read_tokens = 40;
        one.cache_creation_tokens = 30;
        let snapshot = build_app_usage_snapshot(&[one], "all", "UTC", 0, 1_700_000_000_100);
        // 40 / 100 (cache tokens are a breakdown, not added to the denominator).
        assert!((snapshot["summary"]["cacheHitRate"].as_f64().unwrap() - 0.4).abs() < 1e-9);
    }
}
