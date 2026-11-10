//! Memory agent/extraction logic (ported concepts from TS core/src/memory).
//!
//! Full LLM-driven extraction loop wiring depends on host provider/tool runtime;
//! this module implements the real, testable business logic: extraction
//! decisions, prompt construction, path-safety helpers, and configuration/state.
//! No mocks/stubs - pure logic operates on real data structures.

use crate::memory_paths;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const MINIMUM_USER_WORDS: usize = 3;

/// Status of one extraction execution attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemoryExtractionStatus {
    Success,
    NoOp,
    Error,
    Aborted,
}

/// A manifest entry describing an existing memory file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MemoryManifestEntry {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// Snapshot of messages eligible for extraction (host provides real messages).
#[derive(Debug, Clone)]
pub struct MemoryExtractionSnapshot<M> {
    pub boundary_message_id: Option<String>,
    pub durable_messages: Vec<M>,
    pub memory_root: std::path::PathBuf,
    pub working_directory: std::path::PathBuf,
    pub workspace_root: std::path::PathBuf,
}

/// Decision about whether extraction should run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryExtractionDecision {
    Run {
        message_count: usize,
    },
    Skip {
        message_count: usize,
        reason: SkipReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkipReason {
    DirectMemoryWrite,
    NoUserProse,
    Empty,
}

#[derive(Debug, Clone)]
pub struct MemoryAgentConfig {
    pub max_turns: usize,
    pub max_messages: usize,
}

impl Default for MemoryAgentConfig {
    fn default() -> Self {
        Self {
            max_turns: 10,
            max_messages: 80,
        }
    }
}

/// Build the extraction prompt (aligned with TS `buildMemoryExtractionPrompt` intent).
pub fn build_memory_extraction_prompt(
    manifest: &[MemoryManifestEntry],
    message_count: usize,
) -> String {
    let existing = if manifest.is_empty() {
        String::new()
    } else {
        let mut lines = String::from("\n\n## Existing memory files\n\n");
        for entry in manifest {
            let title = entry.title.as_deref().unwrap_or(&entry.path);
            lines.push_str(&format!("- {}: {}\n", entry.path, title));
            if let Some(summary) = &entry.summary {
                if !summary.trim().is_empty() {
                    lines.push_str(&format!("  summary: {}\n", summary.trim()));
                }
            }
        }
        lines.push_str(
            "\nCheck this list before writing — update an existing file rather than creating a duplicate.",
        );
        lines
    };
    format!(
        "You are now acting as the memory extraction subagent. Analyze the most recent ~{message_count} messages above and use them to update your persistent memory systems.\n\n\
Available tools: Read, Grep, Glob, read-only Bash (ls/find/cat/stat/wc/head/tail and similar), and Edit/Write for paths inside the memory directory only, and Bash rm with paths inside the memory directory only. All other tools — MCP, Agent, write-capable Bash, etc. — will be denied.\n\n\
You have a limited turn budget. Edit requires a prior Read of the same file, so the efficient strategy is: turn 1 — issue all Read calls in parallel for every file you might update; turn 2 — issue all Write/Edit calls in parallel. Do not interleave reads and writes across multiple turns.\n\n\
You MUST only use content from the last ~{message_count} messages to update your persistent memories. Do not waste any turns attempting to investigate or verify that content further — no grepping source files, no reading code to confirm a pattern exists, no git commands.{existing}\n\n\
If nothing is worth saving, output only 'Nothing to save.' Do not explain why.\n\n\
If the user explicitly asks you to remember something, save it immediately as whichever type fits best. If they ask you to forget something, find and remove the relevant entry.\n\n\
Apply the memory types, what-not-to-save criteria, and frontmatter format from the Memory section of your system prompt — it is already in your context above."
    )
}

/// Count word-like tokens in user prose (simple, deterministic).
pub fn count_user_words(text: &str) -> usize {
    text.split_whitespace().count()
}

/// Evaluate whether extraction should run given counts and observed facts.
/// `has_direct_memory_write`: true if the message window already performed a
/// memory mutation (skip to avoid double-writing).
/// `has_eligible_user_prose`: true if there is enough user text to extract from.
pub fn evaluate_extraction(
    message_count: usize,
    has_direct_memory_write: bool,
    has_eligible_user_prose: bool,
) -> MemoryExtractionDecision {
    if message_count == 0 {
        return MemoryExtractionDecision::Skip {
            message_count,
            reason: SkipReason::Empty,
        };
    }
    if has_direct_memory_write {
        return MemoryExtractionDecision::Skip {
            message_count,
            reason: SkipReason::DirectMemoryWrite,
        };
    }
    if !has_eligible_user_prose {
        return MemoryExtractionDecision::Skip {
            message_count,
            reason: SkipReason::NoUserProse,
        };
    }
    MemoryExtractionDecision::Run { message_count }
}

/// Policy: is a memory mutation path allowed under `root_dir`?
pub fn is_allowed_memory_mutation(root_dir: &Path, candidate: &Path) -> bool {
    memory_paths::is_contained_markdown_mutation(root_dir, candidate)
}

/// Policy: is a bash command allowed for the memory agent?
/// Allows read-only inspection commands and `rm` only when targeting a safe
/// contained markdown file under the memory root (conservative heuristic).
pub fn is_allowed_memory_bash(command: &str, memory_root: &Path, _working_dir: &Path) -> bool {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Conservative read-only allowlist (prefix-based, no shell metacharacters that enable writes/redirection).
    let read_only_prefixes = [
        "ls", "find", "cat", "stat", "wc", "head", "tail", "pwd", "echo", "rg", "grep",
    ];
    let first = trimmed.split_whitespace().next().unwrap_or("");
    let has_write_metachar = trimmed.contains('>')
        || trimmed.contains(">>")
        || trimmed.contains('|') && trimmed.contains("tee")
        || trimmed.contains(';')
        || trimmed.contains("&&")
        || trimmed.contains("`")
        || trimmed.contains("$(");
    if read_only_prefixes.contains(&first) && !has_write_metachar {
        return true;
    }
    // Allow `rm <path>` only for a safe markdown file under memory root.
    if first == "rm" && !has_write_metachar {
        for token in trimmed.split_whitespace().skip(1) {
            if token.starts_with('-') {
                continue;
            }
            let path = Path::new(token);
            if !is_allowed_memory_mutation(memory_root, path) {
                return false;
            }
            return true;
        }
    }
    false
}

/// Cursor/state for extraction scheduling (append-only progress).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryExtractionCursor {
    pub last_boundary_message_id: Option<String>,
    pub last_status: Option<MemoryExtractionStatus>,
    pub runs: u64,
}

