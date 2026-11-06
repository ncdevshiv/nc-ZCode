//! Remote-target discovery for the Tauri shell.
//!
//! These commands mirror the Electron main-process discovery half of the
//! remote-connection IPC (`isDockerAvailable`, `listWSLDistros`,
//! `listDockerContainers`, `listSSHConfigAliases`). Establishing the remote
//! session itself still lives in the TypeScript host, so the shell only reports
//! what is actually installed on this machine and never fabricates targets.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use tokio::process::Command;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_SSH_ALIASES: usize = 200;
const MAX_INCLUDE_DEPTH: usize = 8;

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DockerContainerInfo {
    pub id: String,
    pub image: String,
    pub name: String,
    pub state: String,
    pub status: String,
}

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WslDistro {
    pub name: String,
    pub is_default: bool,
    pub state: String,
    pub version: Option<u8>,
}

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SshConfigAliasOption {
    pub alias: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_key_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

async fn run_bytes_with_timeout(program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let mut command = Command::new(program);
    command.args(args).kill_on_drop(true);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.as_std_mut().creation_flags(CREATE_NO_WINDOW);
    }
    let output = tokio::time::timeout(COMMAND_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout)
}

async fn run_text_with_timeout(program: &str, args: &[&str]) -> Option<String> {
    run_bytes_with_timeout(program, args)
        .await
        .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
}

/// `IPlatformService.isDockerAvailable`.
pub async fn is_docker_available() -> bool {
    run_text_with_timeout("docker", &["info", "--format", "{{.ServerVersion}}"])
        .await
        .is_some_and(|value| !value.trim().is_empty())
}

/// `IPlatformService.listDockerContainers`.
pub async fn list_docker_containers() -> Vec<DockerContainerInfo> {
    let Some(stdout) = run_text_with_timeout("docker", &["ps", "--format", "{{json .}}"]).await
    else {
        return Vec::new();
    };
    parse_docker_ps(&stdout)
}

fn parse_docker_ps(stdout: &str) -> Vec<DockerContainerInfo> {
    let mut containers = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let field = |name: &str| {
            value
                .get(name)
                .and_then(|item| item.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let container = DockerContainerInfo {
            id: field("ID"),
            image: field("Image"),
            name: field("Names"),
            state: field("State"),
            status: field("Status"),
        };
        if !container.id.is_empty() || !container.name.is_empty() {
            containers.push(container);
        }
    }
    containers
}

/// `IPlatformService.listWSLDistros`.
pub async fn list_wsl_distros() -> Vec<WslDistro> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let Some(bytes) = run_bytes_with_timeout("wsl.exe", &["-l", "-v"]).await else {
        return Vec::new();
    };
    parse_wsl_list(&decode_utf16le(&bytes))
}

fn decode_utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    String::from_utf16_lossy(&units)
}

fn parse_wsl_list(output: &str) -> Vec<WslDistro> {
    let mut distros = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim_start_matches(['\u{feff}', ' ', '\t', '\0']);
        if trimmed.is_empty() {
            continue;
        }
        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }
        if tokens
            .iter()
            .any(|token| token.eq_ignore_ascii_case("NAME"))
            || trimmed.eq_ignore_ascii_case("windows subsystem for linux")
        {
            continue;
        }
        let mut is_default = false;
        let mut rest = &tokens[..];
        if rest.first().is_some_and(|token| *token == "*") {
            is_default = true;
            rest = &rest[1..];
        }
        if rest.len() < 3 {
            continue;
        }
        let version = rest.last().and_then(|token| token.parse::<u8>().ok());
        let state = rest[rest.len() - 2].to_string();
        let name = rest[..rest.len() - 2].join(" ");
        if name.is_empty() {
            continue;
        }
        distros.push(WslDistro {
            name,
            is_default,
            state,
            version,
        });
    }
    distros
}

