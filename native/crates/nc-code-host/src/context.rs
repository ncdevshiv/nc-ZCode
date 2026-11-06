//! Context assembly primitives (ported concepts from TS core/src/context).
//!
//! Provides portable types for context sections, cache hints, and simple
//! budgeting/truncation helpers used by host prompt/context builders.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSource {
    CliPrefix,
    Identity,
    EnvInfo,
    SystemContext,
    Skills,
    Tools,
    RequestUserContext,
    Memory,
    CurrentDate,
    CustomSystemPrompt,
    WorkflowActorIdentity,
    SubagentAgentPrompt,
    SubagentNotes,
    SubagentEnvironment,
    DynamicBehavior,
    SessionGuidance,
    OutputStyle,
    ContextManagement,
    DesktopContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextInjectionTarget {
    System,
    MetaUser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextCacheHint {
    Stable,
    Dynamic,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextSection {
    pub name: String,
    pub source: ContextSource,
    pub injection_target: ContextInjectionTarget,
    pub cache_hint: ContextCacheHint,
    pub content: String,
    pub chars: usize,
    pub estimated_tokens: usize,
}

impl ContextSection {
    pub fn new(
        name: impl Into<String>,
        source: ContextSource,
        injection_target: ContextInjectionTarget,
        cache_hint: ContextCacheHint,
        content: impl Into<String>,
    ) -> Self {
        let content = content.into();
        let chars = content.chars().count();
        let estimated_tokens = estimate_tokens(chars);
        Self {
            name: name.into(),
            source,
            injection_target,
            cache_hint,
            content,
            chars,
            estimated_tokens,
        }
    }

    pub fn preview(&self, max_chars: usize) -> String {
        self.content.chars().take(max_chars).collect()
    }
}

/// Rough token estimate (chars/4 heuristic), deterministic and cheap.
pub fn estimate_tokens(chars: usize) -> usize {
    (chars.saturating_add(3)) / 4
}

#[derive(Debug, Clone, Default)]
pub struct ContextBudget {
    pub max_chars: usize,
    pub max_tokens: usize,
}

impl ContextBudget {
    pub fn fits(&self, chars: usize, tokens: usize) -> bool {
        (self.max_chars == 0 || chars <= self.max_chars)
            && (self.max_tokens == 0 || tokens <= self.max_tokens)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ContextBuildSummary {
    pub total_chars: usize,
    pub total_tokens: usize,
    pub section_count: usize,
    pub truncated_sections: usize,
}

/// Append sections while respecting budget; truncates individual oversized
/// sections with an explicit marker. Returns kept sections and summary.
pub fn fit_sections(
    sections: Vec<ContextSection>,
    budget: &ContextBudget,
) -> (Vec<ContextSection>, ContextBuildSummary) {
    let mut kept = Vec::with_capacity(sections.len());
    let mut summary = ContextBuildSummary::default();
    let mut used_chars = 0usize;
    let mut used_tokens = 0usize;
    let unlimited_chars = budget.max_chars == 0;
    let unlimited_tokens = budget.max_tokens == 0;
    // If both are explicitly zero, treat as no budget (drop all).
    if budget.max_chars == 0 && budget.max_tokens == 0 {
        summary.section_count = sections.len();
        summary.truncated_sections = sections.len();
        return (Vec::new(), summary);
    }
    for mut section in sections {
        summary.section_count += 1;
        let remaining_chars = if unlimited_chars {
            usize::MAX
        } else {
            budget.max_chars.saturating_sub(used_chars)
        };
        let remaining_tokens = if unlimited_tokens {
            usize::MAX
        } else {
            budget.max_tokens.saturating_sub(used_tokens)
        };
        if remaining_chars == 0 || remaining_tokens == 0 {
            summary.truncated_sections += 1;
            continue;
        }
        if section.chars > remaining_chars || section.estimated_tokens > remaining_tokens {
            let marker = "\n\n[context truncated]";
            let marker_len = marker.chars().count();
            if remaining_chars <= marker_len {
                // Not enough room even for marker; hard truncate to remaining chars.
                section.content = section.content.chars().take(remaining_chars).collect();
            } else {
                let take = remaining_chars.saturating_sub(marker_len);
                let truncated: String = section.content.chars().take(take).collect();
                section.content = format!("{truncated}{marker}");
            }
            section.chars = section.content.chars().count();
            section.estimated_tokens = estimate_tokens(section.chars);
            // Ensure we never exceed budget due to estimation.
            if !unlimited_chars && section.chars > remaining_chars {
                section.content = section.content.chars().take(remaining_chars).collect();
                section.chars = section.content.chars().count();
                section.estimated_tokens = estimate_tokens(section.chars);
            }
            summary.truncated_sections += 1;
        }
        used_chars = used_chars.saturating_add(section.chars);
        used_tokens = used_tokens.saturating_add(section.estimated_tokens);
        kept.push(section);
    }
    summary.total_chars = used_chars;
    summary.total_tokens = used_tokens;
    (kept, summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_preview_and_estimates() {
        let s = ContextSection::new(
            "identity",
            ContextSource::Identity,
            ContextInjectionTarget::System,
            ContextCacheHint::Stable,
            "hello world",
        );
        assert_eq!(s.chars, 11);
        assert!(s.estimated_tokens >= 2);
        assert_eq!(s.preview(5), "hello");
    }

    #[test]
    fn fit_sections_truncates_with_marker() {
        let sections = vec![ContextSection::new(
            "big",
            ContextSource::Skills,
            ContextInjectionTarget::MetaUser,
            ContextCacheHint::Dynamic,
            "abcdefghijklmnopqrstuvwxyz0123456789", // 36 chars
        )];
        let budget = ContextBudget {
            max_chars: 30,
            max_tokens: 100,
        };
        let (kept, summary) = fit_sections(sections, &budget);
        assert_eq!(kept.len(), 1);
        assert_eq!(summary.truncated_sections, 1);
        assert!(kept[0].content.contains("[context truncated]"));
        assert!(kept[0].chars <= 30);
    }

    #[test]
    fn fit_sections_drops_when_no_budget() {
        let sections = vec![ContextSection::new(
            "a",
            ContextSource::EnvInfo,
            ContextInjectionTarget::System,
            ContextCacheHint::Stable,
            "12345",
        )];
        let budget = ContextBudget {
            max_chars: 0,
            max_tokens: 0,
        };
        let (kept, summary) = fit_sections(sections, &budget);
        assert!(kept.is_empty());
        assert_eq!(summary.truncated_sections, 1);
    }
}
