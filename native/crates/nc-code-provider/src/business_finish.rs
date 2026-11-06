//! finish 深层业务失败探测（对齐 `provider-finish-business-error.ts` 的
//! `detectProviderBusinessFinishError`：isFinishLikeRecord 门控 + hasBusinessSignal 判定 +
//! collectCandidateRecords 深扫 + 固定字段优先级取值）。
//!
//! 与 `business`（fetch 层浅读：无条件 per-frame `readProviderBusinessFailureFromBody` +
//! `event: error` 兜底）分层对应——TS 侧两次探测分处 fetch 层与 runner 层，语义不同：
//!
//! ```text
//! SSE 帧 ──▶ business::business_failure_from_json_frame（浅：顶层 code/success 硬信号）
//!        └─▶ 本模块 detect_business_failure_deep（深：finish 形状门控 + BFS 深扫嵌套码)
//! ```
//!
//! 深扫只在 `frame_carries_business_signal` 通过后执行（对应 TS `isFinishLikeRecord`
//! 只对 finish/error 形状的 typed chunk 生效），避免对每个内容帧做全树扫描。

use crate::business::ProviderBusinessFailure;
use serde_json::{Map, Value};

/// 深扫候选记录上限：TS `collectCandidateRecords` 无界；这里对单帧候选数设硬上限，
/// 防御异常/恶意大 JSON 把扫描拖成 O(n) 大树遍历。真实 provider 响应远小于此；
/// 偏差仅在超过 512 个嵌套对象的畸形帧上体现（见 README 已知差异）。
const DEEP_SCAN_NODE_CAP: usize = 512;

/// BigModel/Z.AI 业务码前缀：`[1302][...][request_id]`（对齐
/// BIGMODEL_BRACKETED_BUSINESS_CODE_PATTERN `/^\[(\d{4})\](?=\[)/`）。
const BIGMODEL_CODE_BRACKET_CLOSE: u8 = b']';
const BIGMODEL_CODE_LEN: usize = 4;
/// `[` + 4 数字 + `]` + 前瞻所需的下一个 `[`。
const BIGMODEL_CODE_FRAME_LEN: usize = 7;

/// `provider_success_false` 字面量（AI SDK 把 success:false 提升为 finish 原因的位置）。
const PROVIDER_SUCCESS_FALSE: &str = "provider_success_false";

/// finish 形状门控（wire 帧等价物，对齐 TS `isFinishLikeRecord` 的 typed-chunk 判定）：
/// 只有"这是一帧 finish/error 语义"时才做深扫。wire 帧没有平铺 `finishReason`，
/// 用 provider 形状逐一映射：openai `choices[0].finish_reason`、anthropic
/// `event: message_delta/message_stop` 与 `stop_reason`、错误帧 `error` 键/event。
pub fn frame_carries_business_signal(value: &Value, event: Option<&str>) -> bool {
    if event.is_some_and(|name| {
        let lower = name.to_ascii_lowercase();
        lower == "error" || lower == "finish" || lower == "message_delta" || lower == "message_stop"
    }) {
        return true;
    }
    let Some(record) = value.as_object() else {
        return false;
    };
    if let Some(kind) = record.get("type").and_then(Value::as_str) {
        if matches!(kind, "finish" | "error" | "message_delta" | "message_stop") {
            return true;
        }
    }
    // anthropic wire：message_delta 携带 stop_reason（typed chunk 才被映射成 finishReason）。
    if record.contains_key("error")
        || record.contains_key("finish_reason")
        || record.contains_key("rawFinishReason")
        || record.contains_key("stop_reason")
        || record.contains_key("success")
        || record.contains_key("providerCode")
        || record.contains_key("error_code")
    {
        return true;
    }
    if record
        .get("isProviderBusinessError")
        .and_then(Value::as_bool)
        == Some(true)
    {
        return true;
    }
    if record.get("name").and_then(Value::as_str) == Some("ProviderBusinessError") {
        return true;
    }
    // openai wire：finish_reason 位于 choices[0]。
    if choices_finish_reason(record).is_some() {
        return true;
    }
    record
        .get("providerMetadata")
        .and_then(Value::as_object)
        .is_some_and(|metadata| {
            metadata
                .get("rawFinishReason")
                .is_some_and(|value| !value.is_null())
        })
}

