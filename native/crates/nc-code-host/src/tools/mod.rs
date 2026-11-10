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
use nc_code_provider::request::ToolDefinition;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
pub mod actor_control_tools;
pub mod apply_patch;
pub mod ask_user;
pub mod automation_tools;
pub mod background_tools;
pub mod bash;
pub mod browser;
pub mod computer;
pub mod fs_tools;
pub mod list_models;
pub mod node_repl;
pub mod offpeak_tools;
pub mod pdf_image;
pub mod pdf_render;
pub mod plan;
pub mod read_image;
pub mod read_pdf;
pub mod read_session_context;
pub mod respond_to_coordinator;
pub mod search_tools;
pub mod send_message;
pub mod skill;
pub mod todo;
pub mod web_search;
pub mod webfetch;
pub mod workflow;
/// sideEffectScope 闭集（TS `ModelToolSideEffectScope`）。
pub const SCOPE_NONE: &str = "none";
pub const SCOPE_WORKSPACE: &str = "workspace";
pub const SCOPE_SYSTEM: &str = "system";
pub const SCOPE_SESSION: &str = "session";
pub const SCOPE_NETWORK: &str = "network";
pub const SCOPE_USER_INTERACTION: &str = "userInteraction";
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

    /// Per-tool cap used when the host folds a tool result back into the model
    /// context (TS `maxModelBytes`). Defaults to `max_output_bytes`.
    pub max_model_bytes: usize,
    /// Which end of an oversized output is kept in the model context:
    /// `"head"` (default) or `"tail"`.
    pub preview_direction: &'static str,
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
/// 单次调用的权限裁决。`Ask` 由 turn 打开 permission 交互并等待用户应答。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authorization {
    Allow,
    Ask,
    Deny(String),
}
/// Port trait for scheduled-task (automation) operations.
///
/// Host defines this trait; the composition root (services layer) injects an
/// implementation. Without an implementation the cron tools fail closed.
pub trait AutomationPort: Send + Sync {
    fn create(
        &self,
        input: &serde_json::Value,
        session_id: &str,
        model: Option<&str>,
    ) -> Result<serde_json::Value, String>;
    fn list(&self) -> Result<Vec<serde_json::Value>, String>;
    fn update(&self, input: &serde_json::Value) -> Result<serde_json::Value, String>;
    fn delete(&self, id: &str) -> Result<bool, String>;
}
pub struct ToolContext {
    pub workspace_root: PathBuf,

    /// 生效的 full-access（`--full-access` 或用户在 permission 里授予的会话级授权）。
    pub full_access: bool,

    /// 当前会话是否有可应答的订阅者；headless 时非只读调用 fail-closed。
    pub interactive: bool,
    pub trace_id: String,
    pub turn_id: String,

    /// Authoritative session id for tools whose state is session-scoped.
    pub session_id: String,
    /// Provider/model identity (`provider/model`) for tools that report or target a model.
    pub model_ref: Option<String>,
    /// The tool_use id of this invocation (interaction correlation / plan commands).
    pub tool_call_id: Option<String>,
    /// The session's plan-mode bit at the start of this invocation.
    pub plan_mode: bool,

    /// read-before-edit 状态：Read 成功即登记，Edit 强制先读（对齐 TS readFileState）。
    pub read_state: Arc<Mutex<HashSet<PathBuf>>>,
    pub cancel: Arc<AtomicBool>,

    /// 会话级状态（当前只有 todo 列表）；Session 持有同一实例，工具写入即会话可见。
    pub todos: todo::TodoStore,

    /// host 级 provider 配置（ListModels 的事实来源；每轮由 TurnDeps 注入）。
    pub provider_config: Option<std::sync::Arc<crate::provider_config::ProviderConfig>>,

