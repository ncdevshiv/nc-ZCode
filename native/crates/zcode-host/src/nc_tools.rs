//! nc-tools 原生集成：把 `F:\nc-sdk\nc-tools` 的 89 个 typed 工具**进程内直接
//! 链接**进 host（不是 MCP server、不是 sidecar、不是子进程）。
//!
//! 为什么这样接：`nct-mcp::build_kernel(root)` 装配全部工具家族（fs/git/proc/
//! net/semantic + sys.batch/doctor + coordination/replay），`Kernel::call` 是
//! 同步、串行、按连接串行化的执行面——与 zcode-host 的 turn 串行语义天然一致。
//! 我们只取 kernel 与工具面，**不取它的 MCP 传输层**：传输由 zcode 的 v4 协议
//! 承担，工具面直接进 host 的 ToolRegistry。
//!
//! 工具名映射：模型看到的是 nc-tools 的点号名（`fs.read`、`git.status`……），
//! 通过 `NcTool` 代理暴露；TS 侧同名工具（Read/Edit/Bash…）与 nc-tools 并存，
//! 名字不冲突（README 差异表记录：这是能力超集，不是替换）。
//!
//! 隔离：每个会话一个 kernel 实例（root = workspace_root），journal 落在
//! `<workspace>/.nc-tools/journal.jsonl`——工具调用天然可审计（与 zcode 的
//! transcript 是两条独立事实链）。

use std::path::PathBuf;
use std::sync::Arc;

use nct_core::kernel::{CallOutcome, Kernel};
use nct_core::ToolError;
use serde_json::Value;

use crate::tools::{Tool, ToolContext, ToolContract, ToolError as HostToolError, ToolErrorCode};

/// 工具描述：Kernel::descriptors() 返回 `{name, description, inputSchema}`。
fn tool_descriptors(kernel: &Kernel) -> Vec<(String, String, Value)> {
    kernel
        .descriptors()
        .into_iter()
        .filter_map(|descriptor| {
            let name = descriptor.get("name")?.as_str()?.to_string();
            let description = descriptor
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let schema = descriptor
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"type": "object"}));
            Some((name, description, schema))
        })
        .collect()
}

/// 单个 nc-tools 工具的 host 侧代理。持有共享 kernel（Arc）+ 工具名。
pub struct NcTool {
    kernel: Arc<Kernel>,
    name: String,
    description: String,
    input_schema: Value,
}

impl NcTool {
    pub fn new(
        kernel: Arc<Kernel>,
        name: String,
        description: String,
        input_schema: Value,
    ) -> Self {
        Self {
            kernel,
            name,
            description,
            input_schema,
        }
    }

    pub fn tool_name(&self) -> &str {
        &self.name
    }
}

impl Tool for NcTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
            // nc-tools 自己做权限面（readOnlyHint / 声明式变更），host 不二次猜测：
            // 默认按「可读」放行，破坏性由 nc-tools 的 patch/stage 语义兜住
            // （写入必须走校验过的 patch，见 SDK README 原则 3）。
            read_only: true,
            destructive: false,
            concurrent_safe: false,
            side_effect_scope: crate::tools::SCOPE_WORKSPACE,
            timeout_ms: 120_000,
            max_output_bytes: 4 * 1024 * 1024,
        }
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, HostToolError> {
        if ctx.is_cancelled() {
            return Err(HostToolError::new(
                ToolErrorCode::Cancelled,
                "turn cancelled",
            ));
        }
        // Tool::execute 是同步面（与仓库其余工具一致）。Kernel::call 本身是同步且
        // 内部有界等待（proc.start 走线程、net.http 有上限），所以这里直接调用；
        // 需要真超时的长操作由 nc-tools 自己的 handler 契约承担。
        render_outcome(self.kernel.call(&self.name, input))
    }
}

/// CallOutcome → 工具结果文本（JSON 字符串；错误走 typed text 回灌模型）。
fn render_outcome(outcome: CallOutcome) -> Result<String, HostToolError> {
    if outcome.ok {
        return Ok(
            serde_json::to_string(&outcome.result.unwrap_or(Value::Null))
                .unwrap_or_else(|_| "null".to_string()),
        );
    }
    let error = outcome.error.unwrap_or_else(|| {
        ToolError::new(nct_core::errors::codes::INTERNAL, "nc-tools call failed")
    });
    Err(HostToolError::new(
        ToolErrorCode::ExecutionFailed,
        format!("[{}] {}", error.code, error.message),
    ))
}

/// 构建 host 的 nc-tools 工具集（每个 workspace/session 一个 kernel）。
///
/// 失败（kernel 构造/journal 不可写）返回 Err：host 启动即失败，不做"部分工具面"
/// 降级——工具面缺失会让模型以为能力不存在，比启动失败更危险。
pub fn build_nc_tools(workspace_root: &std::path::Path) -> Result<Vec<Arc<NcTool>>, HostToolError> {
    let root: PathBuf = workspace_root.to_path_buf();
    let kernel = nct_mcp::build_kernel(root).map_err(|error| {
        HostToolError::new(
            ToolErrorCode::ExecutionFailed,
            format!(
                "nc-tools kernel init failed: [{}] {}",
                error.code, error.message
            ),
        )
    })?;
    // session 默认 base 锚到 workspace（与 zcode 的 workspaceIdentity 同一目录）。
    kernel.set_default_base(workspace_root, "roots");
    let shared = Arc::new(kernel);
    let tools = tool_descriptors(&shared)
        .into_iter()
        .map(|(name, description, input_schema)| {
            Arc::new(NcTool::new(
                Arc::clone(&shared),
                name,
                description,
                input_schema,
            ))
        })
        .collect();
    Ok(tools)
}

/// 供 turn 投影与自省：kernel 上的工具名（排序）。
pub fn nc_tool_names(kernel: &Kernel) -> Vec<String> {
    kernel.list_tools()
}
