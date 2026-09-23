//! Provider 配置：host 模型调用的唯一事实源。
//!
//! 与 TS 侧的关系（specs/rust-native-crates.md S3）：本 increment 不移植完整
//! provider registry / OAuth / coding-plan 体系，只接受一个显式的 JSON 配置文件
//! （二进制启动参数 `--provider-config`）。没有配置文件 = 模型层不可用，turn
//! fail-closed 返回 `ModelErrorCode::ProviderNotConfigured`——与 `zcode-cua` 的
//! fail-closed 纪律一致，绝不伪造 provider。

use serde::{Deserialize, Serialize};
use zcode_provider::request::ProviderKind;

/// provider 配置文件（`--provider-config`）。wire 键名与 TS 侧配置一致，
/// camelCase（`baseUrl`/`apiKey`/`maxTokens`）；Rust 字段名保持 snake_case。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    /// `anthropic` | `openai-compatible`（对齐 TS provider api.type 的两个主形状）。
    pub kind: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

fn default_max_tokens() -> u32 {
    4096
}

#[derive(Debug)]
pub enum ProviderConfigError {
    Io(std::io::Error),
    Parse(String),
    UnsupportedKind(String),
    EmptyField(&'static str),
}

impl std::fmt::Display for ProviderConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "provider config io error: {error}"),
            Self::Parse(message) => write!(f, "provider config parse error: {message}"),
            Self::UnsupportedKind(kind) => write!(f, "unsupported provider kind: {kind}"),
            Self::EmptyField(field) => {
                write!(f, "provider config field must be non-empty: {field}")
            }
        }
    }
}

impl ProviderConfig {
    pub fn load(path: &std::path::Path) -> Result<Self, ProviderConfigError> {
        let raw = std::fs::read_to_string(path).map_err(ProviderConfigError::Io)?;
        let config: Self = serde_json::from_str(&raw)
            .map_err(|error| ProviderConfigError::Parse(error.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ProviderConfigError> {
        if self.base_url.trim().is_empty() {
            return Err(ProviderConfigError::EmptyField("baseUrl"));
        }
        if self.api_key.trim().is_empty() {
            return Err(ProviderConfigError::EmptyField("apiKey"));
        }
        if self.model.trim().is_empty() {
            return Err(ProviderConfigError::EmptyField("model"));
        }
        self.provider_kind()?;
        Ok(())
    }

    pub fn provider_kind(&self) -> Result<ProviderKind, ProviderConfigError> {
        match self.kind.as_str() {
            "anthropic" => Ok(ProviderKind::Anthropic),
            "openai-compatible" => Ok(ProviderKind::OpenaiCompatible),
            other => Err(ProviderConfigError::UnsupportedKind(other.to_string())),
        }
    }

    /// 出站 URL：anthropic 用 `<base>/messages`，openai-compatible 用
    /// `<base>/chat/completions`（对齐 TS 两个 api.type 的路径约定；base 已含
    /// `/v1` 等版本段时由配置方负责）。
    pub fn endpoint_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        match self.kind.as_str() {
            "openai-compatible" => format!("{base}/chat/completions"),
            _ => format!("{base}/messages"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ProviderConfig {
        ProviderConfig {
            kind: "anthropic".to_string(),
            base_url: "https://api.example.com/anthropic".to_string(),
            api_key: "key".to_string(),
            model: "m-1".to_string(),
            max_tokens: 1024,
        }
    }

    #[test]
    fn rejects_empty_required_fields() {
        let mut broken = config();
        broken.api_key = "  ".to_string();
        assert!(matches!(
            broken.validate(),
            Err(ProviderConfigError::EmptyField("apiKey"))
        ));
    }

    #[test]
    fn rejects_unknown_kind() {
        let mut broken = config();
        broken.kind = "gemini".to_string();
        assert!(matches!(
            broken.validate(),
            Err(ProviderConfigError::UnsupportedKind(_))
        ));
    }

    #[test]
    fn endpoint_urls_follow_provider_conventions() {
        let anthropic = config();
        assert_eq!(
            anthropic.endpoint_url(),
            "https://api.example.com/anthropic/messages"
        );
        let mut openai = config();
        openai.kind = "openai-compatible".to_string();
        openai.base_url = "https://api.example.com/v1/".to_string();
        assert_eq!(
            openai.endpoint_url(),
            "https://api.example.com/v1/chat/completions"
        );
    }
}