    /// Injected ports. `None` means the capability is not configured in this host and
    /// the tool must fail closed with its named error (never fake a result).
    pub ask_user: Option<Arc<ask_user::AskUserPrompter>>,
    pub plan: Option<Arc<plan::PlanController>>,
    pub subagent: Option<Arc<crate::subagent::SubagentRunner>>,
    pub background: Option<Arc<crate::background::BackgroundRegistry>>,
    pub sessions: Option<Arc<dyn read_session_context::SessionContextPort>>,
    pub workflow: Option<Arc<dyn workflow::WorkflowToolPort>>,
    pub automation: Option<Arc<dyn AutomationPort>>,
    /// Native browser-use port; `None` makes the `Browser` tool fail closed.
    pub browser: Option<Arc<dyn browser::BrowserControlPort>>,
    /// Native computer-use port; `None` makes the `Computer` tool fail closed.
    pub computer: Option<Arc<dyn computer::ComputerControlPort>>,
    /// Off-peak task port; `None` makes off-peak tools fail closed.
    pub offpeak: Option<Arc<dyn offpeak_tools::OffPeakPort>>,
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

    /// This tool owns a user interaction (plan approval / question) and must not be
    /// routed through the generic permission ask.
    fn requires_user_interaction(&self) -> bool {
        false
    }
    /// Always shows the permission prompt, even when the input looks safe.
    fn always_requires_approval(&self) -> bool {
        false
    }
    /// Safe-by-design tools that never need a prompt (read-only projections).
    fn always_allowed_by_name(&self) -> bool {
        false
    }
    /// Static display attached to the tool definition (optional).
    fn result_display(&self) -> Option<Value> {
        None
    }
    /// Result-derived display variant (task_output / workflow cards / images).
    fn result_display_for(&self, _input: &Value, _output_text: &str) -> Option<Value> {
        None
    }
    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError>;
}
/// Tools that must wait on the runtime (user interaction, subagent turns, workflow
/// commands). They run on the async executor, never on the blocking pool.
#[async_trait::async_trait]
pub trait AsyncTool: Send + Sync {
    fn contract(&self) -> ToolContract;
    fn is_read_only_invocation(&self, _input: &Value) -> bool {
        false
    }
    fn requires_user_interaction(&self) -> bool {
        false
    }
    fn always_requires_approval(&self) -> bool {
        false
    }
    fn always_allowed_by_name(&self) -> bool {
        false
    }
    fn result_display(&self) -> Option<Value> {
        None
    }
    fn result_display_for(&self, _input: &Value, _output_text: &str) -> Option<Value> {
        None
    }
    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError>;
}
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    async_tools: HashMap<String, Arc<dyn AsyncTool>>,
    /// 解析别名（`Task → Agent`）：只在调用解析时生效，不出现在 provider 的工具面。
    aliases: HashMap<String, String>,
}
impl ToolRegistry {
    pub fn with_defaults() -> Self {
        let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
        tools.insert("Bash".to_string(), Arc::new(bash::BashTool));
        tools.insert("Read".to_string(), Arc::new(fs_tools::ReadTool));
        tools.insert("Write".to_string(), Arc::new(fs_tools::WriteTool));
        tools.insert("Edit".to_string(), Arc::new(fs_tools::EditTool));
        tools.insert(
            "ApplyPatch".to_string(),
            Arc::new(apply_patch::ApplyPatchTool),
        );
        tools.insert("Glob".to_string(), Arc::new(search_tools::GlobTool));
        tools.insert("Grep".to_string(), Arc::new(search_tools::GrepTool));
        tools.insert("TodoWrite".to_string(), Arc::new(todo::TodoWriteTool));
        tools.insert("TodoRead".to_string(), Arc::new(todo::TodoReadTool));
        tools.insert("WebFetch".to_string(), Arc::new(webfetch::WebFetchTool));
        // WebSearch uses an injected nc-tools net.search backend when provided by
        // composition; the default no-backend instance fails closed with a named error
        // (no fake results), matching TS behavior when provider-native search is absent.
        tools.insert(
            "WebSearch".to_string(),
            Arc::new(web_search::WebSearchTool::new(None)),
        );
        tools.insert(
            "ListModels".to_string(),
            Arc::new(list_models::ListModelsTool),
        );
        tools.insert("ReadImage".to_string(), Arc::new(read_image::ReadImageTool));
        tools.insert("Skill".to_string(), Arc::new(skill::SkillTool));
        tools.insert(
            "ReadSessionContext".to_string(),
            Arc::new(read_session_context::ReadSessionContextTool),
        );
        tools.insert(
            "TaskOutput".to_string(),
            Arc::new(background_tools::TaskOutputTool),
        );
        tools.insert(
            "TaskStop".to_string(),
            Arc::new(background_tools::TaskStopTool),
        );
        tools.insert(
            "EnterPlanMode".to_string(),
            Arc::new(plan::EnterPlanModeTool),
        );
        tools.insert("js".to_string(), Arc::new(node_repl::JsTool::new()));
        tools.insert(
            "CronCreate".to_string(),
            Arc::new(automation_tools::CronCreateTool),
        );
        tools.insert(
            "CronList".to_string(),
            Arc::new(automation_tools::CronListTool),
        );
        tools.insert(
            "CronUpdate".to_string(),
            Arc::new(automation_tools::CronUpdateTool),
        );
        tools.insert(
            "CronDelete".to_string(),
            Arc::new(automation_tools::CronDeleteTool),
        );
        tools.insert(
            "ListWorkflowRuns".to_string(),
            Arc::new(workflow::runs::ListWorkflowRunsTool),
        );
        tools.insert(
            "GetWorkflowRun".to_string(),
            Arc::new(workflow::runs::GetWorkflowRunTool),
        );
        tools.insert(
            "ResolveWorkflowQuestion".to_string(),
            Arc::new(workflow::runs::ResolveWorkflowQuestionTool),
        );
        tools.insert(
            "ListSavedWorkflows".to_string(),
            Arc::new(workflow::runs::ListSavedWorkflowsTool),
        );
        tools.insert(
            "OffPeakCreate".to_string(),
            Arc::new(offpeak_tools::OffPeakCreateTool),
        );
        tools.insert(
            "OffPeakList".to_string(),
            Arc::new(offpeak_tools::OffPeakListTool),
        );
        tools.insert(
            "RespondToCoordinator".to_string(),
            Arc::new(respond_to_coordinator::RespondToCoordinatorTool),
        );
        tools.insert(
            "submit_result".to_string(),
            Arc::new(actor_control_tools::SubmitResultTool),
        );
        tools.insert(
            "escalate".to_string(),
            Arc::new(actor_control_tools::EscalateTool),
        );
        let mut registry = Self {
            tools,
            async_tools: HashMap::new(),
            aliases: HashMap::new(),
        };
        registry = registry
            .with_async_tool(Arc::new(ask_user::AskUserTool))
            .with_async_tool(Arc::new(plan::ExitPlanModeTool))
            .with_async_tool(Arc::new(crate::subagent::TaskTool::default()))
            .with_async_tool(Arc::new(send_message::SendMessageTool))
            .with_async_tool(Arc::new(workflow::authoring::CreateWorkflowTool))
            .with_async_tool(Arc::new(workflow::authoring::AmendWorkflowTool))
            .with_async_tool(Arc::new(workflow::authoring::SaveWorkflowTool))
            .with_async_tool(Arc::new(workflow::authoring::EvalWorkflowSnippetTool))
            .with_async_tool(Arc::new(workflow::runs::ResumeWorkflowRunTool))
            .with_async_tool(Arc::new(browser::BrowserTool))
            .with_async_tool(Arc::new(computer::ComputerTool))
            // TS keeps `Task` as a provider-invisible resolution alias for plugin prompts;
            // the advertised name is `Agent` only.
            .with_alias("Task", "Agent");
        registry
    }