/// `IPlatformService.listSSHConfigAliases`.
pub fn list_ssh_config_aliases() -> Vec<SshConfigAliasOption> {
    let Some(home) = home_directory() else {
        return Vec::new();
    };
    let config = home.join(".ssh").join("config");
    if !config.is_file() {
        return Vec::new();
    }
    let mut blocks = Vec::new();
    let mut visited = Vec::new();
    collect_ssh_blocks(&config, 0, &mut visited, &mut blocks);
    build_ssh_aliases(&blocks)
}

fn home_directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

#[derive(Debug, Clone)]
struct SshBlock {
    patterns: Vec<String>,
    directives: Vec<(String, String)>,
    source: String,
    from_host: bool,
}

fn collect_ssh_blocks(
    path: &Path,
    depth: usize,
    visited: &mut Vec<PathBuf>,
    blocks: &mut Vec<SshBlock>,
) {
    if depth > MAX_INCLUDE_DEPTH || visited.iter().any(|seen| seen == path) {
        return;
    }
    visited.push(path.to_path_buf());
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let source = path.to_string_lossy().to_string();
    let mut current = SshBlock {
        patterns: vec!["*".to_string()],
        directives: Vec::new(),
        source: source.clone(),
        from_host: false,
    };
    blocks.push(current.clone());
    let mut current_index = blocks.len() - 1;
    for raw_line in content.lines() {
        let stripped = strip_inline_comment(raw_line);
        let trimmed = stripped.trim();
        if trimmed.is_empty() {
            continue;
        }
        let tokens = split_ssh_tokens(trimmed);
        let Some(key) = tokens.first() else {
            continue;
        };
        let key = key.to_ascii_lowercase();
        if key == "include" {
            let base = path.parent().unwrap_or(Path::new("."));
            let mut included = Vec::new();
            for token in &tokens[1..] {
                for target in resolve_include_targets(token, base) {
                    collect_ssh_blocks(&target, depth + 1, visited, &mut included);
                }
            }
            // SSH processes Include in place; keep the current block and append
            // the included blocks after it so later directives still target it.
            for (offset, block) in included.into_iter().enumerate() {
                blocks.insert(current_index + 1 + offset, block);
            }
            continue;
        }
        if key == "host" {
            current = SshBlock {
                patterns: tokens[1..].to_vec(),
                directives: Vec::new(),
                source: source.clone(),
                from_host: true,
            };
            blocks.push(current.clone());
            current_index = blocks.len() - 1;
            continue;
        }
        if tokens.len() < 2 {
            continue;
        }
        let directive = (key, tokens[1..].join(" "));
        blocks[current_index].directives.push(directive);
        current.directives = blocks[current_index].directives.clone();
    }
}

fn resolve_include_targets(token: &str, base: &Path) -> Vec<PathBuf> {
    let expanded = expand_home(token.trim());
    if expanded.is_empty() {
        return Vec::new();
    }
    let candidate = if Path::new(&expanded).is_absolute() {
        PathBuf::from(&expanded)
    } else {
        base.join(&expanded)
    };
    let file_name = candidate
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    if !file_name.contains('*') && !file_name.contains('?') {
        return if candidate.is_file() {
            vec![candidate]
        } else {
            Vec::new()
        };
    }
    let Some(parent) = candidate.parent() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut matches: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| {
            path.file_name()
                .map(|name| wildcard_match(&file_name, &name.to_string_lossy()))
                .unwrap_or(false)
        })
        .collect();
    matches.sort();
    matches
}

fn expand_home(value: &str) -> String {
    let Some(home) = home_directory() else {
        return value.to_string();
    };
    let home = home.to_string_lossy().to_string();
    if value == "~" {
        return home;
    }
    if let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        return format!("{home}/{rest}");
    }
    value.to_string()
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let mut dp = vec![vec![false; text.len() + 1]; pattern.len() + 1];
    dp[0][0] = true;
    for index in 1..=pattern.len() {
        if pattern[index - 1] == '*' {
            dp[index][0] = dp[index - 1][0];
        }
    }
    for index in 1..=pattern.len() {
        for position in 1..=text.len() {
            dp[index][position] = match pattern[index - 1] {
                '*' => dp[index - 1][position] || dp[index][position - 1],
                '?' => dp[index - 1][position - 1],
                literal => literal == text[position - 1] && dp[index - 1][position - 1],
            };
        }
    }
    dp[pattern.len()][text.len()]
}

