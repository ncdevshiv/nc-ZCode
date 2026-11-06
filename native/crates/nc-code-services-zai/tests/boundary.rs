//! Structural guard for the claim data boundary (`specs/zai-claim-boundary.md`).
//!
//! These tests are intentionally source-level: the claim crate must not gain a
//! dependency on the agent/file/provider runtime, and its non-comment code must not
//! mention model-data concepts (prompt/history/telemetry/usage).

use std::fs;
use std::path::Path;

fn source_files() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    for entry in fs::read_dir(&dir).expect("src dir") {
        let entry = entry.expect("entry");
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let contents = fs::read_to_string(&path).expect("read source");
            files.push((name, contents));
        }
    }
    files
}

#[test]
fn cargo_dependencies_stay_within_the_claim_boundary() {
    let manifest = include_str!("../Cargo.toml");
    // Allowed deps for the (currently disabled/stub) zai crate in this workspace.
    // Keep the claim boundary tight: no host/agent/provider/runtime/file deps.
    let allowed = [
        "async-trait",
        "base64",
        "rand",
        "reqwest",
        "serde",
        "serde_json",
        "tokio",
        "url",
        "nc-code-channels",
        "nc-code-rpc",
        "nc-code-service-core",
        "nc-code-services-config",
    ];
    let mut in_dependencies = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_dependencies = trimmed == "[dependencies]";
            continue;
        }
        if !in_dependencies || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let name = trimmed.split('=').next().unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        assert!(
            allowed.contains(&name),
            "claim crate must not depend on {name}; add it to the boundary spec first"
        );
    }
}

#[test]
fn claim_code_never_mentions_model_data_concepts() {
    let forbidden = [
        "prompt",
        "messages",
        "conversation",
        "workspace",
        "telemetry",
        "analytics",
        "completion",
    ];
    for (name, contents) in source_files() {
        for (index, line) in contents.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
                continue;
            }
            let lower = line.to_ascii_lowercase();
            for term in forbidden {
                assert!(
                    !lower.contains(term),
                    "{name}:{} mentions `{term}` outside a comment; claim code must not model-data",
                    index + 1
                );
            }
        }
    }
}

#[test]
fn claim_code_has_no_agent_or_file_runtime_imports() {
    for (name, contents) in source_files() {
        for (index, line) in contents.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for needle in [
                "nc_code_host",
                "nc-code-host",
                "nc_code_provider",
                "nc-code-provider",
                "nc_code_core",
                "nc-code-core",
                "nc_code_scan",
                "nc_code_watch",
                "nc_code_proc",
                "std::fs",
                "tokio::fs",
            ] {
                assert!(
                    !line.contains(needle),
                    "{name}:{} imports/refs {needle}; the claim path must stay account-only",
                    index + 1
                );
            }
        }
    }
}