    /// Register a resolution-only alias (`alias → target`). Aliases are not advertised
    /// in `tool_definitions`/`names`; they only make a legacy name resolve.
    pub fn with_alias(mut self, alias: &str, target: &str) -> Self {
        self.aliases.insert(alias.to_string(), target.to_string());
        self
    }

    /// 把 nc-tools 的原生工具面（进程内 Kernel，89 个 typed 工具）并入注册表。
    ///
    /// 模型面用安全名（点号→下划线，OpenAI 兼容网关只接受 `[A-Za-z0-9_-]`）；
    /// kernel 的点号名注册为解析别名，所以模型误用 `fs.read` 也能命中。
    pub fn with_nc_tools(mut self, nc: Vec<Arc<crate::nc_tools::NcTool>>) -> Self {
        for tool in nc {
            let safe = tool.contract().name;
            let kernel_name = tool.kernel_name().to_string();
            if safe != kernel_name {
                self.aliases.insert(kernel_name, safe.clone());
            }
            self.tools.insert(safe, tool as Arc<dyn Tool>);
        }
        self
    }

    /// 把已连接的 MCP 工具并入注册表（`mcp__server__tool`）。
    pub fn with_mcp_tools(mut self, mcp: Vec<Arc<crate::mcp_tools::McpToolProxy>>) -> Self {
        for tool in mcp {
            self.tools
                .insert(tool.contract().name, tool as Arc<dyn Tool>);
        }
        self
    }

