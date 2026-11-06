//! Workspace file-search ignore rules (`.zcodeignore`).
//!
//! Mirrors the TS file layout and template/transform semantics. Matching implements
//! the gitignore subset the product relies on: last-rule-wins, `!` negation, directory
//! suffix, anchoring, `*`/`?`/`**`. A missing `.zcodeignore` is created from
//! `.gitignore` + the built-in default section; later `.gitignore` edits do not sync
//! implicitly.

use std::path::Path;

use nc_code_service_core::atomic::atomic_write_text;
use nc_code_service_core::ServiceError;

pub const IGNORE_FILE_NAME: &str = ".zcodeignore";
const GITIGNORE_FILE_NAME: &str = ".gitignore";
const SYNC_MARKER: &str =
    "# ===== ↑ The above is synchronized from .gitignore (Synchronize from .gitignore only rewrites the above part) =====";
const DEFAULTS_MARKER: &str =
    "# ----- ↑ The above are ZCode's default exclusion rules (please write custom rules below this line, they will not be synchronized/restored) -----";
const CUSTOM_HINT: &str =
    "# Custom rules are written below (the prompt in this line can be deleted)";

const BUILTIN_IGNORE_LINES: &[&str] = &[
    ".git/",
    ".hg/",
    ".svn/",
    "node_modules/",
    "bower_components/",
    "jspm_packages/",
    "__pycache__/",
    "site-packages/",
    "venv/",
    "coverage/",
    "htmlcov/",
    "lcov-report/",
    "cmakefiles/",
    "cmake-build-*/",
    "bazel-*/",
    "pods/",
    "deriveddata/",
    "storybook-static/",
    "playwright-report/",
    "test-results/",
    "allure-results/",
    "allure-report/",
    "cdk.out/",
    "*.egg-info/",
    "*.dist-info/",
    "eggs/",
    "pip-wheel-metadata/",
    "wheels/",
];

const TEMPLATE_HEADER: &[&str] = &[
    "# ZCode workspace file search ignore rules (.zcodeignore)",
    "# The syntax is consistent with .gitignore, and only affects ZCode's @ file candidate / Command Center / file tree search,",
    "# Does not affect file tree browsing, uploading, or Agent file access. ",
    "# Modifications to .gitignore will not be automatically synchronized to this file; you can Synchronize from .gitignore on the settings page. ",
    "",
];

#[derive(Debug, Clone)]
struct Rule {
    negated: bool,
    dir_only: bool,
    anchored: bool,
    pattern: String,
}

#[derive(Debug, Clone)]
pub struct IgnoreRules {
    rules: Vec<Rule>,
}

impl IgnoreRules {
    pub fn parse(content: &str) -> Self {
        let mut rules = Vec::new();
        for raw in content.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (negated, rest) = match line.strip_prefix('!') {
                Some(rest) => (true, rest.trim()),
                None => (false, line),
            };
            if rest.is_empty() {
                continue;
            }
            let dir_only = rest.ends_with('/');
            let trimmed = rest.trim_end_matches('/');
            let anchored = trimmed.starts_with('/');
            let pattern = trimmed.trim_start_matches('/').to_string();
            if pattern.is_empty() {
                continue;
            }
            rules.push(Rule {
                negated,
                dir_only,
                anchored,
                pattern,
            });
        }
        Self { rules }
    }

    pub fn is_ignored(&self, relative_path: &str, is_dir: bool) -> bool {
        let mut ignored = false;
        for rule in &self.rules {
            if rule.matches(relative_path, is_dir) {
                ignored = !rule.negated;
            }
        }
        ignored
    }
}

