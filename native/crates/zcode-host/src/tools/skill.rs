//! Skill：按名字加载 workspace 内的技能指令（替代 TS
//! `core/src/tool/handlers/skill.ts`）。
//!
//! 契约对齐 `contracts/tools/skill.ts`：输入 `{skill, args?}`；输出为技能内容。
//! **名字不猜**：只从 `available-skills` 发现到的技能目录里取；找不到即报错
//! （TS 同纪律——"Do not guess names"）。
//!
//! 发现规则（对齐 TS skill discovery 的最小可执行面）：workspace 根下的
//! `.claude/skills/<name>/SKILL.md` 与 `.agents/skills/<name>/SKILL.md` 两种
//! 约定目录；体积上限 100_000 字节（TS MAX_SKILL_BYTES）。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::fs_tools::resolve_workspace_path;
use super::{
    optional_string, require_string, string_schema, Tool, ToolContext, ToolContract, ToolError,
    ToolErrorCode,
};

const MAX_SKILL_BYTES: u64 = 100_000;

/// TS 的两种技能目录约定（`@` 风格）。目录名即技能名。
const SKILL_DIRS: [&str; 2] = [".claude/skills", ".agents/skills"];

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::new(ToolErrorCode::InvalidInput, message)
}

/// 找到技能文件路径；不存在返回 None（调用方给"unknown skill"错误）。
fn find_skill_file(root: &Path, name: &str) -> Option<PathBuf> {
    for base in SKILL_DIRS {
        let candidate = root.join(base).join(name).join("SKILL.md");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 列出 workspace 内可用技能名（供提示注入/自省；当前工具描述里不含此列表，
/// 但保留为测试与未来 context 注入的公共面）。
pub fn available_skills(root: &Path) -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    for base in SKILL_DIRS {
        if let Ok(entries) = std::fs::read_dir(root.join(base)) {
            for entry in entries.flatten() {
                if entry.path().join("SKILL.md").is_file() {
                    names.insert(entry.file_name().to_string_lossy().to_string());
                }
            }
        }
    }
    names.into_iter().collect()
}

pub struct SkillTool;

impl Tool for SkillTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "Skill".to_string(),
            description: "Load a reusable local skill's instructions by name. Names must come from the available skills list — never guess.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "skill": string_schema("The name of a skill from the available-skills list"),
                    "args": string_schema("Optional arguments for the skill"),
                },
                "required": ["skill"],
            }),
            read_only: true,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: crate::tools::SCOPE_NONE,
            timeout_ms: 5_000,
            max_output_bytes: 128 * 1024,
        }
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        if ctx.is_cancelled() {
            return Err(ToolError::new(ToolErrorCode::Cancelled, "turn cancelled"));
        }
        let name = require_string(input, "skill")?.trim();
        if name.is_empty() {
            return Err(invalid("skill must be non-empty"));
        }
        // 防目录穿越：技能名只能是安全段。
        if name.contains(['/', '\\', '.']) {
            return Err(invalid("skill name must be a simple identifier"));
        }
        let _args = optional_string(input, "args");

        let root = resolve_workspace_path(ctx, ".")?;
        let Some(path) = find_skill_file(&root, name) else {
            let available = available_skills(&root);
            return Err(invalid(format!(
                "unknown skill: {name} (available: {})",
                if available.is_empty() {
                    "none".to_string()
                } else {
                    available.join(", ")
                }
            )));
        };
        let metadata = std::fs::metadata(&path)
            .map_err(|error| invalid(format!("stat skill {name}: {error}")))?;
        if metadata.len() > MAX_SKILL_BYTES {
            return Err(invalid(format!(
                "skill {name} exceeds the {MAX_SKILL_BYTES} byte limit"
            )));
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|error| invalid(format!("read skill {name}: {error}")))?;
        Ok(json!({
            "skill": name,
            "content": content,
        })
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    fn ctx(root: &Path) -> ToolContext {
        ToolContext {
            workspace_root: root.to_path_buf(),
            full_access: false,
            interactive: false,
            trace_id: "t".to_string(),
            turn_id: "turn".to_string(),
            read_state: Arc::new(Mutex::new(std::collections::HashSet::new())),
            cancel: Arc::new(AtomicBool::new(false)),
            todos: crate::tools::todo::new_todo_store(),
            provider_config: None,
        }
    }

    fn workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zcode-skill-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("workspace");
        dir
    }

    #[test]
    fn loads_skill_from_claude_and_agents_conventions() {
        let root = workspace("load");
        for base in SKILL_DIRS {
            let dir = root.join(base).join("deploy");
            std::fs::create_dir_all(&dir).expect("skill dir");
            std::fs::write(dir.join("SKILL.md"), "# Deploy\nsteps").expect("write");
        }
        let output = SkillTool
            .execute(&json!({"skill": "deploy"}), &ctx(&root))
            .expect("load skill");
        let parsed: Value = serde_json::from_str(&output).expect("json");
        assert!(parsed["content"]
            .as_str()
            .unwrap_or_default()
            .contains("Deploy"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_skill_is_an_error_listing_available() {
        let root = workspace("unknown");
        let dir = root.join(".claude/skills/known");
        std::fs::create_dir_all(&dir).expect("skill dir");
        std::fs::write(dir.join("SKILL.md"), "x").expect("write");
        let error = SkillTool
            .execute(&json!({"skill": "ghost"}), &ctx(&root))
            .expect_err("unknown skill must fail");
        assert!(error.to_model_text().contains("known"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_path_traversal_names() {
        let root = workspace("traversal");
        assert!(SkillTool
            .execute(&json!({"skill": "../escape"}), &ctx(&root))
            .is_err());
        assert!(SkillTool
            .execute(&json!({"skill": "a/b"}), &ctx(&root))
            .is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn available_skills_discovers_both_conventions() {
        let root = workspace("discover");
        for (base, name) in [(".claude/skills", "one"), (".agents/skills", "two")] {
            let dir = root.join(base).join(name);
            std::fs::create_dir_all(&dir).expect("skill dir");
            std::fs::write(dir.join("SKILL.md"), "x").expect("write");
        }
        let names = available_skills(&root);
        assert_eq!(names, vec!["one".to_string(), "two".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
