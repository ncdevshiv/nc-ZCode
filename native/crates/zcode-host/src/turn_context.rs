//! Context 装配：系统提示 + 工具面路由 + AGENTS/CLAUDE 发现 + 技能目录 + 环境事实。
//!
//! 与 TS context builder 的关系（README 差异表）：本 increment 装配 workspace 根的
//! AGENTS.md / CLAUDE.md、技能目录清单与最小环境事实；memory 索引、
//! `~/.zcode` 用户级配置、MCP 工具提示仍未接线（OPEN）。发现结果**只列名字与
//! 摘要**——技能正文按需由 Skill 工具加载（TS 同设计：不要把全部 skill 塞进
//! context）。清单有长度上限，防止大仓库吃掉 context（TS 同样有界）。

use std::path::Path;

use serde_json::json;
use zcode_core::identity::identity_key;
use zcode_provider::request::{ContentPart, Message, MessageRole};

const SYSTEM_PROMPT: &str = "You are zcode-host, the native Rust agent host of ZCode. \
You run inside the user's workspace. Rules: prefer Read/Glob/Grep to understand before \
acting; Edit requires a prior Read of that file; destructive actions require the \
operator to have granted full access; never claim an action succeeded without a tool \
result proving it; report blockers honestly instead of guessing.";

#[derive(Debug, Clone)]
pub struct ContextFacts {
    pub system_blocks: Vec<String>,
    pub workspace_identity: String,
    pub instruction_files: Vec<String>,
    /// 工作区可用技能名（排序去重）；正文由 Skill 工具按需加载。
    pub available_skills: Vec<String>,
    /// 是否注入了 MEMORY.md 索引（缺失/空文件为 false）。
    pub memory_loaded: bool,
    pub estimated_context_bytes: usize,
}

/// 发现并注入 instruction 文件（存在才注入；读失败不致命——但要说出来）。
fn discover_instruction_files(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let path = root.join(name);
        if let Ok(metadata) = std::fs::metadata(&path) {
            if metadata.is_file() {
                found.push(name.to_string());
            }
        }
    }
    found
}

pub fn assemble_context(workspace_root: &Path) -> ContextFacts {
    assemble_context_with_memory(workspace_root, None)
}

/// 同 `assemble_context`，但额外注入 memory root 下的 `MEMORY.md` 索引。
/// host 启动时用 data-dir/memory 作为默认 memory root。
pub fn assemble_context_with_memory(
    workspace_root: &Path,
    memory_root: Option<&Path>,
) -> ContextFacts {
    let workspace_identity = identity_key(None, &workspace_root.to_string_lossy());
    let instruction_files = discover_instruction_files(workspace_root);
    let available_skills = crate::tools::skill::available_skills(workspace_root);
    let memory_index = memory_root.and_then(crate::memory::read_memory_index);
    let mut system_blocks = vec![SYSTEM_PROMPT.to_string()];

    // 工具面路由指引：zcode 原生工具与 nc-tools 家族并存时，明确每类该用哪个
    // （裁定见 reconciliation.rs / RUST-PORT-LOG ENTRY 016）。
    system_blocks.push(crate::reconciliation::overlap_routing_hint().to_string());

    if !instruction_files.is_empty() {
        system_blocks.push(format!(
            "Workspace instruction files present: {}. Read them with the Read tool before acting; \
             they carry project rules that override your defaults.",
            instruction_files.join(", ")
        ));
    }

    // 技能目录：只列名字，正文按需用 Skill 工具加载（TS 同设计）。
    if !available_skills.is_empty() {
        system_blocks.push(format!(
            "Available skills: {}. Load one with the Skill tool only when its instructions \
             are relevant to the task; do not guess names that are not listed.",
            available_skills.join(", ")
        ));
    }

    // 项目记忆索引（存在才注入；缺失/读失败都不是错误）。
    if let Some(index) = memory_index.filter(|index| !index.is_empty()) {
        system_blocks.push(format!(
            "Project memory index (from MEMORY.md — durable facts learned in this workspace; \
             trust it over assumptions, and update it with the Edit tool when facts change):\n\n{index}"
        ));
    }

    system_blocks.push(
        json!({
            "environment": {
                "workspace": workspace_root.to_string_lossy(),
                "workspaceIdentity": workspace_identity,
                "platform": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
            }
        })
        .to_string(),
    );

    let estimated_context_bytes = system_blocks.iter().map(|block| block.len()).sum();
    ContextFacts {
        system_blocks,
        workspace_identity,
        instruction_files,
        available_skills,
        memory_loaded: memory_root
            .is_some_and(|root| root.join(crate::memory::MEMORY_FILE_NAME).exists()),
        estimated_context_bytes,
    }
}

/// microcompact 阈值（字符）。超过后机械压缩最早的工具结果——不调用模型
/// （LLM 摘要压缩是 OPEN 项，需要 provider；机械压缩无需模型即可生效）。
pub const MICROCOMPACT_THRESHOLD_CHARS: usize = 120_000;
const TOOL_RESULT_KEEP_CHARS: usize = 2_000;

