//! Named subagent profiles (the ported subset of TS `core/src/subagent/profile.ts` / `general-purpose.ts` /
//! `explore.ts` / `profile-frontmatter.ts`).
//!
//! The directories and precedence copy bootstrap's `loadZCodeAgentProfiles` verbatim: user-level `<data-dir>/agents/*.md`,
//! then project-level `<workspace>/.zcode/agents/*.md` (a later same name overrides the earlier one); the built-in
//! `general-purpose` and `Explore` are present first, and a custom profile with the same name overrides them.
//!
//! Boundaries with TS (recorded honestly, see `specs/rust-native-crates.md` S3 "named subagents"):
//! - Supported frontmatter: `name` / `description` / `model` / `thoughtLevel` / `tools` /
//!   `disallowedTools` / `injectAgentsMd` / `maxTurns`. `color` is UI decoration and this host has no
//!   consumer for it (subagent rows do not project a color), so it is accepted but ignored.
//! - **Unsupported frontmatter**: `permissionMode` / `memory` / `skills` / `mcpServers` /
//!   `background`. They change the child runtime's capability surface rather than its display surface; this host has no corresponding subsystem,
//!   so a profile carrying these fields is **refused at load** with the `agent_unsupported_frontmatter` diagnostic —
//!   silently degrading into an agent with fewer capabilities is worse than saying plainly what is missing.
//! - A model must be an entry in this host's static catalog (`--provider-config`); when it cannot be resolved, the profile is refused with
//!   `agent_unknown_model`.
//! - Other unknown keys are ignored, following TS's lenient parsing.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::provider_config::ProviderConfig;
use crate::tools::workflow::support;

pub const GENERAL_PURPOSE_AGENT: &str = "general-purpose";
pub const EXPLORE_AGENT: &str = "Explore";
/// The TS child-runtime default turn cap (`request.maxTurns ?? subagents.maxTurns ?? 4`).
pub const DEFAULT_MAX_TURNS: usize = 4;