impl Rule {
    fn matches(&self, relative_path: &str, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        if self.anchored || self.pattern.contains('/') {
            return glob_match(&self.pattern, relative_path);
        }
        relative_path
            .split('/')
            .any(|segment| glob_match(&self.pattern, segment))
    }
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    fn walk(pattern: &[char], text: &[char]) -> bool {
        if pattern.is_empty() {
            return text.is_empty();
        }
        match pattern[0] {
            '*' => {
                if pattern.get(1) == Some(&'*') {
                    let rest = &pattern[2..];
                    let rest = rest.strip_prefix(&['/']).unwrap_or(rest);
                    if walk(rest, text) {
                        return true;
                    }
                    for index in 0..=text.len() {
                        if walk(rest, &text[index..]) {
                            return true;
                        }
                    }
                    false
                } else {
                    let rest = &pattern[1..];
                    for index in 0..=text.len() {
                        if walk(rest, &text[index..]) {
                            return true;
                        }
                        if index < text.len() && text[index] == '/' {
                            break;
                        }
                    }
                    false
                }
            }
            '?' => !text.is_empty() && text[0] != '/' && walk(&pattern[1..], &text[1..]),
            character => {
                !text.is_empty() && text[0] == character && walk(&pattern[1..], &text[1..])
            }
        }
    }
    walk(&pattern, &text)
}

pub async fn load_rules(root: &Path) -> Result<IgnoreRules, ServiceError> {
    let file = root.join(IGNORE_FILE_NAME);
    match tokio::fs::read_to_string(&file).await {
        Ok(content) => Ok(IgnoreRules::parse(&content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let content = build_template(read_gitignore(root).await.as_deref());
            let _ = atomic_write_text(&file, &content).await;
            Ok(IgnoreRules::parse(&content))
        }
        Err(error) => Err(ServiceError::new(format!(
            "read .zcodeignore failed: {error}"
        ))),
    }
}

pub async fn read_content(root: &Path) -> Result<serde_json::Value, ServiceError> {
    match tokio::fs::read_to_string(root.join(IGNORE_FILE_NAME)).await {
        Ok(content) => Ok(serde_json::json!({"content": content, "source": "file"})),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let gitignore = read_gitignore(root).await;
            Ok(serde_json::json!({
                "content": build_template(gitignore.as_deref()),
                "source": "template",
            }))
        }
        Err(error) => Err(ServiceError::new(format!(
            "read .zcodeignore failed: {error}"
        ))),
    }
}

pub async fn transform(root: &Path, transform: &str) -> Result<serde_json::Value, ServiceError> {
    let current = match tokio::fs::read_to_string(root.join(IGNORE_FILE_NAME)).await {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            build_template(read_gitignore(root).await.as_deref())
        }
        Err(error) => {
            return Err(ServiceError::new(format!(
                "read .zcodeignore failed: {error}"
            )))
        }
    };
    let gitignore = read_gitignore(root).await;
    let next = match transform {
        "sync-gitignore" => sync_from_gitignore(&current, gitignore.as_deref()),
        "reset-defaults" => reset_defaults(&current, gitignore.as_deref()),
        other => {
            return Err(ServiceError::invalid_params(format!(
                "unknown .zcodeignore transform: {other}"
            )))
        }
    };
    Ok(serde_json::json!({ "content": next }))
}

pub async fn write(root: &Path, content: &str) -> Result<(), ServiceError> {
    let file = root.join(IGNORE_FILE_NAME);
    atomic_write_text(&file, content)
        .await
        .map_err(|error| ServiceError::new(format!("write .zcodeignore failed: {error}")))
}

pub async fn fingerprint(root: &Path) -> String {
    match tokio::fs::metadata(root.join(IGNORE_FILE_NAME)).await {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis())
                .unwrap_or(0);
            format!("{modified}:{}", metadata.len())
        }
        Err(_) => "none".to_string(),
    }
}

async fn read_gitignore(root: &Path) -> Option<String> {
    tokio::fs::read_to_string(root.join(GITIGNORE_FILE_NAME))
        .await
        .ok()
}

fn build_template(gitignore: Option<&str>) -> String {
    let gitignore_section = match gitignore {
        Some(content) if !content.trim().is_empty() => {
            if content.ends_with('\n') {
                content.to_string()
            } else {
                format!("{content}\n")
            }
        }
        _ => format!("{}\n", TEMPLATE_HEADER.join("\n")),
    };
    let defaults = build_defaults_section(gitignore);
    [
        gitignore_section,
        SYNC_MARKER.to_string(),
        defaults,
        DEFAULTS_MARKER.to_string(),
        CUSTOM_HINT.to_string(),
        String::new(),
    ]
    .join("\n")
}

