//! Provider 配置：host 模型调用的唯一事实源。
//!
//! 与 TS 侧的关系（specs/rust-native-crates.md S3）：本 increment 不移植完整
//! provider registry / OAuth / coding-plan 体系，只接受一个显式的 JSON 配置文件
//! （二进制启动参数 `--provider-config`）。没有配置文件 = 模型层不可用，turn
//! fail-closed 返回 `ModelErrorCode::ProviderNotConfigured`——与 `zcode-cua` 的
//! fail-closed 纪律一致，绝不伪造 provider。

use nc_code_provider::request::ProviderKind;
use serde::{Deserialize, Serialize};

/// provider 配置文件（`--provider-config`）。wire 键名与 TS 侧配置一致，
/// camelCase（`baseUrl`/`apiKey`/`maxTokens`）；Rust 字段名保持 snake_case。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    /// 个人 provider store 中的 provider id（provider-settings / model-selection 的关联键）。
    /// 静态 `--provider-config` 没有该键时为 `None`；不参与请求体构造。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    /// `anthropic` | `openai-compatible`（对齐 TS provider api.type 的两个主形状）。
    pub kind: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// 鉴权样式：`bearer`（Authorization: Bearer）| `x-api-key`（anthropic 原生）。
    /// 缺省按 kind 的原生样式（anthropic → x-api-key；openai → bearer）。部分网关
    /// 对两种形状都只认 Bearer——显式声明即可，不必改代码。
    #[serde(default)]
    pub auth_style: Option<String>,
    /// 流式开关；缺省 true。部分 Provider/网关只提供非流式完整响应（或 stream 被
    /// 网关翻译成别的形状），置 false 走完整响应路径（executor 合成等价 SSE）。
    #[serde(default = "default_stream")]
    pub stream: bool,
    /// 静态已知模型目录（ListModels 的事实来源）。缺省只含主模型——**绝不用
    /// 动态 registry 之外的猜测填充**（TS 的完整 model 目录含账户/网关动态下发，
    /// 在 host 侧未接线，见 crate README 差异表）。
    #[serde(default)]
    pub extra_models: Vec<String>,
    /// 该 provider 支持的思考档位（ListModels 输出；缺省空 = 不支持）。
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
    /// 自定义出站头（TS `api.headers` 对象形态：`{"X-Foo":"bar"}`）。
    /// serde 默认把 JSON 对象读成 `Vec<(String,String)>` 需要自定义反序列化；
    /// 这里用 `headers` 模块的 `deserialize` 函数。缺省空 = 无自定义头。
    /// 出站时在鉴权头之后追加（后者胜出，见 `mergeModelRequestHeaders`
    /// 语义，见 turn.rs）。
    #[serde(default, deserialize_with = "headers_serde::deserialize")]
    pub headers: Vec<(String, String)>,
}

impl ProviderConfig {
    /// 模型目录：主模型在前，其余去重保序。
    pub fn model_catalog(&self) -> Vec<String> {
        let mut models = vec![self.model.clone()];
        for model in &self.extra_models {
            if !models.contains(model) {
                models.push(model.clone());
            }
        }
        models
    }
}

fn default_max_tokens() -> u32 {
    4096
}

fn default_stream() -> bool {
    true
}

/// serde helpers for the `headers` field: TS `api.headers` is a JSON object
/// (`{"X-Foo":"bar"}`), not an array. A plain `Vec<(String,String)>`
/// deserializer expects `[["X-Foo","bar"]]` and would reject the wire shape.
mod headers_serde {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<(String, String)>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let map = std::collections::BTreeMap::<String, String>::deserialize(deserializer)?;
        Ok(map.into_iter().collect())
    }
}

pub const AUTH_STYLE_BEARER: &str = "bearer";
pub const AUTH_STYLE_X_API_KEY: &str = "x-api-key";

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

    /// 生效鉴权样式：显式 `authStyle` 优先，否则按 kind 原生默认。
    pub fn auth_style(&self) -> Result<&'static str, ProviderConfigError> {
        match self.auth_style.as_deref() {
            Some(AUTH_STYLE_BEARER) => Ok(AUTH_STYLE_BEARER),
            Some(AUTH_STYLE_X_API_KEY) => Ok(AUTH_STYLE_X_API_KEY),
            Some(other) => Err(ProviderConfigError::UnsupportedKind(format!(
                "authStyle: {other}"
            ))),
            None => match self.provider_kind()? {
                ProviderKind::Anthropic => Ok(AUTH_STYLE_X_API_KEY),
                ProviderKind::OpenaiCompatible => Ok(AUTH_STYLE_BEARER),
            },
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
            provider_id: None,
            kind: "anthropic".to_string(),
            base_url: "https://api.example.com/anthropic".to_string(),
            api_key: "key".to_string(),
            model: "m-1".to_string(),
            max_tokens: 1024,
            auth_style: None,
            stream: true,
            extra_models: Vec::new(),
            reasoning_levels: Vec::new(),
            headers: Vec::new(),
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