/// The tools stripped from every subagent (TS `SUBAGENT_CHILD_FORCED_DISALLOWED_TOOLS`): a subagent has no
/// plan approval resume surface, so exposing the plan tools would make it block the parent turn while waiting for a user who does not exist.
const FORCED_DISALLOWED: [&str; 2] = ["EnterPlanMode", "ExitPlanMode"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileSource {
    BuiltIn,
    User,
    Project,
}

#[derive(Clone, Debug)]
pub struct AgentProfile {
    pub name: String,
    pub description: String,
    pub source: ProfileSource,
    pub system_prompt: String,
    /// `None` = all tools (TS `tools: ["*"]` is synonymous with the default).
    pub tools: Option<Vec<String>>,
    pub disallowed_tools: Vec<String>,
    /// The canonical form `providerId/modelId[$level]`; `None` = inherit the parent session's model.
    pub model: Option<String>,
    pub thought_level: Option<String>,
    pub max_turns: Option<usize>,
    pub inject_agents_md: bool,
    pub path: Option<String>,
}

impl AgentProfile {
    /// TS `isBuiltInExploreAgentProfile`: a user profile with the same name must not be given the built-in read-only behaviour.
    pub fn is_builtin_explore(&self) -> bool {
        self.name == EXPLORE_AGENT && self.source == ProfileSource::BuiltIn
    }

    /// The child registry's tool allowlist (every forced/declared disallowment already removed); `None` = no restriction.
    pub fn tool_allowlist(&self) -> Option<Vec<String>> {
        self.tools.as_ref().map(|tools| {
            tools
                .iter()
                .filter(|name| !self.tool_disallowlist().contains(name))
                .cloned()
                .collect()
        })
    }

    pub fn tool_disallowlist(&self) -> Vec<String> {
        FORCED_DISALLOWED
            .iter()
            .map(|name| name.to_string())
            .chain(self.disallowed_tools.iter().cloned())
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileDiagnostic {
    pub code: String,
    pub message: String,
    pub path: Option<String>,
}

impl ProfileDiagnostic {
    fn new(code: &str, message: impl Into<String>, path: Option<&Path>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            path: path.map(|path| path.to_string_lossy().to_string()),
        }
    }
}

pub fn builtin_general_purpose() -> AgentProfile {
    AgentProfile {
        name: GENERAL_PURPOSE_AGENT.to_string(),
        description: "General-purpose agent for researching complex questions, searching for code, and executing multi-step tasks. When you are searching for a keyword or file and are not confident that you will find the right match in the first few tries use this agent to perform the search for you.".to_string(),
        source: ProfileSource::BuiltIn,
        system_prompt: general_purpose_prompt(),
        tools: None,
        disallowed_tools: Vec::new(),
        model: None,
        thought_level: None,
        max_turns: None,
        inject_agents_md: true,
        path: None,
    }
}

pub fn builtin_explore() -> AgentProfile {
    AgentProfile {
        name: EXPLORE_AGENT.to_string(),
        description: "Read-only search agent for broad fan-out searches - when answering means sweeping many files, directories, or naming conventions and you only need the conclusion, not the file dumps. It reads excerpts rather than whole files, so it locates code; it doesn't review or audit it. Specify search breadth: \"medium\" for moderate exploration, \"very thorough\" for multiple locations and naming conventions.".to_string(),
        source: ProfileSource::BuiltIn,
        system_prompt: explore_prompt(),
        tools: Some(
            [
                "Bash",
                "Glob",
                "Grep",
                "Read",
                "WebFetch",
                "WebSearch",
                "TodoWrite",
            ]
            .iter()
            .map(|name| name.to_string())
            .collect(),
        ),
        disallowed_tools: Vec::new(),
        model: None,
        thought_level: None,
        max_turns: None,
        inject_agents_md: false,
        path: None,
    }
}

fn general_purpose_prompt() -> String {
    [
        "You are an agent for ZCode CLI. Given the user's message, you should use the tools available to complete the task. Complete the task fully—don't gold-plate, but don't leave it half-done. When you complete the task, respond with a concise report covering what was done and any key findings — the caller will relay this to the user, so it only needs the essentials.",
        "",
        "Your strengths:",
        "- Searching for code, configurations, and patterns across large codebases",
        "- Analyzing multiple files to understand system architecture",
        "- Investigating complex questions that require exploring many files",
        "- Performing multi-step research tasks",
        "",
        "Guidelines:",
        "- For file searches: search broadly when you don't know where something lives. Use Read when you know the specific file path.",
        "- For analysis: Start broad and narrow down. Use multiple search strategies if the first doesn't yield results.",
        "- Be thorough: Check multiple locations, consider different naming conventions, look for related files.",
        "- NEVER create files unless they're absolutely necessary for achieving your goal. ALWAYS prefer editing an existing file to creating a new one.",
        "- NEVER proactively create documentation files (*.md) or README files. Only create documentation files if explicitly requested.",
    ]
    .join("\n")
}

fn explore_prompt() -> String {
    [
        "You are ZCode Explore, a file search and codebase research specialist for ZCode CLI. You excel at thoroughly navigating and exploring codebases.",
        "",
        "=== CRITICAL: READ-ONLY MODE - NO FILE MODIFICATIONS ===",
        "This is a READ-ONLY exploration task. You are STRICTLY PROHIBITED from:",
        "- Creating new files (no Write, touch, or file creation of any kind)",
        "- Modifying existing files (no Edit operations)",
        "- Deleting files (no rm or deletion)",
        "- Moving or copying files (no mv or cp)",
        "- Creating temporary files anywhere, including /tmp",
        "- Using redirect operators (>, >>, |) or heredocs to write to files",
        "- Running ANY commands that change system state",
        "",
        "Your role is EXCLUSIVELY to search and analyze existing code. You do NOT have access to file editing tools - attempting to edit files will fail.",
        "",
        "Your strengths:",
        "- Rapidly finding files using glob patterns",
        "- Searching code and text with powerful regex patterns",
        "- Reading and analyzing file contents",
        "",
        "Guidelines:",
        "- Use Glob for broad file pattern matching",
        "- Use Grep for searching file contents with regex",
        "- Use Read when you know the specific file path you need to read",
        "- Use Bash ONLY for read-only operations (ls, git status, git log, git diff, find, cat, head, tail)",
        "- NEVER use Bash for: mkdir, touch, rm, cp, mv, git add, git commit, npm install, pip install, or any file creation/modification",
        "- Adapt your search approach based on the thoroughness level specified by the caller",
        "- Communicate your final report directly as a regular message - do NOT attempt to create files",
        "",
        "NOTE: You are meant to be a fast agent that returns output as quickly as possible. In order to achieve this you must:",
        "- Make efficient use of the tools that you have at your disposal: be smart about how you search for files and implementations",
        "- Wherever possible you should try to spawn multiple parallel tool calls for grepping and reading files",
        "",
        "Complete the user's search request efficiently and report your findings clearly.",
    ]
    .join("\n")
}

/// Load the profile directories: the two built-ins + user-level + project-level (a later same name overrides the earlier, keeping the first-occurrence slot).
pub fn load_agent_profiles(
    data_dir: &Path,
    workspace: &Path,
    provider: Option<&ProviderConfig>,
) -> (Vec<AgentProfile>, Vec<ProfileDiagnostic>) {
    let mut active = vec![builtin_general_purpose(), builtin_explore()];
    let mut diagnostics = Vec::new();
    let roots = [
        (data_dir.join("agents"), ProfileSource::User),
        (
            workspace.join(".zcode").join("agents"),
            ProfileSource::Project,
        ),
    ];
    for (root, source) in roots {
        for path in list_markdown_files(&root) {
            let content = match std::fs::read_to_string(&path) {
                Ok(content) => content,
                Err(error) => {
                    diagnostics.push(ProfileDiagnostic::new(
                        "agent_read_failed",
                        format!("Agent Markdown could not be read: {error}"),
                        Some(&path),
                    ));
                    continue;
                }
            };
            match parse_profile(&content, &path, source, provider) {
                Ok(profile) => match active.iter().position(|known| known.name == profile.name) {
                    Some(index) => active[index] = profile,
                    None => active.push(profile),
                },
                Err(diagnostic) => diagnostics.push(diagnostic),
            }
        }
    }
    (active, diagnostics)
}

fn list_markdown_files(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        })
        .collect();
    files.sort();
    files
}

