//! Crash capture for the shipped shell.
//!
//! Electron shipped Crashpad and archived renderer/main process dumps. The Tauri shell is
//! a single native process, so the equivalent guarantee is: **any panic that escapes is
//! recorded on disk with location and backtrace before the process dies**, and the report
//! set is bounded (the oldest files are pruned). The panic hook never masks the crash —
//! the previous hook still runs, so the process still aborts/exits as it would.
//!
//! The data dir is `<data-base>/.zcode/crashes` (same root as settings/telemetry), i.e. it
//! lives next to the app data the user can attach to a bug report.

use std::io::Write;
use std::path::{Path, PathBuf};

use nc_code_service_core::AppPaths;

/// Retained crash reports. Bounded so a crash loop cannot fill the disk.
pub const MAX_CRASH_FILES: usize = 10;

/// `<data-base>/.zcode/crashes`.
pub fn crash_dir(paths: &AppPaths) -> PathBuf {
    paths.nc_code_data_root().join("crashes")
}

/// One human-readable report. Kept pure so its shape is pinned by a test.
pub fn format_report(
    when_ms: i64,
    thread: &str,
    payload: &str,
    location: Option<&str>,
    backtrace: Option<&str>,
) -> String {
    let mut report = String::new();
    report.push_str("NC-Code shell crash report\n");
    report.push_str(&format!("time_ms: {when_ms}\n"));
    report.push_str(&format!("thread: {thread}\n"));
    report.push_str(&format!("payload: {payload}\n"));
    match location {
        Some(location) => report.push_str(&format!("location: {location}\n")),
        None => report.push_str("location: <unknown>\n"),
    }
    match backtrace {
        Some(backtrace) => {
            report.push_str("backtrace:\n");
            report.push_str(backtrace);
            if !backtrace.ends_with('\n') {
                report.push('\n');
            }
        }
        None => report.push_str("backtrace: <unavailable>\n"),
    }
    report
}

/// Crash file name: zero-padded timestamp first so lexical order is chronological.
pub fn crash_file_name(when_ms: i64, pid: u32) -> String {
    format!("crash-{when_ms:013}-{pid}.log")
}

/// Keep the newest `keep` crash reports; returns how many were removed. Files that are
/// not crash reports are left alone (the hook never deletes unowned data).
pub fn prune(dir: &Path, keep: usize) -> std::io::Result<usize> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(0);
    };
    let mut reports: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("crash-") && name.ends_with(".log"))
        })
        .collect();
    // Names embed a zero-padded millisecond timestamp, so newest-first is lexical-desc.
    reports.sort();
    let mut removed = 0;
    while reports.len() > keep {
        let oldest = reports.remove(0);
        if std::fs::remove_file(&oldest).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Write one report and prune the set. Returns the file path when the write succeeded;
/// failures are swallowed (the panic path must not fail differently).
pub fn write_report(
    dir: &Path,
    when_ms: i64,
    thread: &str,
    payload: &str,
    location: Option<&str>,
    backtrace: Option<&str>,
) -> Option<PathBuf> {
    std::fs::create_dir_all(dir).ok()?;
    let path = dir.join(crash_file_name(when_ms, std::process::id()));
    let report = format_report(when_ms, thread, payload, location, backtrace);
    let mut file = std::fs::File::create(&path).ok()?;
    file.write_all(report.as_bytes()).ok()?;
    file.flush().ok()?;
    let _ = prune(dir, MAX_CRASH_FILES);
    Some(path)
}

/// Install the panic hook. The previous hook is preserved and still runs.
pub fn install(dir: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let when_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0);
        let thread = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string();
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|text| (*text).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        let location = info.location().map(|location| {
            format!(
                "{}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            )
        });
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        if let Some(path) = write_report(
            &dir,
            when_ms,
            &thread,
            &payload,
            location.as_deref(),
            Some(&backtrace),
        ) {
            eprintln!("[zcode-shell] crash report written: {}", path.display());
        }
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zcode-crash-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn report_carries_thread_payload_location_and_backtrace() {
        let report = format_report(
            1_700_000_000_000,
            "main",
            "boom",
            Some("src/main.rs:42:7"),
            Some("0: zcode_shell::main"),
        );
        assert!(report.contains("time_ms: 1700000000000"), "{report}");
        assert!(report.contains("thread: main"), "{report}");
        assert!(report.contains("payload: boom"), "{report}");
        assert!(report.contains("location: src/main.rs:42:7"), "{report}");
        assert!(
            report.contains("backtrace:\n0: zcode_shell::main"),
            "{report}"
        );
    }

    #[test]
    fn report_without_location_or_backtrace_is_explicit() {
        let report = format_report(1, "worker", "boom", None, None);
        assert!(report.contains("location: <unknown>"), "{report}");
        assert!(report.contains("backtrace: <unavailable>"), "{report}");
    }

    #[test]
    fn crash_names_sort_chronologically() {
        let older = crash_file_name(1_700_000_000_000, 1);
        let newer = crash_file_name(1_700_000_000_500, 2);
        assert!(older < newer, "{older} < {newer}");
        assert_eq!(older, "crash-1700000000000-1.log");
    }

    #[test]
    fn prune_keeps_the_newest_reports_and_ignores_other_files() {
        let dir = temp_dir("prune");
        for index in 0..5 {
            std::fs::write(
                dir.join(crash_file_name(1_700_000_000_000 + index, 1)),
                "old",
            )
            .expect("write crash");
        }
        std::fs::write(dir.join("settings.json"), "{}").expect("write other");
        let removed = prune(&dir, 2).expect("prune");
        assert_eq!(removed, 3);
        let remaining: Vec<String> = std::fs::read_dir(&dir)
            .expect("read")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            remaining.len(),
            3,
            "2 crashes + settings.json: {remaining:?}"
        );
        assert!(remaining.contains(&"settings.json".to_string()));
        assert!(!remaining.contains(&crash_file_name(1_700_000_000_000, 1)));
        assert!(remaining.contains(&crash_file_name(1_700_000_000_004, 1)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prune_on_a_missing_dir_is_not_an_error() {
        let missing = std::env::temp_dir().join("zcode-crash-does-not-exist-xyz");
        assert_eq!(prune(&missing, 10).expect("prune"), 0);
    }

    #[test]
    fn write_report_persists_the_report_and_prunes_old_ones() {
        let dir = temp_dir("write");
        for index in 0..MAX_CRASH_FILES {
            write_report(
                &dir,
                1_700_000_000_000 + index as i64,
                "main",
                "earlier",
                None,
                None,
            )
            .expect("earlier report");
        }
        let path = write_report(
            &dir,
            1_700_000_000_000 + MAX_CRASH_FILES as i64,
            "worker",
            "hook round trip",
            Some("src/main.rs:7:1"),
            Some("0: zcode_shell::main"),
        )
        .expect("report path");
        let text = std::fs::read_to_string(&path).expect("report text");
        assert!(text.contains("payload: hook round trip"), "{text}");
        assert!(text.contains("thread: worker"), "{text}");
        assert!(text.contains("location: src/main.rs:7:1"), "{text}");
        let retained = std::fs::read_dir(&dir).expect("read").count();
        assert!(
            retained <= MAX_CRASH_FILES,
            "retention bound holds, got {retained}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
