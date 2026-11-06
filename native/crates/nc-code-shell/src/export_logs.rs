//! Log export for the Tauri shell.
//!
//! Collects the same diagnostic sources as the Electron main process:
//!
//! - `{appConfigDir}/logs` (desktop logs),
//! - `~/.zcode/cli/log`, `~/.zcode/cli/config.json`, `~/.zcode/cli/rollout`,
//! - `~/.zcode/computer-use/run/*.exit.log` (helper diagnostics only; the
//!   directory also contains broker tokens).
//!
//! The archive is written to a user-chosen path via the native save dialog.
//! Limits keep a runaway log directory from filling the disk: 200 MiB total,
//! 50 MiB per file, 20k files.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::AppHandle;
use tauri_plugin_dialog::DialogExt;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const MAX_TOTAL_BYTES: u64 = 200 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
const MAX_FILES: usize = 20_000;
const RECENT_WINDOW: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Clone, Copy)]
struct Limits {
    max_total_bytes: u64,
    max_file_bytes: u64,
    max_files: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_total_bytes: MAX_TOTAL_BYTES,
            max_file_bytes: MAX_FILE_BYTES,
            max_files: MAX_FILES,
        }
    }
}

#[derive(Clone, Copy)]
enum LogSourceKind {
    Directory,
    File,
}