/// 深层业务失败扫描（对齐 `detectProviderBusinessFinishError`）。
///
/// 顺序：BFS 收集候选记录 → 按 TS 固定优先级逐字段取值（code/message/requestId/status
/// 独立首中即得，可来自不同候选记录）→ `hasBusinessSignal` 判定（仅 providerCode /
/// `provider_success_false` / 错误类标记构成业务失败；纯 message 不触发，避免把
/// "rate limited" 之类普通 Error 文案误判成业务错误）。
pub fn detect_business_failure_deep(value: &Value) -> Option<ProviderBusinessFailure> {
    let candidates = collect_candidate_records(value);
    let provider_code = candidates
        .iter()
        .find_map(|record| read_deep_provider_code(record));
    let provider_message = candidates
        .iter()
        .find_map(|record| read_deep_provider_message(record));
    let provider_request_id = candidates
        .iter()
        .find_map(|record| read_deep_request_id(record));
    let status_code = candidates
        .iter()
        .find_map(|record| read_deep_status(record));
    let body_error_type = candidates.iter().find_map(|record| {
        record
            .get("error")
            .and_then(|error| error.get("type"))
            .and_then(Value::as_str)
            .map(str::to_string)
    });

    let success_false = raw_finish_reason_is_provider_success_false(value);
    let flagged = value.as_object().is_some_and(|record| {
        record
            .get("isProviderBusinessError")
            .and_then(Value::as_bool)
            == Some(true)
            || record.get("name").and_then(Value::as_str) == Some("ProviderBusinessError")
    });
    if provider_code.is_none() && !success_false && !flagged {
        return None;
    }

    Some(ProviderBusinessFailure {
        status_code,
        provider_code,
        provider_message,
        provider_request_id,
        body_error_type,
    })
}

/// `collectCandidateRecords`：BFS 遍历候选记录。serde_json 树无环，用指针去重
/// 对应 TS `WeakSet`（同一子对象经固定字段链与全量 values 两次入队，首次弹出者生效，
/// 保持与 TS 相同的候选顺序）。固定字段链先于全量 values 入队（TS 同序）。
fn collect_candidate_records(value: &Value) -> Vec<&Map<String, Value>> {
    use std::collections::{HashSet, VecDeque};

    let mut result: Vec<&Map<String, Value>> = Vec::new();
    let mut seen: HashSet<usize> = HashSet::new();
    let mut queue: VecDeque<&Value> = VecDeque::from([value]);
    while let Some(current) = queue.pop_front() {
        if !seen.insert(std::ptr::from_ref(current) as usize) {
            continue;
        }
        match current {
            Value::Array(items) => queue.extend(items.iter()),
            Value::Object(record) => {
                if result.len() >= DEEP_SCAN_NODE_CAP {
                    break;
                }
                result.push(record);
                // 固定字段链（TS queue.push 顺序）：providerMetadata、response、
                // response.body、body、error、data、choices。
                push_key(&mut queue, record, "providerMetadata");
                push_key(&mut queue, record, "response");
                if let Some(response) = record.get("response") {
                    if let Some(body) = response.get("body") {
                        queue.push_back(body);
                    }
                }
                push_key(&mut queue, record, "body");
                push_key(&mut queue, record, "error");
                push_key(&mut queue, record, "data");
                push_key(&mut queue, record, "choices");
                // 兜底：nc-code-plan 等业务错误可能只出现在深层 JSON（TS 注释同款）。
                for nested in record.values() {
                    if nested.is_object() || nested.is_array() {
                        queue.push_back(nested);
                    }
                }
            }
            _ => {}
        }
    }
    result
}

fn push_key<'a>(
    queue: &mut std::collections::VecDeque<&'a Value>,
    record: &'a Map<String, Value>,
    key: &str,
) {
    if let Some(nested) = record.get(key) {
        queue.push_back(nested);
    }
}