    /// Register an async tool (interaction / subagent / workflow surface).
    pub fn with_async_tool(mut self, tool: Arc<dyn AsyncTool>) -> Self {
        self.async_tools.insert(tool.contract().name, tool);
        self
    }
    /// Register one more sync tool (used by the workflow driver to observe calls).
    pub fn with_tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.insert(tool.contract().name, tool);
        self
    }
    /// Copy of this registry without the named tools (subagent/workflow actors must
    /// not call tools that would recurse or escape their capability envelope).
    pub fn without(&self, names: &[&str]) -> Self {
        let excluded: std::collections::HashSet<&str> = names.iter().copied().collect();
        Self {
            tools: self
                .tools
                .iter()
                .filter(|(name, _)| !excluded.contains(name.as_str()))
                .map(|(name, tool)| (name.clone(), Arc::clone(tool)))
                .collect(),
            async_tools: self
                .async_tools
                .iter()
                .filter(|(name, _)| !excluded.contains(name.as_str()))
                .map(|(name, tool)| (name.clone(), Arc::clone(tool)))
                .collect(),
            aliases: self
                .aliases
                .iter()
                .filter(|(alias, target)| {
                    !excluded.contains(alias.as_str()) && !excluded.contains(target.as_str())
                })
                .map(|(alias, target)| (alias.clone(), target.clone()))
                .collect(),
        }
    }
    /// Copy of this registry restricted to an allowlist (`None` entries are not
    /// representable here; the caller only uses this with a concrete list).
    pub fn only(&self, names: &[String]) -> Self {
        let allowed: std::collections::HashSet<&str> = names.iter().map(String::as_str).collect();
        Self {
            tools: self
                .tools
                .iter()
                .filter(|(name, _)| allowed.contains(name.as_str()))
                .map(|(name, tool)| (name.clone(), Arc::clone(tool)))
                .collect(),
            async_tools: self
                .async_tools
                .iter()
                .filter(|(name, _)| allowed.contains(name.as_str()))
                .map(|(name, tool)| (name.clone(), Arc::clone(tool)))
                .collect(),
            aliases: self
                .aliases
                .iter()
                .filter(|(alias, target)| {
                    allowed.contains(alias.as_str()) && allowed.contains(target.as_str())
                })
                .map(|(alias, target)| (alias.clone(), target.clone()))
                .collect(),
        }
    }

    /// 注册测试/实验工具（仅供宿主内部装配与测试使用；名字冲突时覆盖）。
    pub fn with_test_tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.insert(tool.contract().name, tool);
        self
    }
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        let resolved = self.aliases.get(name).map(String::as_str).unwrap_or(name);
        self.tools.get(resolved).cloned()
    }
    pub fn get_async(&self, name: &str) -> Option<Arc<dyn AsyncTool>> {
        let resolved = self.aliases.get(name).map(String::as_str).unwrap_or(name);
        self.async_tools.get(resolved).cloned()
    }
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tools
            .keys()
            .chain(self.async_tools.keys())
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// 出站工具定义（provider `ToolDefinition`；strict 关闭——schema 折叠是
    ///
    /// zcode-provider 的门控，host 不自行开启）。
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let sync_defs = self.tools.values().map(|tool| tool.contract());
        let async_defs = self.async_tools.values().map(|tool| tool.contract());
        let mut defs: Vec<ToolDefinition> = sync_defs
            .chain(async_defs)
            .map(|contract| ToolDefinition {
                name: contract.name.clone(),
                description: Some(contract.description.clone()),
                input_schema: contract.input_schema.clone(),
                strict: false,
            })
            .collect();
        defs.sort_by(|left, right| left.name.cmp(&right.name));
        defs
    }

    /// 权限闸门（host 执行前调用）：只读或已授权 → Allow；有交互客户端 → Ask
    ///
    /// （由 turn 打开 permission 交互并等待）；headless → Deny（fail-closed）。
    pub fn authorize(&self, tool: &dyn Tool, input: &Value, ctx: &ToolContext) -> Authorization {
        let contract = tool.contract();
        let read_only = contract.read_only || tool.is_read_only_invocation(input);
        self.authorize_contract(
            &contract,
            read_only,
            ctx,
            tool.requires_user_interaction(),
            tool.always_allowed_by_name(),
            tool.always_requires_approval(),
        )
    }
    pub fn authorize_async(
        &self,
        tool: &dyn AsyncTool,
        input: &Value,
        ctx: &ToolContext,
    ) -> Authorization {
        let contract = tool.contract();
        let read_only = contract.read_only || tool.is_read_only_invocation(input);
        self.authorize_contract(
            &contract,
            read_only,
            ctx,
            tool.requires_user_interaction(),
            tool.always_allowed_by_name(),
            tool.always_requires_approval(),
        )
    }
    pub fn authorize_contract(
        &self,
        contract: &ToolContract,
        read_only: bool,
        ctx: &ToolContext,
        requires_user_interaction: bool,
        always_allowed_by_name: bool,
        always_requires_approval: bool,
    ) -> Authorization {
        // Plan mode is ordered before every ask-exemption: a tool that would write        // (or is destructive) is denied even under full access, unless it owns its        // own approval interaction (ExitPlanMode must be able to leave plan mode).
        if ctx.plan_mode
            && !requires_user_interaction
            && (contract.destructive || !(read_only || contract.read_only))
        {
            return Authorization::Deny(format!(                "Plan mode only allows read-only, non-destructive tools; {} was not run until the plan is approved",                contract.name            ));
        }
        if always_allowed_by_name || requires_user_interaction {
            return Authorization::Allow;
        }
        if always_requires_approval {
            return if ctx.interactive {
                Authorization::Ask
            } else {
                Authorization::Deny(format!(                    "{} always requires approval and no interactive client is attached;  the action was not performed",                    contract.name                ))
            };
        }
        if read_only || ctx.full_access {
            return Authorization::Allow;
        }
        if ctx.interactive {
            return Authorization::Ask;
        }
        Authorization::Deny(format!(
            "{} is not read-only and no interactive client is attached; \
             the action was not performed",
            contract.name
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
/// TS `semanticBoolean`: accepts a real boolean or the exact strings `"true"`/`"false"`.
/// Anything else is absent (the tool applies its documented default).
pub fn optional_semantic_bool(input: &Value, key: &str) -> Option<bool> {
    match input.get(key)? {
        Value::Bool(value) => Some(*value),
        Value::String(value) if value == "true" => Some(true),
        Value::String(value) if value == "false" => Some(false),
        _ => None,
    }
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
                max_model_bytes: 1024,
                preview_direction: "head",
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
            interactive: false,
            trace_id: "t".to_string(),
            turn_id: "turn".to_string(),
            read_state: Arc::new(Mutex::new(HashSet::new())),
            cancel: Arc::new(AtomicBool::new(false)),
            todos: crate::tools::todo::new_todo_store(),
            provider_config: None,

            session_id: "test-session".to_string(),

            model_ref: None,

            tool_call_id: None,

            plan_mode: false,

            ask_user: None,

            plan: None,

            subagent: None,

            background: None,

            sessions: None,

            workflow: None,

            automation: None,
            browser: None,
            computer: None,
            offpeak: None,
        }
    }
    #[test]
    fn authorize_fails_closed_without_full_access() {
        let registry = ToolRegistry::with_defaults();
        let tool = StubTool;
        assert!(matches!(
            registry.authorize(&tool, &json!({}), &ctx(false)),
            Authorization::Deny(_)
        ));
        assert_eq!(
            registry.authorize(&tool, &json!({}), &ctx(true)),
            Authorization::Allow
        );
        // 有交互客户端时可询问用户
        let mut interactive = ctx(false);
        interactive.interactive = true;
        assert_eq!(
            registry.authorize(&tool, &json!({}), &interactive),
            Authorization::Ask
        );
    }
    #[test]
    fn truncate_reports_and_stays_on_char_boundary() {
        let (text, truncated) = truncate_to_limit("héllo wörld".to_string(), 8);
        assert!(truncated);
        assert!(text.starts_with("héllo"));
    }
}
