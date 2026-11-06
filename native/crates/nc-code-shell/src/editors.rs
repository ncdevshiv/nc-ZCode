//! Installed editor discovery and `openInEditor` for the Tauri shell.
//!
//! The Electron main process discovered VS Code/Cursor/Zed/Sublime and launched
//! them with `file:line` or `vscode-remote://` URIs. This port keeps the same
//! local behavior on Windows/macOS/Linux and supports VS Code Remote-SSH/WSL
//! folder/file URIs. Docker remote targets return an explicit unsupported error.
//!
//! Icons are not extracted from the executables yet; the renderer receives a
//! valid transparent PNG placeholder so `<img>` rendering stays intact.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

const TRANSPARENT_PNG: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl2n1cAAAAASUVORK5CYII=";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorInfo {
    pub id: String,
    pub name: String,
    pub icon_data_url: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OpenInEditorOptions {
    #[serde(default)]
    pub remote_target: Option<RemoteTarget>,
    #[serde(default)]
    pub workspace_identity: Option<String>,
    #[serde(default)]
    pub path_kind: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RemoteTarget {
    Ssh {
        host: String,
        #[serde(default)]
        port: Option<u16>,
        username: String,
        #[serde(default)]
        ssh_config_alias: Option<String>,
    },
    Wsl {
        #[serde(default)]
        distro: Option<String>,
        #[serde(default)]
        user: Option<String>,
    },
    Docker {
        container: String,
    },
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OpenInEditorResult {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl OpenInEditorResult {
    fn failure(error: impl Into<String>) -> Self {
        Self {
            success: false,
            error: Some(error.into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// Variants are constructed only on their matching platform (Windows uses
// AppPath, macOS MacApp, Linux Command); the launcher still matches all three.
#[allow(dead_code)]
enum EditorKind {
    AppPath,
    Command,
    MacApp,
}

#[derive(Debug, Clone)]
struct EditorDef {
    id: &'static str,
    name: &'static str,
    kind: EditorKind,
    candidates: Vec<String>,
}

fn editor_defs() -> Vec<EditorDef> {
    #[cfg(windows)]
    {
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
        let program_files = std::env::var("ProgramFiles").unwrap_or_default();
        vec![
            EditorDef {
                id: "vscode",
                name: "VS Code",
                kind: EditorKind::AppPath,
                candidates: vec![
                    format!("{local}\\Programs\\Microsoft VS Code\\Code.exe"),
                    format!("{program_files}\\Microsoft VS Code\\Code.exe"),
                    "code.cmd".to_string(),
                    "code.exe".to_string(),
                ],
            },
            EditorDef {
                id: "vscode-insiders",
                name: "VS Code Insiders",
                kind: EditorKind::AppPath,
                candidates: vec![
                    format!("{local}\\Programs\\Microsoft VS Code Insiders\\Code - Insiders.exe"),
                    format!("{program_files}\\Microsoft VS Code Insiders\\Code - Insiders.exe"),
                    "code-insiders.cmd".to_string(),
                ],
            },
            EditorDef {
                id: "cursor",
                name: "Cursor",
                kind: EditorKind::AppPath,
                candidates: vec![
                    format!("{local}\\Programs\\cursor\\Cursor.exe"),
                    "cursor.cmd".to_string(),
                    "cursor.exe".to_string(),
                ],
            },
            EditorDef {
                id: "zed",
                name: "Zed",
                kind: EditorKind::AppPath,
                candidates: vec![
                    format!("{local}\\Programs\\Zed\\Zed.exe"),
                    "zed.exe".to_string(),
                ],
            },
            EditorDef {
                id: "sublime",
                name: "Sublime Text",
                kind: EditorKind::AppPath,
                candidates: vec![
                    format!("{program_files}\\Sublime Text\\sublime_text.exe"),
                    "subl.exe".to_string(),
                    "sublime_text.exe".to_string(),
                ],
            },
        ]
    }
    #[cfg(target_os = "macos")]
    {
        vec![
            EditorDef {
                id: "vscode",
                name: "VS Code",
                kind: EditorKind::MacApp,
                candidates: vec!["/Applications/Visual Studio Code.app".to_string()],
            },
            EditorDef {
                id: "vscode-insiders",
                name: "VS Code Insiders",
                kind: EditorKind::MacApp,
                candidates: vec!["/Applications/Visual Studio Code - Insiders.app".to_string()],
            },
            EditorDef {
                id: "cursor",
                name: "Cursor",
                kind: EditorKind::MacApp,
                candidates: vec!["/Applications/Cursor.app".to_string()],
            },
            EditorDef {
                id: "zed",
                name: "Zed",
                kind: EditorKind::MacApp,
                candidates: vec!["/Applications/Zed.app".to_string()],
            },
            EditorDef {
                id: "sublime",
                name: "Sublime Text",
                kind: EditorKind::MacApp,
                candidates: vec!["/Applications/Sublime Text.app".to_string()],
            },
        ]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        vec![
            EditorDef {
                id: "vscode",
                name: "VS Code",
                kind: EditorKind::Command,
                candidates: vec!["code".to_string()],
            },
            EditorDef {
                id: "vscode-insiders",
                name: "VS Code Insiders",
                kind: EditorKind::Command,
                candidates: vec!["code-insiders".to_string()],
            },
            EditorDef {
                id: "cursor",
                name: "Cursor",
                kind: EditorKind::Command,
                candidates: vec!["cursor".to_string()],
            },
            EditorDef {
                id: "zed",
                name: "Zed",
                kind: EditorKind::Command,
                candidates: vec!["zed".to_string()],
            },
            EditorDef {
                id: "sublime",
                name: "Sublime Text",
                kind: EditorKind::Command,
                candidates: vec!["subl".to_string()],
            },
        ]
    }
}

fn resolve_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            if !name.contains('.') {
                for extension in ["exe", "cmd", "bat"] {
                    let candidate = directory.join(format!("{name}.{extension}"));
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
    }
    None
}

fn resolve_editor(def: &EditorDef) -> Option<PathBuf> {
    for candidate in &def.candidates {
        let path = Path::new(candidate);
        if path.is_absolute() {
            if path.exists() {
                return Some(path.to_path_buf());
            }
            continue;
        }
        if let Some(resolved) = resolve_on_path(candidate) {
            return Some(resolved);
        }
    }
    None
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.as_bytes() {
        let allowed = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            );
        if allowed {
            encoded.push(*byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn normalize_remote_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if normalized.starts_with('/') {
        normalized
    } else {
        format!("/{normalized}")
    }
}

fn encode_remote_path(path: &str) -> String {
    normalize_remote_path(path)
        .split('/')
        .map(|segment| {
            if segment.is_empty() {
                String::new()
            } else {
                percent_encode(segment)
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn ssh_authority(host: &str, port: Option<u16>, username: &str, alias: Option<&str>) -> String {
    if let Some(alias) = alias.map(str::trim).filter(|value| !value.is_empty()) {
        return alias.to_string();
    }
    let user = username.trim();
    let host = host.trim();
    let user_host = if user.is_empty() {
        host.to_string()
    } else {
        format!("{user}@{host}")
    };
    match port {
        Some(port) if port != 22 => format!("{user_host}:{port}"),
        _ => user_host,
    }
}

fn build_vscode_remote_uri(path: &str, target: &RemoteTarget) -> Result<String, String> {
    match target {
        RemoteTarget::Ssh {
            host,
            port,
            username,
            ssh_config_alias,
        } => {
            let authority = ssh_authority(host, *port, username, ssh_config_alias.as_deref());
            Ok(format!(
                "vscode-remote://ssh-remote+{}{}",
                percent_encode(&authority),
                encode_remote_path(path)
            ))
        }
        RemoteTarget::Wsl { distro, user } => {
            let distro = distro
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "wsl_distro_required".to_string())?;
            let _ = user;
            Ok(format!(
                "vscode-remote://wsl+{}{}",
                percent_encode(distro),
                encode_remote_path(path)
            ))
        }
        RemoteTarget::Docker { container } => {
            let _ = container;
            Err("remote_open_unsupported".to_string())
        }
    }
}

fn path_kind(path: &Path, requested: Option<&str>) -> &'static str {
    if let Some(requested) = requested {
        if requested == "file" || requested == "directory" {
            return if requested == "file" {
                "file"
            } else {
                "directory"
            };
        }
    }
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => "file",
        Ok(metadata) if metadata.is_dir() => "directory",
        _ => "unknown",
    }
}

fn spawn_editor(program: &Path, args: &[String]) -> Result<(), String> {
    #[cfg(windows)]
    {
        let extension = program
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if extension == "cmd" || extension == "bat" {
            let mut command = Command::new("cmd");
            command.arg("/C").arg(program).args(args);
            return command
                .spawn()
                .map(|_| ())
                .map_err(|error| error.to_string());
        }
    }
    Command::new(program)
        .args(args)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// `IPlatformService.getInstalledEditors`.
#[tauri::command]
pub fn get_installed_editors() -> Vec<EditorInfo> {
    editor_defs()
        .into_iter()
        .filter(|def| resolve_editor(def).is_some())
        .map(|def| EditorInfo {
            id: def.id.to_string(),
            name: def.name.to_string(),
            icon_data_url: TRANSPARENT_PNG.to_string(),
        })
        .collect()
}

/// `IPlatformService.openInEditor`.
#[tauri::command]
pub fn open_in_editor(
    editor_id: String,
    path: String,
    options: Option<OpenInEditorOptions>,
) -> OpenInEditorResult {
    let options = options.unwrap_or_default();
    let _ = options.workspace_identity.as_deref();
    let Some(def) = editor_defs().into_iter().find(|def| def.id == editor_id) else {
        return OpenInEditorResult::failure("editor_not_found");
    };
    let Some(program) = resolve_editor(&def) else {
        return OpenInEditorResult::failure("editor_not_found");
    };
    let target_path = Path::new(&path);
    if !target_path.exists() {
        return OpenInEditorResult::failure("path_not_found");
    }
    let kind = path_kind(target_path, options.path_kind.as_deref());
    if let Some(remote) = &options.remote_target {
        if !matches!(def.id, "vscode" | "vscode-insiders") {
            return OpenInEditorResult::failure("remote_open_unsupported");
        }
        let uri = match build_vscode_remote_uri(&path, remote) {
            Ok(uri) => uri,
            Err(error) => return OpenInEditorResult::failure(error),
        };
        let flag = if kind == "file" {
            "--file-uri"
        } else {
            "--folder-uri"
        };
        return match spawn_editor(&program, &[flag.to_string(), uri]) {
            Ok(()) => OpenInEditorResult {
                success: true,
                error: None,
            },
            Err(error) => OpenInEditorResult::failure(error),
        };
    }
    let args = vec![path.clone()];
    match def.kind {
        EditorKind::MacApp => {
            let app = program.to_string_lossy().to_string();
            match spawn_editor(Path::new("open"), &["-a".to_string(), app, path]) {
                Ok(()) => OpenInEditorResult {
                    success: true,
                    error: None,
                },
                Err(error) => OpenInEditorResult::failure(error),
            }
        }
        EditorKind::AppPath | EditorKind::Command => match spawn_editor(&program, &args) {
            Ok(()) => OpenInEditorResult {
                success: true,
                error: None,
            },
            Err(error) => OpenInEditorResult::failure(error),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_vscode_ssh_remote_uri_with_alias() {
        let target = RemoteTarget::Ssh {
            host: "example.com".to_string(),
            port: Some(2222),
            username: "alice".to_string(),
            ssh_config_alias: Some("dev".to_string()),
        };
        assert_eq!(
            build_vscode_remote_uri("/home/alice/my project", &target).unwrap(),
            "vscode-remote://ssh-remote+dev/home/alice/my%20project"
        );
    }

    #[test]
    fn builds_vscode_ssh_authority_from_user_host_port() {
        let target = RemoteTarget::Ssh {
            host: "example.com".to_string(),
            port: Some(2222),
            username: "alice".to_string(),
            ssh_config_alias: None,
        };
        assert_eq!(
            build_vscode_remote_uri("/srv/app", &target).unwrap(),
            "vscode-remote://ssh-remote+alice%40example.com%3A2222/srv/app"
        );
    }

    #[test]
    fn builds_vscode_wsl_uri_and_requires_distro() {
        let target = RemoteTarget::Wsl {
            distro: Some("Ubuntu 24.04".to_string()),
            user: None,
        };
        assert_eq!(
            build_vscode_remote_uri("\\home\\me\\app", &target).unwrap(),
            "vscode-remote://wsl+Ubuntu%2024.04/home/me/app"
        );
        let missing = RemoteTarget::Wsl {
            distro: None,
            user: None,
        };
        assert_eq!(
            build_vscode_remote_uri("/app", &missing),
            Err("wsl_distro_required".to_string())
        );
    }

    #[test]
    fn docker_remote_is_explicitly_unsupported() {
        let target = RemoteTarget::Docker {
            container: "dev".to_string(),
        };
        assert_eq!(
            build_vscode_remote_uri("/app", &target),
            Err("remote_open_unsupported".to_string())
        );
    }

    #[test]
    fn percent_encodes_remote_segments() {
        assert_eq!(percent_encode("a b@c:d"), "a%20b%40c%3Ad");
        assert_eq!(
            encode_remote_path("C:\\Users\\me\\a b.txt"),
            "/C%3A/Users/me/a%20b.txt"
        );
    }

    #[test]
    fn resolves_editor_candidates_from_temp_files() {
        let root = std::env::temp_dir().join(format!(
            "zcode-editors-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join("Code.exe");
        std::fs::write(&executable, b"fake").unwrap();
        let path = executable.to_string_lossy().to_string();
        let def = EditorDef {
            id: "test",
            name: "Test",
            kind: EditorKind::AppPath,
            candidates: vec![path],
        };
        assert_eq!(resolve_editor(&def), Some(executable));
        let _ = std::fs::remove_dir_all(&root);
    }
}