/// `readProviderCode`：结构化码键（含 error/context 嵌套）→ BigModel 方括号前缀。
fn read_deep_provider_code(record: &Map<String, Value>) -> Option<String> {
    let error_record = record.get("error").and_then(Value::as_object);
    let context_record = record.get("context").and_then(Value::as_object);
    let nested = |source: Option<&Map<String, Value>>, key: &str| {
        source.and_then(|inner| read_code_like(inner.get(key)))
    };
    read_code_like(record.get("providerCode"))
        .or_else(|| nested(error_record, "providerCode"))
        .or_else(|| nested(context_record, "providerCode"))
        .or_else(|| read_code_like(record.get("error_code")))
        .or_else(|| nested(error_record, "error_code"))
        // 包装码在 read_code_like 内被丢弃（PROVIDER_BUSINESS_ERROR），不会提前截断。
        .or_else(|| read_code_like(record.get("code")))
        .or_else(|| nested(error_record, "code"))
        .or_else(|| nested(context_record, "code"))
        .or_else(|| read_bigmodel_bracketed_code(record.get("message")))
        .or_else(|| read_bigmodel_bracketed_code(record.get("providerMessage")))
        .or_else(|| {
            read_bigmodel_bracketed_code(error_record.and_then(|error| error.get("message")))
        })
        .or_else(|| {
            read_bigmodel_bracketed_code(
                error_record.and_then(|error| error.get("providerMessage")),
            )
        })
        .or_else(|| {
            read_bigmodel_bracketed_code(context_record.and_then(|context| context.get("message")))
        })
        .or_else(|| {
            read_bigmodel_bracketed_code(
                context_record.and_then(|context| context.get("providerMessage")),
            )
        })
}

/// `readProviderMessage`（finish 深扫版）：msg/providerMessage/message 三键 +
/// error/context 嵌套，按 TS 固定优先级。
fn read_deep_provider_message(record: &Map<String, Value>) -> Option<String> {
    let error_record = record.get("error").and_then(Value::as_object);
    let context_record = record.get("context").and_then(Value::as_object);
    let nested = |source: Option<&Map<String, Value>>, key: &str| {
        source.and_then(|inner| normalize_stringish(inner.get(key)))
    };
    normalize_stringish(record.get("msg"))
        .or_else(|| normalize_stringish(record.get("providerMessage")))
        .or_else(|| normalize_stringish(record.get("message")))
        .or_else(|| nested(error_record, "msg"))
        .or_else(|| nested(error_record, "providerMessage"))
        .or_else(|| nested(error_record, "message"))
        .or_else(|| nested(context_record, "providerMessage"))
        .or_else(|| nested(context_record, "msg"))
        .or_else(|| nested(context_record, "message"))
}

/// `readProviderRequestId`（finish 深扫版）：request_id/requestId/id + error 嵌套。
fn read_deep_request_id(record: &Map<String, Value>) -> Option<String> {
    let error_record = record.get("error").and_then(Value::as_object);
    let nested = |source: Option<&Map<String, Value>>, key: &str| {
        source.and_then(|inner| normalize_stringish(inner.get(key)))
    };
    normalize_stringish(record.get("request_id"))
        .or_else(|| normalize_stringish(record.get("requestId")))
        .or_else(|| normalize_stringish(record.get("id")))
        .or_else(|| nested(error_record, "request_id"))
        .or_else(|| nested(error_record, "requestId"))
        .or_else(|| nested(error_record, "id"))
}

/// `readStatusCode`：statusCode/status 有限数字（首个候选的首中有值者）。
/// Rust 侧收敛到 `u16`（状态码语义域）；超出域的值按无状态处理——classify 的
/// effective_status 只消费 4xx/5xx，超域值在 TS 侧同样落不进判定。
fn read_deep_status(record: &Map<String, Value>) -> Option<u16> {
    read_finite_u16(record.get("statusCode")).or_else(|| read_finite_u16(record.get("status")))
}

