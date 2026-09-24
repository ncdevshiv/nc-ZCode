//! ListModels：列出当前 provider 配置下可用的模型（替代 TS
//! `core/src/tool/handlers/list-models.ts`）。
//!
//! 契约对齐 `contracts/tools/list-models.ts`：输出 `{models: [...], reasoningLevels:
//! [...]}`。每个 model entry 携带 id/provider/displayName/是否默认等展示事实。
//!
//! 事实来源：host 启动时读入的 provider 配置（`--provider-config`）。这是本
//! increment 的**最小诚实面**：TS 侧的完整 registry 还含 OAuth/coding-plan 账户、
//! 动态下发的模型列表与 option 规格——host 侧未接线（crate README 差异表记录），
//! 本工具只报配置里**静态已知**的模型，绝不编造动态列表。

use serde_json::{json, Value};

use crate::provider_config::ProviderConfig;
use crate::tools::{Tool, ToolContext, ToolContract, ToolError, ToolErrorCode};

pub struct ListModelsTool;

impl Tool for ListModelsTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "ListModels".to_string(),
            description: "List the models available under the active provider configuration, with their ids, providers and display names.".to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
            read_only: true,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: crate::tools::SCOPE_NONE,
            timeout_ms: 5_000,
            max_output_bytes: 32 * 1024,
        }
    }

    fn execute(&self, _input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let Some(config) = ctx.provider_config.as_deref() else {
            return Err(ToolError::new(
                ToolErrorCode::ExecutionFailed,
                "no provider configured; start the host with --provider-config",
            ));
        };
        let models: Vec<Value> = config
            .model_catalog()
            .iter()
            .map(|model| describe_model(config, model))
            .collect();
        Ok(json!({
            "models": models,
            "reasoningLevels": config.reasoning_levels,
        })
        .to_string())
    }
}

fn describe_model(config: &ProviderConfig, model: &str) -> Value {
    json!({
        "id": model,
        "provider": config.kind,
        "displayName": model,
        "isDefault": model == config.model,
        "source": "static-config",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ProviderConfig {
        ProviderConfig {
            kind: "openai-compatible".to_string(),
            base_url: "https://example.com/v1".to_string(),
            api_key: "k".to_string(),
            model: "main-model".to_string(),
            max_tokens: 4096,
            auth_style: Some("bearer".to_string()),
            stream: true,
            extra_models: vec!["other-model".to_string(), "main-model".to_string()],
            reasoning_levels: vec!["low".to_string(), "high".to_string()],
        }
    }

    #[test]
    fn catalog_lists_main_model_first_and_dedupes() {
        assert_eq!(
            config().model_catalog(),
            vec!["main-model".to_string(), "other-model".to_string()]
        );
    }

    #[test]
    fn empty_extra_models_still_reports_the_configured_model() {
        let mut config = config();
        config.extra_models.clear();
        assert_eq!(config.model_catalog(), vec!["main-model".to_string()]);
    }

    #[test]
    fn default_model_flag_marks_only_the_configured_one() {
        let config = config();
        let rows: Vec<Value> = config
            .model_catalog()
            .iter()
            .map(|model| describe_model(&config, model))
            .collect();
        assert_eq!(rows[0]["isDefault"], json!(true));
        assert_eq!(rows[1]["isDefault"], json!(false));
        assert_eq!(rows[0]["source"], json!("static-config"));
    }
}
