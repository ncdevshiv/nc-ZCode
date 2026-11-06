//! Resource Manager window for the Tauri shell.
//!
//! Mirrors the Electron Help → Resource Manager entry with a native window that
//! shows system memory/CPU and the memory/CPU of ZCode-related processes. The
//! Electron implementation used `app.getAppMetrics()` plus host/agent utility
//! process bookkeeping; the Tauri shell samples the OS process table with
//! `sysinfo` instead, which works without Electron's process model.

use std::sync::Mutex;

use serde::Serialize;
use sysinfo::System;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

const WINDOW_LABEL: &str = "resource-manager";
const WINDOW_TITLE: &str = "Resource Manager";
const MAX_PROCESSES: usize = 50;
const PROCESS_NAME_MARKERS: [&str; 2] = ["zcode", "nc-code"];

#[derive(Default)]
pub struct ResourceManagerState {
    system: Mutex<System>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SystemSnapshot {
    pub os: String,
    pub os_version: String,
    pub kernel_version: String,
    pub host_name: String,
    pub logical_cpus: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub physical_cpus: Option<usize>,
    pub total_memory_bytes: u64,
    pub used_memory_bytes: u64,
    pub cpu_usage_percent: f32,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProcessSnapshot {
    pub pid: u32,
    pub name: String,
    pub memory_bytes: u64,
    pub cpu_usage_percent: f32,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ResourceSnapshot {
    pub system: SystemSnapshot,
    pub processes: Vec<ProcessSnapshot>,
}

/// Process names from the shell, the CLI agent, and `nc-code-*` helpers.
pub fn is_zcode_process_name(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    PROCESS_NAME_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

/// Sort by memory (descending) and cap the table size.
pub fn select_processes(mut processes: Vec<ProcessSnapshot>) -> Vec<ProcessSnapshot> {
    processes.sort_by(|left, right| {
        right
            .memory_bytes
            .cmp(&left.memory_bytes)
            .then_with(|| left.pid.cmp(&right.pid))
    });
    processes.truncate(MAX_PROCESSES);
    processes
}

fn collect_snapshot(state: &ResourceManagerState) -> ResourceSnapshot {
    let mut system = match state.system.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    system.refresh_all();

    let current_pid = sysinfo::get_current_pid().ok();
    let processes = system
        .processes()
        .values()
        .filter(|process| {
            let name = process.name().to_string_lossy();
            is_zcode_process_name(&name) || Some(process.pid()) == current_pid
        })
        .map(|process| ProcessSnapshot {
            pid: process.pid().as_u32(),
            name: process.name().to_string_lossy().to_string(),
            memory_bytes: process.memory(),
            cpu_usage_percent: process.cpu_usage(),
        })
        .collect();

    ResourceSnapshot {
        system: SystemSnapshot {
            os: System::name().unwrap_or_else(|| std::env::consts::OS.to_string()),
            os_version: System::os_version().unwrap_or_default(),
            kernel_version: System::kernel_version().unwrap_or_default(),
            host_name: System::host_name().unwrap_or_default(),
            logical_cpus: system.cpus().len(),
            physical_cpus: system.physical_core_count(),
            total_memory_bytes: system.total_memory(),
            used_memory_bytes: system.used_memory(),
            cpu_usage_percent: system.global_cpu_usage(),
        },
        processes: select_processes(processes),
    }
}

/// `IPlatformService.executeDesktopCommand("openResourceManager")`.
pub fn open(app: &AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        return Ok(());
    }
    WebviewWindowBuilder::new(
        app,
        WINDOW_LABEL,
        WebviewUrl::App("resource-manager.html".into()),
    )
    .title(WINDOW_TITLE)
    .inner_size(820.0, 600.0)
    .min_inner_size(560.0, 360.0)
    .build()
    .map(|_| ())
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn resource_manager_snapshot(
    state: tauri::State<'_, ResourceManagerState>,
) -> ResourceSnapshot {
    collect_snapshot(&state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, name: &str, memory_bytes: u64) -> ProcessSnapshot {
        ProcessSnapshot {
            pid,
            name: name.to_string(),
            memory_bytes,
            cpu_usage_percent: 0.0,
        }
    }

    #[test]
    fn matches_shell_cli_and_helper_processes() {
        assert!(is_zcode_process_name("zcode-shell.exe"));
        assert!(is_zcode_process_name("ZCode"));
        assert!(is_zcode_process_name("nc-code-host"));
        assert!(is_zcode_process_name("  nc-code-server  "));
        assert!(!is_zcode_process_name("explorer.exe"));
        assert!(!is_zcode_process_name(""));
    }

    #[test]
    fn sorts_by_memory_and_caps_the_table() {
        let selected = select_processes(vec![
            process(1, "small", 10),
            process(2, "large", 300),
            process(3, "medium", 100),
        ]);
        assert_eq!(
            selected.iter().map(|item| item.pid).collect::<Vec<_>>(),
            vec![2, 3, 1]
        );

        let many: Vec<ProcessSnapshot> = (0..MAX_PROCESSES + 10)
            .map(|index| process(index as u32, "zcode", index as u64))
            .collect();
        let capped = select_processes(many);
        assert_eq!(capped.len(), MAX_PROCESSES);
        assert_eq!(capped[0].memory_bytes, (MAX_PROCESSES + 9) as u64);
    }

    #[test]
    fn snapshot_serializes_with_camel_case_fields() {
        let snapshot = ResourceSnapshot {
            system: SystemSnapshot {
                os: "Windows".to_string(),
                os_version: "11".to_string(),
                kernel_version: "10.0".to_string(),
                host_name: "host".to_string(),
                logical_cpus: 8,
                physical_cpus: Some(4),
                total_memory_bytes: 1024,
                used_memory_bytes: 512,
                cpu_usage_percent: 12.5,
            },
            processes: vec![process(7, "zcode-shell", 256)],
        };
        let value = serde_json::to_value(snapshot).unwrap();
        assert_eq!(value["system"]["totalMemoryBytes"], 1024);
        assert_eq!(value["system"]["logicalCpus"], 8);
        assert_eq!(value["processes"][0]["memoryBytes"], 256);
    }
}