fn read_finite_u16(value: Option<&Value>) -> Option<u16> {
    let value = value?;
    let number = value.as_f64()?;
    if number.is_finite() && number.fract() == 0.0 && (0.0..=u16::MAX as f64).contains(&number) {
        Some(number as u16)
    } else {
        None
    }
}

/// `readRawFinishReason` 等价 + wire 映射：typed chunk 的 `rawFinishReason`（顶层或
/// providerMetadata）来自 wire 的 `rawFinishReason`；AI SDK 亦会原样携带
/// `finish_reason`，故二者任一等于 `provider_success_false` 即视为命中。
fn raw_finish_reason_is_provider_success_false(value: &Value) -> bool {
    let Some(record) = value.as_object() else {
        return false;
    };
    if record.get("rawFinishReason").and_then(Value::as_str) == Some(PROVIDER_SUCCESS_FALSE) {
        return true;
    }
    if record
        .get("providerMetadata")
        .and_then(Value::as_object)
        .is_some_and(|metadata| {
            metadata.get("rawFinishReason").and_then(Value::as_str) == Some(PROVIDER_SUCCESS_FALSE)
        })
    {
        return true;
    }
    if record.get("finish_reason").and_then(Value::as_str) == Some(PROVIDER_SUCCESS_FALSE) {
        return true;
    }
    choices_finish_reason(record).is_some_and(|reason| reason == PROVIDER_SUCCESS_FALSE)
}

fn choices_finish_reason(record: &Map<String, Value>) -> Option<&str> {
    record
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(Value::as_str)
}

/// 深扫码键读取（对齐 `normalizeProviderCode`）：数字按 JS `normalizeStringish`
/// 截断为整数串，字符串 trim 后非空，包装码丢弃。
fn read_code_like(value: Option<&Value>) -> Option<String> {
    let value = value?;
    let normalized = normalize_stringish(Some(value))?;
    if normalized.eq_ignore_ascii_case(super::business::PROVIDER_BUSINESS_ERROR_WRAPPER_CODE) {
        return None;
    }
    Some(normalized)
}

/// `normalizeStringish`：有限数字截断为整数字符串；字符串 trim 非空。
pub(crate) fn normalize_stringish(value: Option<&Value>) -> Option<String> {
    let value = value?;
    match value {
        Value::Number(number) => {
            let float = number.as_f64()?;
            if !float.is_finite() {
                return None;
            }
            Some(float.trunc().to_string())
        }
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        _ => None,
    }
}