impl MemoryExtractionCursor {
    pub fn advance(&mut self, boundary: Option<String>, status: MemoryExtractionStatus) {
        if matches!(
            status,
            MemoryExtractionStatus::Success | MemoryExtractionStatus::NoOp
        ) && boundary.is_some()
        {
            self.last_boundary_message_id = boundary;
        }
        self.last_status = Some(status);
        self.runs = self.runs.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn prompt_includes_manifest_and_count() {
        let prompt = build_memory_extraction_prompt(
            &[MemoryManifestEntry {
                path: "notes.md".to_string(),
                title: Some("Notes".to_string()),
                summary: Some("stuff".to_string()),
                updated_at: None,
            }],
            12,
        );
        assert!(prompt.contains("~12 messages"));
        assert!(prompt.contains("notes.md"));
        assert!(prompt.contains("Nothing to save"));
    }

    #[test]
    fn extraction_decision_respects_guards() {
        assert!(matches!(
            evaluate_extraction(0, false, false),
            MemoryExtractionDecision::Skip {
                reason: SkipReason::Empty,
                ..
            }
        ));
        assert!(matches!(
            evaluate_extraction(5, true, true),
            MemoryExtractionDecision::Skip {
                reason: SkipReason::DirectMemoryWrite,
                ..
            }
        ));
        assert!(matches!(
            evaluate_extraction(5, false, false),
            MemoryExtractionDecision::Skip {
                reason: SkipReason::NoUserProse,
                ..
            }
        ));
        assert!(matches!(
            evaluate_extraction(5, false, true),
            MemoryExtractionDecision::Run { message_count: 5 }
        ));
    }

    #[test]
    fn memory_mutation_policy_allows_only_safe_md() {
        let root = PathBuf::from("/tmp/mem");
        assert!(is_allowed_memory_mutation(&root, Path::new("topic.md")));
        assert!(!is_allowed_memory_mutation(
            &root,
            Path::new("../escape.md")
        ));
        assert!(!is_allowed_memory_mutation(
            &root,
            Path::new(".git/config.md")
        ));
    }

    #[test]
    fn memory_bash_policy_is_conservative() {
        let root = PathBuf::from("/tmp/mem");
        let wd = PathBuf::from("/tmp/wd");
        assert!(is_allowed_memory_bash("ls -la", &root, &wd));
        assert!(is_allowed_memory_bash("cat notes.md", &root, &wd));
        assert!(!is_allowed_memory_bash("echo hi > notes.md", &root, &wd));
        assert!(!is_allowed_memory_bash("bash -c 'rm -rf /'", &root, &wd));
        assert!(!is_allowed_memory_bash("rm ../escape.md", &root, &wd));
    }

    #[test]
    fn cursor_advances_only_on_success_or_noop() {
        let mut cur = MemoryExtractionCursor::default();
        cur.advance(Some("m1".to_string()), MemoryExtractionStatus::Error);
        assert_eq!(cur.last_boundary_message_id, None);
        cur.advance(Some("m2".to_string()), MemoryExtractionStatus::NoOp);
        assert_eq!(cur.last_boundary_message_id.as_deref(), Some("m2"));
        cur.advance(Some("m3".to_string()), MemoryExtractionStatus::Success);
        assert_eq!(cur.last_boundary_message_id.as_deref(), Some("m3"));
        assert_eq!(cur.runs, 3);
    }
}