fn parse_profile(
    content: &str,
    path: &Path,
    source: ProfileSource,
    provider: Option<&ProviderConfig>,
) -> Result<AgentProfile, ProfileDiagnostic> {
    let Some((frontmatter, body)) = split_frontmatter(content) else {
        return Err(ProfileDiagnostic::new(
            "agent_missing_frontmatter",
            "Agent Markdown must include frontmatter",
            Some(path),
        ));
    };
    let raw: serde_yaml::Value = serde_yaml::from_str(frontmatter).map_err(|error| {
        ProfileDiagnostic::new(
            "agent_invalid_frontmatter",
            format!("Agent frontmatter is not valid YAML: {error}"),
            Some(path),
        )
    })?;
    let value = serde_json::to_value(raw).map_err(|error| {
        ProfileDiagnostic::new(
            "agent_invalid_frontmatter",
            format!("Agent frontmatter could not be decoded: {error}"),
            Some(path),
        )
    })?;
    let Some(object) = value.as_object() else {
        return Err(ProfileDiagnostic::new(
            "agent_invalid_frontmatter",
            "Agent frontmatter must be a mapping",
            Some(path),
        ));
    };
    let unsupported: Vec<&str> = [
        "permissionMode",
        "memory",
        "skills",
        "mcpServers",
        "background",
    ]
    .into_iter()
    .filter(|key| object.contains_key(*key))
    .collect();
    if !unsupported.is_empty() {
        return Err(ProfileDiagnostic::new(
            "agent_unsupported_frontmatter",
            format!(
                "Agent frontmatter uses fields this host cannot honor: {}",
                unsupported.join(", ")
            ),
            Some(path),
        ));
    }
    // Unknown keys are ignored under TS's lenient parsing; known keys not listed here take no part either.
    let name = scalar_string(object.get("name")).ok_or_else(|| {
        ProfileDiagnostic::new(
            "agent_missing_required_frontmatter",
            "Agent frontmatter must include name",
            Some(path),
        )
    })?;
    let description = scalar_string(object.get("description"))
        .map(|description| description.replace("\\n", "\n"))
        .ok_or_else(|| {
            ProfileDiagnostic::new(
                "agent_missing_required_frontmatter",
                "Agent frontmatter must include description",
                Some(path),
            )
        })?;
    let tools = optional_tool_list(object.get("tools"));
    let disallowed_tools = optional_tool_list(object.get("disallowedTools")).unwrap_or_default();
    let thought_level = scalar_string(object.get("thoughtLevel"));
    let model = match object.get("model") {
        None | Some(Value::Null) => None,
        Some(Value::String(raw)) => {
            let raw = raw.trim();
            if raw.is_empty() || matches!(raw, "inherit" | "main" | "sonnet" | "opus" | "haiku") {
                None
            } else {
                let Some(config) = provider else {
                    return Err(ProfileDiagnostic::new(
                        "agent_unknown_model",
                        format!("Agent model {raw} cannot be resolved without a provider config"),
                        Some(path),
                    ));
                };
                let entries = support::catalog_from_provider(config);
                let requested = match &thought_level {
                    Some(level) if !level.is_empty() && !raw.contains('$') => {
                        format!("{raw}${level}")
                    }
                    _ => raw.to_string(),
                };
                Some(
                    support::resolve_model_reference(&requested, &entries).map_err(|error| {
                        ProfileDiagnostic::new("agent_unknown_model", error.message, Some(path))
                    })?,
                )
            }
        }
        Some(_) => {
            return Err(ProfileDiagnostic::new(
                "agent_invalid_frontmatter",
                "Agent frontmatter model must be a string",
                Some(path),
            ))
        }
    };
    let max_turns = optional_positive_integer(object.get("maxTurns"));
    let inject_agents_md = match object.get("injectAgentsMd") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(value)) => *value,
        Some(Value::String(value)) if value.eq_ignore_ascii_case("true") => true,
        Some(Value::String(value)) if value.eq_ignore_ascii_case("false") => false,
        Some(_) => {
            return Err(ProfileDiagnostic::new(
                "agent_invalid_frontmatter",
                "Agent frontmatter injectAgentsMd must be a boolean",
                Some(path),
            ))
        }
    };
    Ok(AgentProfile {
        name,
        description,
        source,
        system_prompt: body.trim().to_string(),
        tools,
        disallowed_tools,
        model,
        thought_level,
        max_turns,
        inject_agents_md,
        path: Some(path.to_string_lossy().to_string()),
    })
}

fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let rest = content.strip_prefix("---")?;
    if !rest.starts_with('\n') && !rest.starts_with("\r\n") {
        return None;
    }
    let rest = rest.trim_start_matches(['\r', '\n']);
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim() == "---" {
            let frontmatter = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return Some((frontmatter, body));
        }
        offset += line.len();
    }
    None
}

fn scalar_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// TS `parseToolList`: an array or a string; a string is split on whitespace/commas, but content inside parentheses is not split.
fn optional_tool_list(value: Option<&Value>) -> Option<Vec<String>> {
    let names = match value? {
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .filter(|item| !item.is_empty())
            .map(|item| tool_name_from_spec(item.trim()))
            .filter(|item| !item.is_empty())
            .collect::<Vec<String>>(),
        Value::String(raw) => split_tool_specs(raw)
            .into_iter()
            .map(|item| tool_name_from_spec(&item))
            .filter(|item| !item.is_empty())
            .collect(),
        _ => return None,
    };
    if names.is_empty() {
        // `tools: []` is an explicit "give it no tools at all", which differs from the default (all tools).
        value.and_then(Value::as_array).map(|_| Vec::new())
    } else {
        Some(names)
    }
}

fn split_tool_specs(raw: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for character in raw.chars() {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0 && (character == ',' || character.is_whitespace()) {
            if !current.trim().is_empty() {
                items.push(current.trim().to_string());
            }
            current.clear();
            continue;
        }
        current.push(character);
    }
    if !current.trim().is_empty() {
        items.push(current.trim().to_string());
    }
    items
}

