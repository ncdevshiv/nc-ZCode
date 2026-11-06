//! MCP 工具的 host 侧代理：把 `McpPool` 里的外部工具接进 `ToolRegistry`。
//!
//! 与 `nc_tools` 的 `NcTool` 同构：`Tool` 实现持有 pool + 工具名，执行时转发到
//! `McpPool::call_tool`。硬化策略：
//!
//! - 解析 server `annotations`（`readOnlyHint`/`destructiveHint` 等）用于权限判定；
//! - **无明确只读标注时 fail-closed**（`readOnly=false`），非只读在 headless
//!   无交互客户端时默认拒绝，除非 `--full-access`；
//! - 显式 `readOnlyHint=true && destructiveHint!=true` 时可标记只读（仍受 host 策略约束）。
use crate::mcp::{McpPool, McpTool as Descriptor};
use crate::tools::{Tool, ToolContext, ToolContract, ToolError, ToolErrorCode};
use serde_json::Value;
use std::sync::Arc;
pub struct McpToolProxy {
    pool: Arc<McpPool>,
    descriptor: Descriptor,
}
impl McpToolProxy {
    pub fn new(pool: Arc<McpPool>, descriptor: Descriptor) -> Self {
        Self { pool, descriptor }
    }
}
impl Tool for McpToolProxy {
    fn contract(&self) -> ToolContract {
        let read_only = self.descriptor.read_only_hint == Some(true)
            && self.descriptor.destructive_hint != Some(true);
        let destructive = self.descriptor.destructive_hint == Some(true);
        ToolContract {
            name: self.descriptor.qualified_name(),
            description: if self.descriptor.description.is_empty() {
                format!(
                    "External MCP tool `{}` from server `{}`.",
                    self.descriptor.tool, self.descriptor.server
                )
            } else {
                self.descriptor.description.clone()
            },
            input_schema: self.descriptor.input_schema.clone(),
            // Hardened: honor explicit readOnlyHint when not destructive; otherwise
            // fail closed (requires approval in non-full-access contexts).
            read_only,
            destructive,
            concurrent_safe: self.descriptor.idempotent_hint.unwrap_or(true),
            side_effect_scope: if read_only {
                crate::tools::SCOPE_SYSTEM
            } else {
                crate::tools::SCOPE_SYSTEM
            },
            timeout_ms: 120_000,
            max_output_bytes: 1024 * 1024,
            max_model_bytes: 256 * 1024,
            preview_direction: "head",
        }
    }
    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        if ctx.is_cancelled() {
            return Err(ToolError::new(ToolErrorCode::Cancelled, "turn cancelled"));
        }
        self.pool
            .call_tool(&self.descriptor.qualified_name(), input)
            .map_err(|error| {
                ToolError::new(
                    ToolErrorCode::ExecutionFailed,
                    format!("[mcp:{}] {error}", self.descriptor.server),
                )
            })
    }
}
/// 把 pool 里当前已连接的全部工具包装成 host 工具。
pub fn proxies_for(pool: &Arc<McpPool>) -> Vec<Arc<McpToolProxy>> {
    pool.tools()
        .into_iter()
        .map(|descriptor| Arc::new(McpToolProxy::new(Arc::clone(pool), descriptor)))
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;
    use serde_json::json;
    use std::collections::HashSet;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;
    fn ctx(full_access: bool) -> ToolContext {
        ToolContext {
            workspace_root: std::env::temp_dir(),
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
    fn descriptor() -> Descriptor {
        Descriptor {
            server: "github".to_string(),
            tool: "create_issue".to_string(),
            description: "Create an issue".to_string(),
            input_schema: json!({"type": "object"}),
            read_only_hint: None,
            destructive_hint: None,
            idempotent_hint: None,
            open_world_hint: None,
        }
    }

    #[test]
    fn explicit_read_only_annotation_is_honored() {
        let pool = Arc::new(McpPool::new());
        let mut d = descriptor();
        d.read_only_hint = Some(true);
        d.destructive_hint = Some(false);
        let proxy = McpToolProxy::new(Arc::clone(&pool), d);
        assert!(proxy.contract().read_only);
    }

    #[test]
    fn destructive_annotation_overrides_read_only_hint() {
        let pool = Arc::new(McpPool::new());
        let mut d = descriptor();
        d.read_only_hint = Some(true);
        d.destructive_hint = Some(true);
        let proxy = McpToolProxy::new(Arc::clone(&pool), d);
        let c = proxy.contract();
        assert!(!c.read_only);
        assert!(c.destructive);
    }
    #[test]
    fn contract_uses_qualified_name_and_defaults_fail_closed() {
        let pool = Arc::new(McpPool::new());
        let proxy = McpToolProxy::new(Arc::clone(&pool), descriptor());
        let contract = proxy.contract();
        assert_eq!(contract.name, "mcp__github__create_issue");
        assert_eq!(contract.description, "Create an issue");
        assert!(
            !contract.read_only,
            "MCP tools without explicit readOnlyHint must fail closed"
        );
    }
    #[test]
    fn empty_pool_yields_no_proxies() {
        let pool = Arc::new(McpPool::new());
        assert!(proxies_for(&pool).is_empty());
    }
    #[test]
    fn connected_tools_join_the_host_registry() {
        // 真实 server 子进程：起一个最小 MCP server，连接后其工具必须出现在
        // ToolRegistry 的 names() 里（模型才看得见）。
        const SERVER: &str = r#"
const readline = require('node:readline');
const rl = readline.createInterface({ input: process.stdin });
rl.on('line', (line) => {
  let msg; try { msg = JSON.parse(line); } catch { return; }
  if (msg.method === 'initialize') {
    send({ jsonrpc: '2.0', id: msg.id, result: { protocolVersion: '2025-06-18', capabilities: { tools: {} } } });
  } else if (msg.method === 'tools/list') {
    send({ jsonrpc: '2.0', id: msg.id, result: { tools: [
      { name: 'ping', description: 'Ping the server', inputSchema: { type: 'object' } } ] } });
  } else if (msg.method === 'tools/call') {
    send({ jsonrpc: '2.0', id: msg.id, result: { content: [{ type: 'text', text: 'pong' }] } });
  }
});
function send(o) { process.stdout.write(JSON.stringify(o) + '\n'); }
setInterval(() => {}, 1 << 30);
"#;
        let pool = Arc::new(McpPool::new());
        pool.connect_stdio("probe", "node", &["-e".to_string(), SERVER.to_string()])
            .expect("connect");
        let registry = ToolRegistry::with_defaults().with_mcp_tools(proxies_for(&pool));
        assert!(
            registry.names().contains(&"mcp__probe__ping".to_string()),
            "MCP tool must be visible to the model: {:?}",
            registry.names()
        );
        // 真实执行：模型调 mcp__probe__ping → server 回 pong。
        let tool = registry.get("mcp__probe__ping").expect("tool in registry");
        let result = tool
            .execute(&json!({}), &ctx(true))
            .expect("MCP tool call must succeed");
        assert_eq!(result, "pong");
        pool.disconnect("probe");
    }
    #[test]
    fn calling_a_disconnected_tool_is_a_typed_error_not_a_silent_success() {
        let pool = Arc::new(McpPool::new());
        let proxy = McpToolProxy::new(Arc::clone(&pool), descriptor());
        let error = proxy
            .execute(&json!({}), &ctx(true))
            .expect_err("disconnected tool must fail");
        assert_eq!(error.code, ToolErrorCode::ExecutionFailed);
        assert!(error.to_model_text().contains("mcp:github"));
    }
}