/// 机械 microcompact：把最早的大段 tool 结果瘦身，保留最近若干条不动。
/// 返回被压缩的消息数（0 = 未触发）。
pub fn microcompact_messages(messages: &mut [Message]) -> usize {
    let total: usize = messages.iter().map(estimate_message_chars).sum();
    if total <= MICROCOMPACT_THRESHOLD_CHARS {
        return 0;
    }
    // 从最旧往里压：保留最后 6 条消息原样（它们是当前工作集）。
    let keep_tail = 6.min(messages.len());
    let compress_end = messages.len().saturating_sub(keep_tail);
    let mut compressed = 0;
    for message in messages.iter_mut().take(compress_end) {
        if message.role != MessageRole::Tool {
            continue;
        }
        let before = estimate_message_chars(message);
        for part in message.parts.iter_mut() {
            if let ContentPart::Text { text } = part {
                if text.chars().count() > TOOL_RESULT_KEEP_CHARS {
                    let mut shortened: String = text.chars().take(TOOL_RESULT_KEEP_CHARS).collect();
                    shortened.push_str("\n[microcompacted: older tool result trimmed by the host]");
                    *text = shortened;
                }
            }
        }
        if estimate_message_chars(message) < before {
            compressed += 1;
        }
    }
    compressed
}

fn estimate_message_chars(message: &Message) -> usize {
    message
        .parts
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => text.len(),
            _ => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_workspace(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zcode-host-ctx-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("workspace");
        dir
    }

    #[test]
    fn context_discovers_instruction_files_and_identity() {
        let root = temp_workspace("discover");
        fs::write(root.join("AGENTS.md"), "# rules\nbe honest\n").expect("write");
        let facts = assemble_context(&root);
        assert_eq!(facts.instruction_files, vec!["AGENTS.md".to_string()]);
        assert!(facts.workspace_identity.contains("zcode-host-ctx-discover"));
        assert!(facts
            .system_blocks
            .iter()
            .any(|block| block.contains("AGENTS.md")));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_workspace_gets_baseline_prompt_only() {
        let root = temp_workspace("empty");
        let facts = assemble_context(&root);
        assert!(facts.instruction_files.is_empty());
        assert!(facts.system_blocks.len() >= 2);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn memory_index_is_injected_when_present_and_absent_when_not() {
        let root = temp_workspace("memory");
        let memory_dir = root.join("memory");
        fs::create_dir_all(&memory_dir).expect("memory dir");

        // 无 MEMORY.md → 不注入、不报错。
        let without = assemble_context_with_memory(&root, Some(&memory_dir));
        assert!(!without.memory_loaded);
        assert!(
            !without
                .system_blocks
                .iter()
                .any(|block| block.contains("Project memory index")),
            "missing memory must not inject a block"
        );

        // 有 MEMORY.md → 注入索引内容。
        fs::write(
            memory_dir.join("MEMORY.md"),
            "- the native host is the default agent runtime\n",
        )
        .expect("write");
        let with = assemble_context_with_memory(&root, Some(&memory_dir));
        assert!(with.memory_loaded);
        let block = with
            .system_blocks
            .iter()
            .find(|block| block.contains("Project memory index"))
            .expect("memory block");
        assert!(block.contains("native host is the default agent runtime"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn available_skills_are_listed_by_name_only() {
        let root = temp_workspace("skills");
        for (base, name) in [(".claude/skills", "deploy"), (".agents/skills", "review")] {
            let dir = root.join(base).join(name);
            fs::create_dir_all(&dir).expect("skill dir");
            fs::write(
                dir.join("SKILL.md"),
                "# Secret body\nmust not be in context",
            )
            .expect("write");
        }
        let facts = assemble_context(&root);
        assert_eq!(
            facts.available_skills,
            vec!["deploy".to_string(), "review".to_string()]
        );
        let block = facts
            .system_blocks
            .iter()
            .find(|block| block.contains("Available skills"))
            .expect("skills block");
        assert!(block.contains("deploy") && block.contains("review"));
        assert!(
            !block.contains("Secret body"),
            "skill bodies must be loaded on demand, not injected"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn microcompact_trims_old_tool_results_and_keeps_tail() {
        let mut messages: Vec<Message> = Vec::new();
        for index in 0..12 {
            let mut message = Message::text(MessageRole::Tool, "x".repeat(20_000));
            message.tool_call_id = Some(format!("call-{index}"));
            messages.push(message);
        }
        let before_total: usize = messages.iter().map(estimate_message_chars).sum();
        assert!(before_total > MICROCOMPACT_THRESHOLD_CHARS);
        let compressed = microcompact_messages(&mut messages);
        assert!(compressed > 0);
        let after_total: usize = messages.iter().map(estimate_message_chars).sum();
        assert!(after_total < before_total);
        for message in messages.iter().skip(messages.len() - 6) {
            assert_eq!(estimate_message_chars(message), 20_000);
        }
    }

    #[test]
    fn microcompact_is_noop_below_threshold() {
        let mut messages = vec![Message::text(MessageRole::Tool, "small")];
        assert_eq!(microcompact_messages(&mut messages), 0);
        match &messages[0].parts[0] {
            ContentPart::Text { text } => assert_eq!(text, "small"),
            other => panic!("unexpected part: {other:?}"),
        }
    }
}