fn strip_inline_comment(line: &str) -> String {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '\'' | '"' => {
                if quote == Some(character) {
                    quote = None;
                } else if quote.is_none() {
                    quote = Some(character);
                }
            }
            '#' if quote.is_none() => return line[..index].to_string(),
            _ => {}
        }
    }
    line.to_string()
}

fn split_ssh_tokens(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut characters = line.chars().peekable();
    while let Some(character) = characters.next() {
        if let Some(active) = quote {
            if character == '\\' {
                if let Some(next) = characters.peek().copied() {
                    if next == active || next == '\\' {
                        current.push(next);
                        characters.next();
                        continue;
                    }
                }
                current.push('\\');
                continue;
            }
            if character == active {
                quote = None;
            } else {
                current.push(character);
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            value if value.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            '\\' => {
                if let Some(next) = characters.peek().copied() {
                    if next.is_whitespace()
                        || next == '\\'
                        || next == '\''
                        || next == '"'
                        || next == '#'
                    {
                        current.push(next);
                        characters.next();
                        continue;
                    }
                }
                current.push('\\');
            }
            value => current.push(value),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn is_connectable_alias(pattern: &str) -> bool {
    let trimmed = pattern.trim();
    !trimmed.is_empty()
        && trimmed != "*"
        && !trimmed.starts_with('!')
        && !trimmed.contains(['?', '*', '[', '\\', ']'])
}

fn host_pattern_matches(pattern: &str, alias: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains(['*', '?']) {
        return pattern == alias;
    }
    wildcard_match(pattern, alias)
}

fn block_matches_alias(patterns: &[String], alias: &str) -> bool {
    let mut matched = false;
    for raw in patterns {
        let pattern = raw.trim();
        if pattern.is_empty() {
            continue;
        }
        if let Some(negated) = pattern.strip_prefix('!') {
            if !negated.is_empty() && host_pattern_matches(negated, alias) {
                return false;
            }
            continue;
        }
        if host_pattern_matches(pattern, alias) {
            matched = true;
        }
    }
    matched
}

fn build_ssh_aliases(blocks: &[SshBlock]) -> Vec<SshConfigAliasOption> {
    let mut aliases: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for block in blocks {
        if !block.from_host || block.patterns.len() != 1 {
            continue;
        }
        let alias = block.patterns[0].trim();
        if !is_connectable_alias(alias) || !seen.insert(alias.to_string()) {
            continue;
        }
        aliases.push((alias.to_string(), block.source.clone()));
        if aliases.len() >= MAX_SSH_ALIASES {
            break;
        }
    }
    aliases
        .into_iter()
        .map(|(alias, source)| {
            let mut host = None;
            let mut port = None;
            let mut username = None;
            let mut private_key_path = None;
            for block in blocks {
                if !block_matches_alias(&block.patterns, &alias) {
                    continue;
                }
                for (key, value) in &block.directives {
                    match key.as_str() {
                        "hostname" if host.is_none() => {
                            let value = value.trim();
                            if !value.is_empty() {
                                host = Some(value.to_string());
                            }
                        }
                        "port" if port.is_none() => {
                            port = value.trim().parse::<u16>().ok().filter(|port| *port > 0);
                        }
                        "user" if username.is_none() => {
                            let value = value.trim();
                            if !value.is_empty() {
                                username = Some(value.to_string());
                            }
                        }
                        "identityfile" if private_key_path.is_none() => {
                            let value = expand_home(value.trim());
                            if !value.is_empty() {
                                private_key_path = Some(value);
                            }
                        }
                        _ => {}
                    }
                }
            }
            SshConfigAliasOption {
                host: Some(host.unwrap_or_else(|| alias.clone())),
                alias,
                port,
                username,
                private_key_path,
                source: Some(source),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_docker_ps_json_lines() {
        let stdout = concat!(
            "{\"ID\":\"abc123\",\"Image\":\"ubuntu:24.04\",\"Names\":\"dev\",\"State\":\"running\",\"Status\":\"Up 2 hours\"}\n",
            "{\"ID\":\"def456\",\"Image\":\"node:24\",\"Names\":\"build\",\"State\":\"exited\",\"Status\":\"Exited (0) 3 hours ago\"}\n",
            "not json\n"
        );
        let containers = parse_docker_ps(stdout);
        assert_eq!(containers.len(), 2);
        assert_eq!(containers[0].id, "abc123");
        assert_eq!(containers[0].name, "dev");
        assert_eq!(containers[1].state, "exited");
    }

    #[test]
    fn parses_wsl_list_table() {
        let output = "  NAME            STATE           VERSION\n* Ubuntu          Running         2\n  Debian          Stopped         1\n";
        let distros = parse_wsl_list(output);
        assert_eq!(distros.len(), 2);
        assert_eq!(distros[0].name, "Ubuntu");
        assert!(distros[0].is_default);
        assert_eq!(distros[0].state, "Running");
        assert_eq!(distros[0].version, Some(2));
        assert!(!distros[1].is_default);
        assert_eq!(distros[1].version, Some(1));
    }

    #[test]
    fn parses_ssh_aliases_and_directives() {
        let blocks = vec![
            SshBlock {
                patterns: vec!["dev".to_string()],
                directives: vec![
                    ("hostname".to_string(), "dev.example.com".to_string()),
                    ("port".to_string(), "2222".to_string()),
                    ("user".to_string(), "alice".to_string()),
                    ("identityfile".to_string(), "~/.ssh/dev".to_string()),
                ],
                source: "config".to_string(),
                from_host: true,
            },
            SshBlock {
                patterns: vec!["*.internal".to_string()],
                directives: vec![("user".to_string(), "bob".to_string())],
                source: "config".to_string(),
                from_host: true,
            },
            SshBlock {
                patterns: vec!["prod".to_string()],
                directives: vec![],
                source: "config".to_string(),
                from_host: true,
            },
            SshBlock {
                patterns: vec!["*".to_string()],
                directives: vec![("user".to_string(), "global".to_string())],
                source: "config".to_string(),
                from_host: false,
            },
        ];
        let aliases = build_ssh_aliases(&blocks);
        assert_eq!(aliases.len(), 2);
        let dev = aliases.iter().find(|item| item.alias == "dev").unwrap();
        assert_eq!(dev.host.as_deref(), Some("dev.example.com"));
        assert_eq!(dev.port, Some(2222));
        assert_eq!(dev.username.as_deref(), Some("alice"));
        assert!(dev.private_key_path.is_some());
        let prod = aliases.iter().find(|item| item.alias == "prod").unwrap();
        assert_eq!(prod.host.as_deref(), Some("prod"));
        assert_eq!(prod.username.as_deref(), Some("global"));
    }

    #[test]
    fn tokenizer_keeps_windows_paths_and_quotes() {
        assert_eq!(
            split_ssh_tokens(r#"IdentityFile "C:\Users\me\.ssh\id_ed25519""#),
            vec![
                "IdentityFile".to_string(),
                r"C:\Users\me\.ssh\id_ed25519".to_string()
            ]
        );
        assert_eq!(
            split_ssh_tokens(&strip_inline_comment("Host dev # comment")),
            vec!["Host".to_string(), "dev".to_string()]
        );
        assert_eq!(
            strip_inline_comment(r#"IdentityFile "C:\path with # hash""#),
            r#"IdentityFile "C:\path with # hash""#
        );
    }

    #[test]
    fn wildcard_matching_covers_glob_includes() {
        assert!(wildcard_match("*.conf", "work.conf"));
        assert!(!wildcard_match("*.conf", "work.cfg"));
        assert!(wildcard_match("host-?", "host-1"));
    }

    #[test]
    fn decodes_utf16le_wsl_output() {
        let bytes: Vec<u8> = "  NAME   STATE   VERSION\r\n* Ubuntu Running 2\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let distros = parse_wsl_list(&decode_utf16le(&bytes));
        assert_eq!(distros.len(), 1);
        assert_eq!(distros[0].name, "Ubuntu");
    }
}
