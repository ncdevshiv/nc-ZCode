//! Durable usage-fact reader (`v4/usage/stats` input side).
//!
//! Facts live in each session transcript as `{"t":"usage","fact":{…}}` entries
//! (written by `nc-code-host::turn::persist_usage_fact`). The transcript is
//! append-only and is the session's own file, so the aggregate needs no second
//! database: walk `<app-config>/agent/<workspace>/sessions/*/transcript.jsonl`.
//!
//! Unreadable/malformed transcripts are **skipped with a count**, never guessed:
//! a missing fact shows up as absent usage, not as an invented number.

use std::path::{Path, PathBuf};

use crate::usage_aggregate::UsageFact;

/// Summary of one aggregation scan (diagnostics for the service log and tests).
#[derive(Debug, Clone, Default)]
pub struct UsageScan {
    pub facts: Vec<UsageFact>,
    pub transcripts: usize,
    pub skipped_files: usize,
    pub skipped_lines: usize,
}

/// Session transcripts under `agent_root`.
///
/// Two layouts are accepted because two real callers share this reader:
/// - `<agent_root>/<workspace-key>/sessions/<id>/transcript.jsonl` (app config agent root);
/// - `<root>/sessions/<id>/transcript.jsonl` (a host data dir, which keeps all
///   workspaces in one store).
pub fn transcript_paths(agent_root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let Ok(entries) = std::fs::read_dir(agent_root) else {
        return paths;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name() == std::ffi::OsStr::new("sessions") {
            collect_session_transcripts(&path, &mut paths);
        } else if path.is_dir() {
            collect_session_transcripts(&path.join("sessions"), &mut paths);
        }
    }
    paths
}

fn collect_session_transcripts(sessions: &Path, paths: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(sessions) else {
        return;
    };
    for session in entries.flatten() {
        let transcript = session.path().join("transcript.jsonl");
        if transcript.is_file() {
            paths.push(transcript);
        }
    }
}

/// Parse one transcript file into usage facts. Only `t == "usage"` lines are read;
/// every other entry type is ignored (the file is shared with rows/history).
pub fn usage_facts_in_transcript(path: &Path, scan: &mut UsageScan) {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(_) => {
            scan.skipped_files += 1;
            return;
        }
    };
    scan.transcripts += 1;
    for line in raw.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            scan.skipped_lines += 1;
            continue;
        };
        if value.get("t").and_then(serde_json::Value::as_str) != Some("usage") {
            continue;
        }
        match value.get("fact").and_then(UsageFact::from_value) {
            Some(fact) => scan.facts.push(fact),
            None => scan.skipped_lines += 1,
        }
    }
}

/// Scan every workspace transcript under `agent_root`.
pub fn scan_agent_usage(agent_root: &Path) -> UsageScan {
    let mut scan = UsageScan::default();
    for transcript in transcript_paths(agent_root) {
        usage_facts_in_transcript(&transcript, &mut scan);
    }
    scan
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn temp_root(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("zcode-usage-scan-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        root
    }

    fn write_transcript(root: &Path, workspace: &str, session: &str, lines: &[String]) {
        let dir = root.join(workspace).join("sessions").join(session);
        fs::create_dir_all(&dir).expect("session dir");
        fs::write(dir.join("transcript.jsonl"), lines.join("\n")).expect("transcript");
    }

    #[test]
    fn scan_reads_only_usage_entries_across_workspaces() {
        let root = temp_root("mixed");
        write_transcript(
            &root,
            "ws-a",
            "s1",
            &[
                json!({"t": "row", "row": {"kind": "assistantText", "text": "hi"}}).to_string(),
                json!({"t": "usage", "fact": {
                    "sessionId": "s1", "modelId": "m1", "inputTokens": 10, "outputTokens": 5,
                    "toolCallCount": 2, "toolErrorCount": 0, "startedAt": 100, "endedAt": 200,
                    "durationMs": 100, "modelRequestCount": 3,
                }})
                .to_string(),
                "not json at all".to_string(),
            ],
        );
        write_transcript(
            &root,
            "ws-b",
            "s2",
            &[json!({"t": "usage", "fact": {
                "sessionId": "s2", "modelId": "m2", "inputTokens": 1, "outputTokens": 1,
                "startedAt": 300, "endedAt": 400, "durationMs": 100,
            }})
            .to_string()],
        );
        let scan = scan_agent_usage(&root);
        assert_eq!(scan.transcripts, 2);
        assert_eq!(
            scan.skipped_lines, 1,
            "the unparsable line is counted, not guessed"
        );
        assert_eq!(scan.facts.len(), 2);
        assert_eq!(scan.facts[0].session_id, "s1");
        assert_eq!(scan.facts[0].tool_call_count, 2);
        assert_eq!(scan.facts[1].model_id.as_deref(), Some("m2"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_reads_a_host_data_dir_with_flat_sessions() {
        // The native host keeps one store per data dir: `<data-dir>/sessions/<id>/transcript.jsonl`
        // (no workspace-key level), which this reader must accept as well.
        let root = temp_root("flat");
        let dir = root.join("sessions").join("s-host");
        fs::create_dir_all(&dir).expect("session dir");
        fs::write(
            dir.join("transcript.jsonl"),
            json!({"t": "usage", "fact": {
                "sessionId": "s-host", "modelId": "m", "inputTokens": 2, "outputTokens": 1,
                "startedAt": 10, "endedAt": 20, "durationMs": 10,
            }})
            .to_string(),
        )
        .expect("transcript");
        let scan = scan_agent_usage(&root);
        assert_eq!(scan.transcripts, 1);
        assert_eq!(scan.facts.len(), 1);
        assert_eq!(scan.facts[0].session_id, "s-host");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_skips_absent_agent_root_without_fabricating_facts() {
        let scan = scan_agent_usage(Path::new("/definitely/not/a/root"));
        assert!(scan.facts.is_empty());
        assert_eq!(scan.transcripts, 0);
    }
}
