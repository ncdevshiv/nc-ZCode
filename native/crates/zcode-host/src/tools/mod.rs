//! 原生工具层（specs/rust-native-crates.md S3 cutover 顺序第 1 阶段）。
//!
//! 契约对齐 TS `core/src/tool/types.ts` 的 `ToolMetadata`：readOnly / destructive /
//! concurrentSafe / sideEffectScope / timeoutMs / maxOutputBytes 是权限与调度读取
//! 的声明，不在调用点按工具名猜测。
//!
//! 与 TS 的差异（README 差异表同步）：本 increment 的工具集是 Bash/Read/Write/
//! Edit/Glob/Grep 六个核心工具；input 校验为逐字段靶向校验（完整 JSON Schema 校验
//! 器未移植）；调度为模型序串行（并行分组调度器未移植——本批工具以写为主，串行是
//! 更稳的语义而不是残留 stub）；权限模型 fail-closed：只读调用放行，其余调用默认
//! 拒绝，仅 `--full-access` 时自动允许（headless 无交互客户端可问，对齐 TS
//! PermissionBroker 的 deny 缺省）。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use zcode_provider::request::ToolDefinition;

pub mod bash;
pub mod fs_tools;
pub mod list_models;
pub mod read_image;
pub mod search_tools;
pub mod skill;
pub mod todo;
pub mod webfetch;

/// sideEffectScope 闭集（TS `ModelToolSideEffectScope`）。
pub const SCOPE_NONE: &str = "none";
pub const SCOPE_WORKSPACE: &str = "workspace";
pub const SCOPE_SYSTEM: &str = "system";

#[derive(Debug, Clone)]
pub struct ToolContract {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub read_only: bool,
    pub destructive: bool,
    pub concurrent_safe: bool,
    pub side_effect_scope: &'static str,
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
}

impl ToolContract {
    pub fn needs_approval(&self) -> bool {
        !self.read_only
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolErrorCode {
    InvalidInput,
    PermissionDenied,
    PathEscapesWorkspace,
    ExecutionFailed,
    Timeout,
    Cancelled,
}

impl ToolErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InvalidInput => "tool.invalidInput",
            Self::PermissionDenied => "tool.permissionDenied",
            Self::PathEscapesWorkspace => "tool.pathEscapesWorkspace",
            Self::ExecutionFailed => "tool.executionFailed",
            Self::Timeout => "tool.timeout",
            Self::Cancelled => "tool.cancelled",
        }
    }
}

#[derive(Debug)]
pub struct ToolError {
    pub code: ToolErrorCode,
    pub message: String,
}

impl ToolError {
    pub fn new(code: ToolErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// 回灌模型的稳定文本（code + message；模型需要能据此修正，而不是猜）。
    pub fn to_model_text(&self) -> String {
        format!("[{}] {}", self.code.as_str(), self.message)
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code.as_str(), self.message)
    }
}

pub struct ToolContext {
    pub workspace_root: PathBuf,
    /// `--full-access`： destructive 调用自动允许；缺省 fail-closed。
    pub full_access: bool,
    pub trace_id: String,
    pub turn_id: String,
    /// read-before-edit 状态：Read 成功即登记，Edit 强制先读（对齐 TS readFileState）。
    pub read_state: Arc<Mutex<HashSet<PathBuf>>>,
    pub cancel: Arc<AtomicBool>,
    /// 会话级状态（当前只有 todo 列表）；Session 持有同一实例，工具写入即会话可见。
    pub todos: todo::TodoStore,
    /// host 级 provider 配置（ListModels 的事实来源；每轮由 TurnDeps 注入）。
    pub provider_config: Option<std::sync::Arc<crate::provider_config::ProviderConfig>>,
}

impl ToolContext {
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::SeqCst)
    }
}

pub trait Tool: Send + Sync {
    fn contract(&self) -> ToolContract;

    /// 单次调用的只读判定（bash 按 argv 静态分类；默认用 contract.read_only）。
    fn is_read_only_invocation(&self, _input: &Value) -> bool {
        false
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError>;
}

pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn with_defaults() -> Self {
        let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
        tools.insert("Bash".to_string(), Arc::new(bash::BashTool));
        tools.insert("Read".to_string(), Arc::new(fs_tools::ReadTool));
        tools.insert("Write".to_string(), Arc::new(fs_tools::WriteTool));
        tools.insert("Edit".to_string(), Arc::new(fs_tools::EditTool));
        tools.insert("Glob".to_string(), Arc::new(search_tools::GlobTool));
        tools.insert("Grep".to_string(), Arc::new(search_tools::GrepTool));
        tools.insert("TodoWrite".to_string(), Arc::new(todo::TodoWriteTool));
        tools.insert("TodoRead".to_string(), Arc::new(todo::TodoReadTool));
        tools.insert("WebFetch".to_string(), Arc::new(webfetch::WebFetchTool));
        tools.insert(
            "ListModels".to_string(),
            Arc::new(list_models::ListModelsTool),
        );
        tools.insert("ReadImage".to_string(), Arc::new(read_image::ReadImageTool));
        tools.insert("Skill".to_string(), Arc::new(skill::SkillTool));
        Self { tools }
    }