#[derive(Clone)]
struct LogSource {
    path: PathBuf,
    archive_prefix: String,
    kind: LogSourceKind,
    filter: Option<fn(&str) -> bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogEntry {
    absolute_path: PathBuf,
    archive_path: String,
    size: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportLogsResult {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ExportLogsResult {
    fn failure(error: impl Into<String>) -> Self {
        Self {
            success: false,
            path: None,
            error: Some(error.into()),
        }
    }
}

fn join_archive_path(prefix: &str, relative: &str) -> String {
    let relative = relative.replace('\\', "/");
    let relative = relative.trim_start_matches('/');
    if prefix.is_empty() {
        relative.to_string()
    } else {
        format!("{}/{}", prefix.trim_end_matches('/'), relative)
    }
}

fn is_safe_relative(relative: &Path) -> bool {
    relative
        .components()
        .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn collect_directory(
    source: &LogSource,
    limits: Limits,
    entries: &mut Vec<LogEntry>,
    total_bytes: &mut u64,
    now: SystemTime,
) {
    let mut stack = vec![source.path.clone()];
    while let Some(directory) = stack.pop() {
        let Ok(read_dir) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if let Some(filter) = source.filter {
                if !filter(name) {
                    continue;
                }
            }
            let Ok(relative) = path.strip_prefix(&source.path) else {
                continue;
            };
            if !is_safe_relative(relative) {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.len() > limits.max_file_bytes {
                continue;
            }
            let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
            if now.duration_since(modified).unwrap_or_default() > RECENT_WINDOW {
                continue;
            }
            if entries.len() >= limits.max_files {
                return;
            }
            if total_bytes.saturating_add(metadata.len()) > limits.max_total_bytes {
                return;
            }
            *total_bytes += metadata.len();
            entries.push(LogEntry {
                absolute_path: path.clone(),
                archive_path: join_archive_path(
                    &source.archive_prefix,
                    &relative.to_string_lossy(),
                ),
                size: metadata.len(),
            });
        }
    }
}

fn collect_single_file(
    source: &LogSource,
    limits: Limits,
    entries: &mut Vec<LogEntry>,
    total_bytes: &mut u64,
) {
    let Ok(metadata) = std::fs::metadata(&source.path) else {
        return;
    };
    if !metadata.is_file() || metadata.len() > limits.max_file_bytes {
        return;
    }
    if entries.len() >= limits.max_files
        || total_bytes.saturating_add(metadata.len()) > limits.max_total_bytes
    {
        return;
    }
    *total_bytes += metadata.len();
    entries.push(LogEntry {
        absolute_path: source.path.clone(),
        archive_path: source.archive_prefix.clone(),
        size: metadata.len(),
    });
}

fn collect_entries(sources: &[LogSource], limits: Limits, now: SystemTime) -> Vec<LogEntry> {
    let mut entries = Vec::new();
    let mut total_bytes = 0u64;
    for source in sources {
        match source.kind {
            LogSourceKind::Directory => {
                collect_directory(source, limits, &mut entries, &mut total_bytes, now);
            }
            LogSourceKind::File => {
                collect_single_file(source, limits, &mut entries, &mut total_bytes);
            }
        }
    }
    entries.sort_by(|left, right| left.archive_path.cmp(&right.archive_path));
    entries
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    (year, month as u32, day as u32)
}

fn format_timestamp(now: SystemTime) -> String {
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default();
    let days = seconds.div_euclid(86_400);
    let remainder = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        remainder / 3_600,
        (remainder % 3_600) / 60,
        remainder % 60
    )
}

fn write_archive(entries: &[LogEntry], output: &Path, generated_at: &str) -> Result<u64, String> {
    let file = File::create(output).map_err(|error| format!("create_failed: {error}"))?;
    let mut archive = ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);
    let manifest = format!(
        "zcode log export\nGenerated: {generated_at}\nFiles: {}\n",
        entries.len()
    );
    archive
        .start_file("about.txt", options)
        .map_err(|error| format!("zip_failed: {error}"))?;
    archive
        .write_all(manifest.as_bytes())
        .map_err(|error| format!("zip_failed: {error}"))?;
    let mut written = 0u64;
    for entry in entries {
        let Ok(mut source) = File::open(&entry.absolute_path) else {
            continue;
        };
        archive
            .start_file(&entry.archive_path, options)
            .map_err(|error| format!("zip_failed: {error}"))?;
        std::io::copy(&mut source, &mut archive).map_err(|error| format!("zip_failed: {error}"))?;
        written += entry.size;
    }
    archive
        .finish()
        .map_err(|error| format!("zip_failed: {error}"))?;
    Ok(written)
}

fn build_sources(app_config_dir: &Path, home_dir: &Path) -> Vec<LogSource> {
    let cli_dir = home_dir.join(".zcode").join("cli");
    vec![
        LogSource {
            path: app_config_dir.join("logs"),
            archive_prefix: String::new(),
            kind: LogSourceKind::Directory,
            filter: None,
        },
        LogSource {
            path: cli_dir.join("log"),
            archive_prefix: ".zcode/cli/log".to_string(),
            kind: LogSourceKind::Directory,
            filter: None,
        },
        LogSource {
            path: cli_dir.join("config.json"),
            archive_prefix: ".zcode/cli/config.json".to_string(),
            kind: LogSourceKind::File,
            filter: None,
        },
        LogSource {
            path: cli_dir.join("rollout"),
            archive_prefix: ".zcode/cli/rollout".to_string(),
            kind: LogSourceKind::Directory,
            filter: None,
        },
        LogSource {
            path: home_dir.join(".zcode").join("computer-use").join("run"),
            archive_prefix: ".zcode/computer-use/run".to_string(),
            kind: LogSourceKind::Directory,
            filter: Some(is_exit_log),
        },
    ]
}

fn is_exit_log(name: &str) -> bool {
    name.ends_with(".exit.log")
}

/// `IPlatformService.exportLogs`: collect logs and save a timestamped zip via
/// the native save dialog. Cancelling the dialog returns an explicit error.
#[tauri::command]
pub async fn export_logs(app: AppHandle) -> ExportLogsResult {
    let paths = nc_code_service_core::AppPaths::from_env();
    let sources = build_sources(&paths.app_config_dir(), paths.home_dir());
    let now = SystemTime::now();
    let generated_at = format_timestamp(now);
    let file_name = format!("zcode-logs-{generated_at}.zip");
    let picked = app
        .dialog()
        .file()
        .set_file_name(&file_name)
        .blocking_save_file();
    let Some(path) = picked else {
        return ExportLogsResult::failure("export_canceled");
    };
    let output = match path.into_path() {
        Ok(path) => path,
        Err(error) => return ExportLogsResult::failure(error.to_string()),
    };
    let entries = collect_entries(&sources, Limits::default(), now);
    let output_for_result = output.to_string_lossy().to_string();
    let output_for_write = output.clone();
    match tauri::async_runtime::spawn_blocking(move || {
        write_archive(&entries, &output_for_write, &generated_at)
    })
    .await
    {
        Ok(Ok(_)) => ExportLogsResult {
            success: true,
            path: Some(output_for_result),
            error: None,
        },
        Ok(Err(error)) => ExportLogsResult::failure(error),
        Err(error) => ExportLogsResult::failure(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use zip::ZipArchive;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zcode-export-logs-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn source_dir(path: PathBuf, prefix: &str) -> LogSource {
        LogSource {
            path,
            archive_prefix: prefix.to_string(),
            kind: LogSourceKind::Directory,
            filter: None,
        }
    }

    #[test]
    fn formats_timestamp_in_utc() {
        assert_eq!(
            format_timestamp(UNIX_EPOCH + Duration::from_secs(1_704_067_200)),
            "20240101-000000"
        );
        assert_eq!(format_timestamp(UNIX_EPOCH), "19700101-000000");
    }

    #[test]
    fn collects_directory_layout_and_filters_exit_logs() {
        let root = temp_dir("collect");
        std::fs::create_dir_all(root.join("logs").join("sub")).unwrap();
        std::fs::write(root.join("logs").join("a.log"), b"a").unwrap();
        std::fs::write(root.join("logs").join("sub").join("b.log"), b"b").unwrap();
        std::fs::create_dir_all(root.join("run")).unwrap();
        std::fs::write(root.join("run").join("helper.exit.log"), b"exit").unwrap();
        std::fs::write(root.join("run").join(".tokens"), b"secret").unwrap();
        let sources = vec![
            source_dir(root.join("logs"), ""),
            LogSource {
                path: root.join("run"),
                archive_prefix: ".zcode/computer-use/run".to_string(),
                kind: LogSourceKind::Directory,
                filter: Some(is_exit_log),
            },
        ];
        let entries = collect_entries(&sources, Limits::default(), SystemTime::now());
        let paths: Vec<&str> = entries
            .iter()
            .map(|entry| entry.archive_path.as_str())
            .collect();
        assert_eq!(
            paths,
            vec![
                ".zcode/computer-use/run/helper.exit.log",
                "a.log",
                "sub/b.log"
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn enforces_file_and_total_limits() {
        let root = temp_dir("limits");
        std::fs::write(root.join("big.log"), vec![0u8; 64]).unwrap();
        std::fs::write(root.join("small.log"), b"small").unwrap();
        let sources = vec![source_dir(root.clone(), "")];
        let limits = Limits {
            max_total_bytes: 32,
            max_file_bytes: 32,
            max_files: 10,
        };
        let entries = collect_entries(&sources, limits, SystemTime::now());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].archive_path, "small.log");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn writes_a_readable_zip_with_manifest() {
        let root = temp_dir("zip");
        std::fs::write(root.join("a.log"), b"hello").unwrap();
        let entries = collect_entries(
            &[source_dir(root.clone(), "")],
            Limits::default(),
            SystemTime::now(),
        );
        let output = root.join("out.zip");
        write_archive(&entries, &output, "20240101-000000").unwrap();
        let file = File::open(&output).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        assert!(archive.by_name("about.txt").is_ok());
        let mut contents = String::new();
        archive
            .by_name("a.log")
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        assert_eq!(contents, "hello");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ignores_files_outside_the_recent_window() {
        let root = temp_dir("recent");
        std::fs::write(root.join("old.log"), b"old").unwrap();
        let entries = collect_entries(
            &[source_dir(root.clone(), "")],
            Limits::default(),
            SystemTime::now() + RECENT_WINDOW + Duration::from_secs(1),
        );
        assert!(entries.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
