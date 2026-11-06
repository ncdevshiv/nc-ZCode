//! SSE 帧切分与字段解析。
//!
//! 对齐 TS：帧分隔 `/\r\n\r\n|\n\n|\r\r/`（model-execution.ts `SSE_FRAME_SEPARATOR_PATTERN`），
//! 行切分 `/\r\n|\n|\r/` + 单个前导空格剥离（`readSseFrame`）；按任务要求额外结构化读取
//! `id:`/`retry:` 与注释行。JSON 层对齐 provider-utils `parseJsonEventStream`：
//! `data` 与 `"[DONE]"` **精确相等**时静默丢弃（不 trim），无 `data` 字段不产生载荷。

use serde_json::Value;
use std::fmt;

/// 一条完整 SSE 帧的字段解析结果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    /// `event:` 字段；无则 None（TS 可选返回，不默认 "message"）。
    pub event: Option<String>,
    /// 多条 `data:` 以 `\n` 连接；一条都没有时为 None。
    pub data: Option<String>,
    pub id: Option<String>,
    /// `retry:` 字段；非数字时忽略（None）。
    pub retry: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
}

impl SseParser {
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// 喂入任意字节切片；返回所有以完整分隔符结尾的帧。
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        self.drain_frames(false)
    }

    /// 流结束：缓冲区中不足一个分隔符的尾帧按完整帧处理（TS flush 分支同语义）。
    pub fn finish(&mut self) -> Vec<SseEvent> {
        self.drain_frames(true)
    }

    fn drain_frames(&mut self, emit_trailing: bool) -> Vec<SseEvent> {
        let mut events = Vec::new();
        while let Some((index, sep_len)) = find_separator(&self.buffer) {
            let frame_bytes: Vec<u8> = self.buffer.drain(..index).collect();
            self.buffer.drain(..sep_len);
            events.push(parse_frame_lossy(&frame_bytes));
        }
        if emit_trailing && !self.buffer.is_empty() {
            let rest: Vec<u8> = std::mem::take(&mut self.buffer);
            events.push(parse_frame_lossy(&rest));
        }
        events
    }
}

/// 在字节流中定位最早出现的分隔符（顺序与正则交替一致：4 字节优先）。
fn find_separator(buf: &[u8]) -> Option<(usize, usize)> {
    for i in 0..buf.len() {
        if buf[i..].starts_with(b"\r\n\r\n") {
            return Some((i, 4));
        }
        if buf[i..].starts_with(b"\n\n") || buf[i..].starts_with(b"\r\r") {
            return Some((i, 2));
        }
    }
    None
}

/// 帧字节按 UTF-8 lossy 解码后解析字段（provider SSE 均为 ASCII/UTF-8 JSON）。
fn parse_frame_lossy(frame: &[u8]) -> SseEvent {
    parse_sse_frame(&String::from_utf8_lossy(frame))
}

/// 解析单帧字段（`readSseFrame` 的移植 + id/retry 补齐）。
pub fn parse_sse_frame(frame: &str) -> SseEvent {
    let mut event = None;
    let mut id = None;
    let mut retry = None;
    let mut data_lines: Vec<&str> = Vec::new();

    for line in split_lines_ts(frame) {
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(trim_one_space(rest).to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(trim_one_space(rest));
        } else if let Some(rest) = line.strip_prefix("id:") {
            id = Some(trim_one_space(rest).to_string());
        } else if let Some(rest) = line.strip_prefix("retry:") {
            // 非法 retry 值静默忽略，不破坏整帧。
            retry = trim_one_space(rest).parse::<u64>().ok();
        }
        // 注释行（`:` 开头）与其它字段按 TS 语义跳过。
    }

    SseEvent {
        event,
        data: if data_lines.is_empty() {
            None
        } else {
            Some(data_lines.join("\n"))
        },
        id,
        retry,
    }
}

/// 只剥离一个前导空格（TS `trimSseFieldValue`；不动 Tab）。
fn trim_one_space(value: &str) -> &str {
    value.strip_prefix(' ').unwrap_or(value)
}

/// 按 `/\r\n|\n|\r/` 切行（保留切分语义：`a\rb` → ["a","b"]）。
fn split_lines_ts(frame: &str) -> Vec<&str> {
    let bytes = frame.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let sep = if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            Some(2usize)
        } else if bytes[i] == b'\n' || bytes[i] == b'\r' {
            Some(1usize)
        } else {
            None
        };
        if let Some(len) = sep {
            lines.push(&frame[start..i]);
            i += len;
            start = i;
        } else {
            i += 1;
        }
    }
    lines.push(&frame[start..]);
    lines
}

// ============================================================
// JSON 事件层
// ============================================================

#[derive(Debug)]
pub enum SseError {
    /// 帧 `data` 不是合法 JSON（对应 AI SDK 的失败 chunk → error part）。
    Json { message: String },
}

impl fmt::Display for SseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SseError::Json { message } => write!(f, "SSE data is not valid JSON: {message}"),
        }
    }
}

impl std::error::Error for SseError {}

/// 通过 JSON 校验的一帧（`data: [DONE]` 与无 data 帧已在上游丢弃）。
#[derive(Debug, Clone, PartialEq)]
pub struct JsonSseFrame {
    pub event: Option<String>,
    pub value: Value,
}

#[derive(Debug, Clone, Default)]
pub struct JsonSseStream {
    parser: SseParser,
}

impl JsonSseStream {
    pub fn new() -> Self {
        Self {
            parser: SseParser::new(),
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<JsonSseFrame>, SseError> {
        let events = self.parser.feed(chunk);
        collect_json_frames(events)
    }

    pub fn finish(&mut self) -> Result<Vec<JsonSseFrame>, SseError> {
        let events = self.parser.finish();
        collect_json_frames(events)
    }
}

fn collect_json_frames(events: Vec<SseEvent>) -> Result<Vec<JsonSseFrame>, SseError> {
    let mut frames = Vec::with_capacity(events.len());
    for event in events {
        if let Some(frame) = frame_to_json(event)? {
            frames.push(frame);
        }
    }
    Ok(frames)
}

fn frame_to_json(event: SseEvent) -> Result<Option<JsonSseFrame>, SseError> {
    let Some(data) = event.data else {
        // 无 data 字段 → 无事件载荷（对齐 EventSource：空 data 不分发）。
        return Ok(None);
    };
    // 精确相等才丢弃；` [DONE]` 这类变体照常走 JSON 解析（TS 分层一致）。
    if data == "[DONE]" {
        return Ok(None);
    }
    match serde_json::from_str::<Value>(&data) {
        Ok(value) => Ok(Some(JsonSseFrame {
            event: event.event,
            value,
        })),
        Err(err) => Err(SseError::Json {
            message: err.to_string(),
        }),
    }
}