    /// 把 nc-tools 的原生工具面（进程内 Kernel，89 个 typed 工具）并入注册表。
    /// 点号工具名（`fs.read`/`git.status`…）与 TS 侧工具名并存，互不覆盖。
    pub fn with_nc_tools(mut self, nc: Vec<Arc<crate::nc_tools::NcTool>>) -> Self {
        for tool in nc {
            self.tools
                .insert(tool.contract().name, tool as Arc<dyn Tool>);
        }
        self
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tools.keys().cloned().collect();
        names.sort();
        names
    }

    /// 出站工具定义（provider `ToolDefinition`；strict 关闭——schema 折叠是
    /// zcode-provider 的门控，host 不自行开启）。
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let mut defs: Vec<ToolDefinition> = self
            .tools
            .values()
            .map(|tool| {
                let contract = tool.contract();
                ToolDefinition {
                    name: contract.name.clone(),
                    description: Some(contract.description.clone()),
                    input_schema: contract.input_schema.clone(),
                    strict: false,
                }
            })
            .collect();
        defs.sort_by(|left, right| left.name.cmp(&right.name));
        defs
    }

    /// 权限闸门（host 执行前调用）：只读放行；其余按 full_access 裁决。
    /// 返回 Ok(()) = 允许执行；Err = fail-closed 拒绝（直接回灌模型）。
    pub fn authorize(
        &self,
        tool: &dyn Tool,
        input: &Value,
        ctx: &ToolContext,
    ) -> Result<(), ToolError> {
        let contract = tool.contract();
        let read_only = contract.read_only || tool.is_read_only_invocation(input);
        if read_only {
            return Ok(());
        }
        if ctx.full_access {
            return Ok(());
        }
        Err(ToolError::new(
            ToolErrorCode::PermissionDenied,
            format!(
                "{} is not read-only and this host runs without --full-access; \
                 the action was not performed",
                contract.name
            ),
        ))
    }
}

/// 通用 input 校验助手：取必填字符串/整数字段。
pub fn require_string<'a>(input: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    input.get(key).and_then(Value::as_str).ok_or_else(|| {
        ToolError::new(
            ToolErrorCode::InvalidInput,
            format!("{key}: required string"),
        )
    })
}

pub fn optional_string<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

pub fn optional_u64(input: &Value, key: &str) -> Option<u64> {
    input.get(key).and_then(Value::as_u64)
}

pub fn string_schema(description: &str) -> Value {
    json!({"type": "string", "description": description})
}

pub fn truncate_to_limit(text: String, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let truncated = format!(
        "{}\n[output truncated at {} bytes; maxOutputBytes]",
        &text[..end],
        max_bytes
    );
    (truncated, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubTool;

    impl Tool for StubTool {
        fn contract(&self) -> ToolContract {
            ToolContract {
                name: "Stub".to_string(),
                description: "stub".to_string(),
                input_schema: json!({"type": "object"}),
                read_only: false,
                destructive: false,
                concurrent_safe: true,
                side_effect_scope: SCOPE_SYSTEM,
                timeout_ms: 1000,
                max_output_bytes: 1024,
            }
        }

        fn execute(&self, _input: &Value, _ctx: &ToolContext) -> Result<String, ToolError> {
            Ok("ok".to_string())
        }
    }

    fn ctx(full_access: bool) -> ToolContext {
        ToolContext {
            workspace_root: PathBuf::from("."),
            full_access,
            trace_id: "t".to_string(),
            turn_id: "turn".to_string(),
            read_state: Arc::new(Mutex::new(HashSet::new())),
            cancel: Arc::new(AtomicBool::new(false)),
            todos: crate::tools::todo::new_todo_store(),
            provider_config: None,
        }
    }

    #[test]
    fn authorize_fails_closed_without_full_access() {
        let registry = ToolRegistry::with_defaults();
        let tool = StubTool;
        assert!(registry.authorize(&tool, &json!({}), &ctx(false)).is_err());
        assert!(registry.authorize(&tool, &json!({}), &ctx(true)).is_ok());
    }

    #[test]
    fn truncate_reports_and_stays_on_char_boundary() {
        let (text, truncated) = truncate_to_limit("héllo wörld".to_string(), 8);
        assert!(truncated);
        assert!(text.starts_with("héllo"));
    }
}
