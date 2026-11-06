//! `video/*` 输入部件与 30MB 预算（对齐两个 ai-sdk patch + nc-code-media-policy）。

use serde_json::{json, Map, Value};
use std::fmt;

/// 输入视频预算上限（对齐 `packages/shared/src/nc-code-media-policy.ts`）。
pub const VIDEO_INPUT_MAX_BYTES: usize = 30 * 1024 * 1024;
/// 预算超限的稳定错误码（wire 契约，改动需两侧同步）。
pub const MEDIA_BUDGET_CURRENT_VIDEO_TOO_LARGE: &str = "MEDIA_BUDGET_CURRENT_VIDEO_TOO_LARGE";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoBudgetError {
    pub size_bytes: usize,
    pub max_bytes: usize,
}

impl fmt::Display for VideoBudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{MEDIA_BUDGET_CURRENT_VIDEO_TOO_LARGE}: video input is {} bytes (max {})",
            self.size_bytes, self.max_bytes
        )
    }
}

impl std::error::Error for VideoBudgetError {}

/// 解析后的 `data:` URL（mediaType 统一小写）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDataUrl {
    pub media_type: String,
    pub data: String,
}

/// 解析 `data:<mediaType>;base64,<data>`（对齐 media-transform-policy.ts 的判定顺序）。
pub fn parse_data_url(url: &str) -> Option<ParsedDataUrl> {
    let rest = url.strip_prefix("data:")?;
    let (header, data) = rest.split_once(',')?;
    let mut parts = header.split(';');
    let media_type = parts.next()?.trim();
    let remaining: Vec<&str> = parts.collect();
    // TS：末段（可能有中间段）trim 后必须是 base64；无媒体类型同样失败。
    let last = remaining.last()?.trim();
    // TS：末段 toLowerCase 后必须等于 "base64"（大小写不敏感）。
    if !last.to_lowercase().eq("base64") {
        return None;
    }
    if media_type.is_empty() {
        return None;
    }
    Some(ParsedDataUrl {
        media_type: media_type.to_lowercase(),
        data: data.to_string(),
    })
}

/// base64 文本解码后的字节数（含 padding 修正）。
pub fn decoded_base64_len(data: &str) -> usize {
    let padding = data.bytes().rev().take_while(|b| *b == b'=').count();
    data.len() / 4 * 3usize.saturating_sub(padding.min(3))
}

/// 视频预算校验：按解码后字节数判定（TS 检查点在附件解析层，见报告 OPEN 项）。
pub fn ensure_video_within_budget(size_bytes: usize) -> Result<(), VideoBudgetError> {
    if size_bytes > VIDEO_INPUT_MAX_BYTES {
        return Err(VideoBudgetError {
            size_bytes,
            max_bytes: VIDEO_INPUT_MAX_BYTES,
        });
    }
    Ok(())
}

/// anthropic `/messages` 的 video 部件：`{"type":"video","source":{...},"cache_control"?}`；
/// 无 cache providerOptions 时不写 `cache_control` 键（JSON 序列化后不存在）。
pub fn anthropic_video_part(media_type: &str, data: &str, cache_control: Option<Value>) -> Value {
    let mut source = Map::new();
    source.insert("type".into(), json!("base64"));
    source.insert("media_type".into(), json!(media_type));
    source.insert("data".into(), json!(data));

    let mut part = Map::new();
    part.insert("type".into(), json!("video"));
    part.insert("source".into(), Value::Object(source));
    if let Some(cache_control) = cache_control {
        part.insert("cache_control".into(), cache_control);
    }
    Value::Object(part)
}

/// openai-compatible 的 video 部件：`{"type":"video_url","video_url":{"url":...}, ...partMetadata}`；
/// partMetadata = `providerOptions?.openaiCompatible ?? {}`，对象键展开在部件顶层。
pub fn openai_compatible_video_part(
    media_type: &str,
    data: &str,
    part_metadata: Option<Value>,
) -> Value {
    let mut video_url = Map::new();
    video_url.insert(
        "url".into(),
        json!(format!("data:{media_type};base64,{data}")),
    );

    let mut part = Map::new();
    part.insert("type".into(), json!("video_url"));
    part.insert("video_url".into(), Value::Object(video_url));
    if let Some(Value::Object(metadata)) = part_metadata {
        for (key, value) in metadata {
            part.insert(key, value);
        }
    }
    Value::Object(part)
}