fn build_defaults_section(gitignore: Option<&str>) -> String {
    let Some(gitignore) = gitignore else {
        return BUILTIN_IGNORE_LINES.join("\n");
    };
    let mut declared = std::collections::HashSet::new();
    for raw in gitignore.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        declared.insert(line.to_string());
        declared.insert(line.trim_end_matches('/').to_string());
    }
    BUILTIN_IGNORE_LINES
        .iter()
        .filter(|line| {
            let bare = line.trim_end_matches('/');
            !declared.contains(**line) && !declared.contains(bare)
        })
        .map(|line| (*line).to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

struct Sections {
    defaults: String,
    custom: String,
}

fn split_sections(content: &str) -> Option<Sections> {
    let lines: Vec<&str> = content.lines().collect();
    let sync_index = lines.iter().position(|line| line.trim() == SYNC_MARKER)?;
    let defaults_index = lines
        .iter()
        .position(|line| line.trim() == DEFAULTS_MARKER)?;
    if defaults_index <= sync_index {
        return None;
    }
    Some(Sections {
        defaults: lines[sync_index + 1..defaults_index]
            .join("\n")
            .trim()
            .to_string(),
        custom: lines[defaults_index + 1..]
            .join("\n")
            .trim_start_matches('\n')
            .to_string(),
    })
}

fn sync_from_gitignore(current: &str, gitignore: Option<&str>) -> String {
    let Some(sections) = split_sections(current) else {
        return build_template(gitignore);
    };
    let gitignore_section = match gitignore {
        Some(content) if !content.trim().is_empty() => {
            if content.ends_with('\n') {
                content.to_string()
            } else {
                format!("{content}\n")
            }
        }
        _ => format!("{}\n", TEMPLATE_HEADER.join("\n")),
    };
    format!(
        "{gitignore_section}\n{SYNC_MARKER}\n{}\n{DEFAULTS_MARKER}\n{}",
        sections.defaults, sections.custom
    )
    .trim_end_matches('\n')
    .to_string()
        + "\n"
}

fn reset_defaults(current: &str, gitignore: Option<&str>) -> String {
    let Some(sections) = split_sections(current) else {
        return build_template(gitignore);
    };
    let lines: Vec<&str> = current.lines().collect();
    let sync_index = lines
        .iter()
        .position(|line| line.trim() == SYNC_MARKER)
        .unwrap_or(0);
    let gitignore_section = lines[..sync_index].join("\n");
    format!(
        "{gitignore_section}\n{SYNC_MARKER}\n{}\n{DEFAULTS_MARKER}\n{}",
        build_defaults_section(gitignore),
        sections.custom
    )
    .trim_end_matches('\n')
    .to_string()
        + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_builtin_and_negation_rules() {
        let rules = IgnoreRules::parse("node_modules/\n*.log\n!keep.log\nbuild/**\n/top.txt\n");
        assert!(rules.is_ignored("node_modules", true));
        assert!(!rules.is_ignored("node_modules", false));
        assert!(rules.is_ignored("a/b/debug.log", false));
        assert!(!rules.is_ignored("a/keep.log", false));
        assert!(rules.is_ignored("build/a/b.js", false));
        assert!(rules.is_ignored("top.txt", false));
        assert!(!rules.is_ignored("nested/top.txt", false));
    }

    #[test]
    fn template_has_markers_and_default_section() {
        let template = build_template(None);
        assert!(template.contains(SYNC_MARKER));
        assert!(template.contains(DEFAULTS_MARKER));
        assert!(template.contains("node_modules/"));
        let with_gitignore = build_template(Some("dist/\n"));
        assert!(with_gitignore.starts_with("dist/\n"));
        assert!(!with_gitignore.contains("\nnode_modules/\nnode_modules"));
    }

    #[test]
    fn sync_keeps_defaults_and_custom_sections() {
        let current = build_template(Some("dist/\n"));
        let synced = sync_from_gitignore(&current, Some("out/\n"));
        assert!(synced.starts_with("out/\n"));
        assert!(synced.contains("node_modules/"));
        assert!(synced.contains(CUSTOM_HINT));
    }
}