fn tool_name_from_spec(value: &str) -> String {
    match value.find('(') {
        Some(index) => value[..index].trim().to_string(),
        None => value.trim().to_string(),
    }
}

fn optional_positive_integer(value: Option<&Value>) -> Option<usize> {
    match value? {
        Value::Number(number) => number
            .as_u64()
            .filter(|value| *value > 0)
            .map(|value| value as usize),
        Value::String(raw) => raw.trim().parse::<usize>().ok().filter(|value| *value > 0),
        _ => None,
    }
}

/// The canonical model form produced from frontmatter → (provider, model, reasoning level).
pub fn split_canonical_model(canonical: &str) -> (Option<String>, String, Option<String>) {
    let (reference, level) = match canonical.rsplit_once('$') {
        Some((reference, level)) if !level.is_empty() => (reference, Some(level.to_string())),
        _ => (canonical, None),
    };
    match reference.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
            (Some(provider.to_string()), model.to_string(), level)
        }
        _ => (None, reference.to_string(), level),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> ProviderConfig {
        ProviderConfig {
            provider_id: None,
            kind: "anthropic".to_string(),
            base_url: "http://127.0.0.1:1".to_string(),
            api_key: "test-key-not-a-real-credential".to_string(),
            model: "main-model".to_string(),
            max_tokens: 1024,
            auth_style: None,
            stream: true,
            extra_models: vec!["fast-model".to_string()],
            reasoning_levels: vec!["low".to_string(), "high".to_string()],
            headers: Vec::new(),
        }
    }

    fn workspace(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zcode-profiles-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("workspace");
        dir
    }

    #[test]
    fn builtins_are_present_and_explore_is_read_only() {
        let workspace = workspace("builtin");
        let (profiles, diagnostics) =
            load_agent_profiles(&workspace.join("data"), &workspace, None);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(profiles[0].name, GENERAL_PURPOSE_AGENT);
        assert_eq!(profiles[1].name, EXPLORE_AGENT);
        let explore = &profiles[1];
        assert!(explore.is_builtin_explore());
        assert!(!explore.inject_agents_md);
        let allowed = explore.tool_allowlist().expect("explore allowlist");
        assert!(allowed.contains(&"Read".to_string()));
        assert!(!allowed.contains(&"Write".to_string()));
        assert!(!allowed.contains(&"Edit".to_string()));
        assert!(explore.system_prompt.contains("READ-ONLY"));
    }

    #[test]
    fn project_and_user_markdown_override_by_name() {
        let workspace = workspace("override");
        let data_dir = workspace.join("data");
        let user_dir = data_dir.join("agents");
        let project_dir = workspace.join(".zcode").join("agents");
        std::fs::create_dir_all(&user_dir).expect("user dir");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::write(
            user_dir.join("reviewer.md"),
            "---\nname: reviewer\ndescription: reviews code\ntools: Read, Grep\nmaxTurns: 7\n---\nYou review code.\n",
        )
        .expect("user profile");
        std::fs::write(
            project_dir.join("reviewer.md"),
            "---\nname: reviewer\ndescription: project reviewer\ndisallowedTools: [Bash]\ninjectAgentsMd: false\n---\nProject rules.\n",
        )
        .expect("project profile");
        let (profiles, diagnostics) = load_agent_profiles(&data_dir, &workspace, Some(&provider()));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let reviewer = profiles
            .iter()
            .find(|profile| profile.name == "reviewer")
            .expect("reviewer loaded");
        assert_eq!(reviewer.source, ProfileSource::Project);
        assert_eq!(reviewer.description, "project reviewer");
        assert_eq!(reviewer.system_prompt, "Project rules.");
        assert!(!reviewer.inject_agents_md);
        assert!(reviewer.disallowed_tools.contains(&"Bash".to_string()));
        assert_eq!(reviewer.max_turns, None, "later file wins wholesale");
        assert!(reviewer
            .tool_disallowlist()
            .contains(&"ExitPlanMode".to_string()));
    }

    #[test]
    fn model_is_resolved_against_the_static_catalog() {
        let workspace = workspace("model");
        let project_dir = workspace.join(".zcode").join("agents");
        std::fs::create_dir_all(&project_dir).expect("agents dir");
        std::fs::write(
            project_dir.join("fast.md"),
            "---\nname: fast\ndescription: quick jobs\nmodel: fast-model\nthoughtLevel: high\n---\nBe quick.\n",
        )
        .expect("profile");
        let (profiles, diagnostics) =
            load_agent_profiles(&workspace.join("data"), &workspace, Some(&provider()));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let fast = profiles
            .iter()
            .find(|profile| profile.name == "fast")
            .expect("fast loaded");
        assert_eq!(fast.model.as_deref(), Some("anthropic/fast-model$high"));
        assert_eq!(
            split_canonical_model(fast.model.as_deref().unwrap()),
            (
                Some("anthropic".to_string()),
                "fast-model".to_string(),
                Some("high".to_string())
            )
        );
    }

    #[test]
    fn unsupported_frontmatter_is_named_and_the_profile_is_not_loaded() {
        let workspace = workspace("unsupported");
        let project_dir = workspace.join(".zcode").join("agents");
        std::fs::create_dir_all(&project_dir).expect("agents dir");
        std::fs::write(
            project_dir.join("skills.md"),
            "---\nname: skilled\ndescription: uses skills\nskills: [pdf]\n---\nBody.\n",
        )
        .expect("profile");
        let (profiles, diagnostics) =
            load_agent_profiles(&workspace.join("data"), &workspace, Some(&provider()));
        assert!(
            !profiles.iter().any(|profile| profile.name == "skilled"),
            "a profile with a capability we cannot honor must not load"
        );
        let diagnostic = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code == "agent_unsupported_frontmatter")
            .expect("unsupported diagnostic");
        assert!(diagnostic.message.contains("skills"), "{diagnostic:?}");
    }

    #[test]
    fn missing_frontmatter_and_bad_models_are_diagnostics() {
        let workspace = workspace("bad");
        let project_dir = workspace.join(".zcode").join("agents");
        std::fs::create_dir_all(&project_dir).expect("agents dir");
        std::fs::write(project_dir.join("plain.md"), "no frontmatter here\n").expect("plain");
        std::fs::write(
            project_dir.join("ghost.md"),
            "---\nname: ghost\ndescription: ghost model\nmodel: missing-model\n---\nBody.\n",
        )
        .expect("ghost");
        let (profiles, diagnostics) =
            load_agent_profiles(&workspace.join("data"), &workspace, Some(&provider()));
        assert_eq!(profiles.len(), 2, "only the built-ins remain");
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "agent_missing_frontmatter"));
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "agent_unknown_model"));
    }
}
