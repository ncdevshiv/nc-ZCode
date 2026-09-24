//! 压缩 live e2e：真实 provider 跑一轮**长历史**（> 阈值），验证压缩真的发生
//! （历史被摘要取代）且压缩后 agent 仍能完成动作。
//!
//! 用真实 provider 代价高，所以这里直接测 `compact_history` 的 live 路径：
//! 造 30 条历史 → 调摘要 → 断言返回历史显著变短、含 summary、尾部保留。

use std::sync::Arc;

use serde_json::json;
use zcode_host::provider_config::ProviderConfig;
use zcode_host::turn_context::{self, ContextFacts};
use zcode_provider::request::{Message, MessageRole};

/// 只在显式提供凭据时跑（默认跳过，避免测试套件消耗 token/网络）。
fn live_config() -> Option<ProviderConfig> {
    let path = std::env::var("ZCODE_HOST_LIVE_PROVIDER_CONFIG").ok()?;
    ProviderConfig::load(std::path::Path::new(&path)).ok()
}

#[test]
fn compaction_summarizes_long_history_with_a_real_provider() {
    let Some(config) = live_config() else {
        eprintln!("[skip] set ZCODE_HOST_LIVE_PROVIDER_CONFIG to run the live compaction test");
        return;
    };
    let context: ContextFacts = turn_context::assemble_context(std::path::Path::new("."));
    let history: Vec<Message> = (0..30)
        .map(|index| {
            Message::text(
                MessageRole::User,
                format!("message {index}: discuss the Rust port step {index}"),
            )
        })
        .collect();

    let cancelled: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(|| false);
    let compacted = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(zcode_host::compact::compact_history(
            &config, &context, &history, cancelled,
        ))
        .expect("compaction must succeed against a live provider");

    assert!(
        compacted.len() < history.len(),
        "compaction must shrink history: {} -> {}",
        history.len(),
        compacted.len()
    );
    let first = compacted[0]
        .parts
        .iter()
        .filter_map(|part| match part {
            zcode_provider::request::ContentPart::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        first.contains("conversation summary"),
        "first message must be the compaction summary: {first}"
    );
    // 尾部保留：最后一条原始消息仍在。
    let last_text: String = compacted
        .last()
        .expect("tail")
        .parts
        .iter()
        .filter_map(|part| match part {
            zcode_provider::request::ContentPart::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        last_text.contains("message 29"),
        "tail must survive: {last_text}"
    );
    let _ = json!({});
}