/// `readBigModelBracketedBusinessCode`：`[dddd][` 强格式前缀手工字节判定
/// （不引入 regex 依赖；lookahead `(?=\[)` 由第 6 字节必须是 `[` 表达）。
fn read_bigmodel_bracketed_code(value: Option<&Value>) -> Option<String> {
    let message = normalize_stringish(value)?;
    let bytes = message.as_bytes();
    if bytes.len() >= BIGMODEL_CODE_FRAME_LEN
        && bytes[0] == b'['
        && bytes[1..=BIGMODEL_CODE_LEN].iter().all(u8::is_ascii_digit)
        && bytes[1 + BIGMODEL_CODE_LEN] == BIGMODEL_CODE_BRACKET_CLOSE
        && bytes[2 + BIGMODEL_CODE_LEN] == b'['
    {
        return Some(message[1..=BIGMODEL_CODE_LEN].to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn gate_accepts_finish_like_wire_shapes() {
        // openai finish 帧
        let openai_finish = json!({"choices": [{"finish_reason": "stop", "delta": {}}]});
        assert!(frame_carries_business_signal(&openai_finish, None));
        // anthropic message_delta（stop_reason）
        let anthropic_delta =
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}});
        assert!(frame_carries_business_signal(
            &anthropic_delta,
            Some("message_delta")
        ));
        // error 键 / event:error
        let error_frame = json!({"error": {"message": "boom"}});
        assert!(frame_carries_business_signal(&error_frame, None));
        assert!(frame_carries_business_signal(
            &json!({"message": "m"}),
            Some("error")
        ));
        // 声明型标记
        assert!(frame_carries_business_signal(
            &json!({"name": "ProviderBusinessError"}),
            None
        ));
    }

    #[test]
    fn gate_rejects_plain_content_frames() {
        let delta = json!({"choices": [{"delta": {"content": "hi"}}]});
        assert!(!frame_carries_business_signal(&delta, None));
        let message_start = json!({"type": "message_start", "message": {"id": "m1"}});
        assert!(!frame_carries_business_signal(
            &message_start,
            Some("message_start")
        ));
        assert!(!frame_carries_business_signal(
            &json!("not an object"),
            None
        ));
    }

    #[test]
    fn deep_finds_nested_code_under_response_body() {
        // nc-code-plan 场景：3007 只出现在 finish 深层 JSON。
        let frame = json!({
            "type": "finish",
            "finishReason": "error",
            "providerMetadata": {"response": {"body": {"error": {"code": 3007, "message": "denied"}}}}
        });
        let failure = detect_business_failure_deep(&frame).expect("nested 3007 must be found");
        assert_eq!(failure.provider_code.as_deref(), Some("3007"));
        assert_eq!(
            failure.provider_message.as_deref(),
            Some("denied"),
            "message 取自同一候选记录的 error.message"
        );
    }

    #[test]
    fn deep_extracts_bigmodel_bracketed_code_from_message() {
        let frame = json!({
            "type": "error",
            "error": {"message": "[1302][quota][req-1] exhausted"}
        });
        let failure = detect_business_failure_deep(&frame).expect("bracketed code must fire");
        assert_eq!(failure.provider_code.as_deref(), Some("1302"));
        assert_eq!(
            failure.provider_message.as_deref(),
            Some("[1302][quota][req-1] exhausted")
        );
    }

    #[test]
    fn deep_message_only_frame_is_not_business() {
        // 纯 message（如 "rate limited"）没有业务码，不构成 hasBusinessSignal。
        let frame = json!({"type": "error", "error": {"message": "rate limited"}});
        assert!(detect_business_failure_deep(&frame).is_none());
    }

    #[test]
    fn deep_provider_success_false_uses_default_message() {
        let frame = json!({
            "type": "finish",
            "rawFinishReason": "provider_success_false",
            "providerMetadata": {"rawFinishReason": "provider_success_false"}
        });
        let failure = detect_business_failure_deep(&frame).expect("psf must fire");
        assert!(failure.provider_code.is_none());
        assert_eq!(failure.message(), "Provider returned a business error.");
    }

    #[test]
    fn deep_wrapper_code_does_not_count_as_signal() {
        // 外层 code 是 ZCode 包装类型；真实码缺省时不得凭包装码触发。
        let frame = json!({
            "type": "finish",
            "code": "PROVIDER_BUSINESS_ERROR",
            "message": "wrapped"
        });
        assert!(detect_business_failure_deep(&frame).is_none());
    }

    #[test]
    fn deep_status_prefers_status_code_then_status() {
        let frame = json!({
            "type": "error",
            "statusCode": 429,
            "status": 500,
            "error": {"providerCode": "3002"}
        });
        let failure = detect_business_failure_deep(&frame).expect("code present");
        assert_eq!(failure.provider_code.as_deref(), Some("3002"));
        assert_eq!(failure.status_code, Some(429));
    }

    #[test]
    fn deep_scan_stops_at_node_cap() {
        // 构造 600 层嵌套链，最内层带 3007：超过 512 候选上限后不再下钻。
        let mut node = json!({"code": 3007});
        for depth in 0..600 {
            node = json!({"level": depth, "nested": node});
        }
        assert!(
            detect_business_failure_deep(&node).is_none(),
            "内层码位于 cap 之外，扫描必须提前停止"
        );
    }

    #[test]
    fn deep_scan_within_cap_still_finds_code() {
        let mut node = json!({"code": 3007});
        for depth in 0..100 {
            node = json!({"level": depth, "nested": node});
        }
        let failure = detect_business_failure_deep(&node).expect("within cap");
        assert_eq!(failure.provider_code.as_deref(), Some("3007"));
    }
}
