//! The wording and delivery model for background-task completion notifications (TS `runtime-task/notification.ts` +
//! `tool/executor/background-tasks.ts:formatBackgroundTaskNotification`).
//!
//! A notification is one **model-visible** input: after the worker finishes, the host starts a
//! `backgroundResult` turn in the parent conversation, so the model knows to fetch the result with `TaskOutput`.
//! The wording shape is aligned with TS field by field:
//!
//! ```text
//! <task-notification>
//! <task-id>…</task-id>
//! <tool-use-id>…</tool-use-id>   (written only when present)
//! <output-file>…</output-file>   (written only when present)
//! <status>completed|failed|killed|lost</status>
//! <summary>…</summary>
//! <result>…</result>             (when the sub-agent has final text)
//! <error>…</error>               (on failure)
//! </task-notification>
//! ```
//!
//! bash notifications carry no `<result>`/`<error>` (same as TS: the full output goes through `output-file`).

use serde_json::Value;

use crate::background::{
    BackgroundWork, BG_CANCELLED, BG_COMPLETED, BG_FAILED, BG_SPAWN_ERROR, TASK_TYPE_LOCAL_AGENT,
};

/// The three terminal status words of TS `TaskNotificationInput.status` (the closed set of the bash tracker).
fn notification_status(status: &str) -> &'static str {
    match status {
        BG_COMPLETED => "completed",
        BG_CANCELLED => "killed",
        BG_FAILED | BG_SPAWN_ERROR => "failed",
        _ => "failed",
    }
}

fn escape_xml(text: &str) -> String {
    // Both TS escapeXml/escapeLocalBashXml handle at least <>&; here quotes are escaped as well, uniformly,
    // which is equivalent for the parser and does not change the text semantics.
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// TS `escapeLocalBashXml`: bash notifications escape only `<>&` (quotes kept verbatim).
fn escape_local_bash_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// TS `buildBackgroundTaskSummary`（bash）。
pub fn bash_summary(work: &BackgroundWork) -> String {
    // TS `subject = description ?? command ?? "Bash background command"`.
    let subject = work
        .description
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let command = work.command.trim();
            (!command.is_empty()).then(|| command.to_string())
        })
        .unwrap_or_else(|| "Bash background command".to_string());
    let prefix = format!("Background command \"{subject}\"");
    match notification_status(work.status) {
        "completed" => match work.exit_code {
            Some(code) => format!("{prefix} completed (exit code {code})"),
            None => format!("{prefix} completed"),
        },
        "failed" => match work.exit_code {
            Some(code) => format!("{prefix} failed with exit code {code}"),
            None => format!("{prefix} failed"),
        },
        _ => format!("{prefix} was stopped"),
    }
}

/// The summary of a sub-agent notification. On the TS side the runner's formatter mints it; here the same set of facts
/// (description + terminal status) is used, with wording kept short and directly readable by the model.
pub fn agent_summary(work: &BackgroundWork, status: &str) -> String {
    let subject = work
        .description
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| work.work_id.clone());
    match notification_status(status) {
        "completed" => format!("Agent \"{subject}\" completed"),
        "killed" => format!("Agent \"{subject}\" was stopped"),
        _ => format!("Agent \"{subject}\" failed"),
    }
}

fn push_line(lines: &mut Vec<String>, tag: &str, value: &str) {
    if !value.is_empty() {
        lines.push(format!("<{tag}>{}</{tag}>", escape_xml(value)));
    }
}

/// The notification text of a bash task.
pub fn bash_notification_text(work: &BackgroundWork) -> String {
    let mut lines = vec!["<task-notification>".to_string()];
    push_line(&mut lines, "task-id", &work.work_id);
    if let Some(tool_call_id) = &work.tool_call_id {
        push_line(&mut lines, "tool-use-id", tool_call_id);
    }
    push_line(
        &mut lines,
        "output-file",
        &work.output_path.to_string_lossy(),
    );
    lines.push(format!(
        "<status>{}</status>",
        escape_local_bash_xml(notification_status(work.status))
    ));
    lines.push(format!(
        "<summary>{}</summary>",
        escape_local_bash_xml(&bash_summary(work))
    ));
    lines.push("</task-notification>".to_string());
    lines.join("\n")
}

/// The notification text of a sub-agent task. `result` is the sub-agent's final text (obtained by reading the output file).
pub fn agent_notification_text(work: &BackgroundWork, result: Option<&str>) -> String {
    let mut lines = vec!["<task-notification>".to_string()];
    push_line(&mut lines, "task-id", &work.work_id);
    if let Some(tool_call_id) = &work.tool_call_id {
        push_line(&mut lines, "tool-use-id", tool_call_id);
    }
    push_line(
        &mut lines,
        "output-file",
        &work.output_path.to_string_lossy(),
    );
    lines.push(format!(
        "<status>{}</status>",
        escape_xml(notification_status(work.status))
    ));
    lines.push(format!(
        "<summary>{}</summary>",
        escape_xml(&agent_summary(work, work.status))
    ));
    if let Some(result) = result {
        if !result.trim().is_empty() {
            push_line(&mut lines, "result", result);
        }
    }
    if notification_status(work.status) == "failed" {
        if let Some(error) = &work.error {
            push_line(&mut lines, "error", error);
        }
    }
    lines.push("</task-notification>".to_string());
    lines.join("\n")
}

/// Choose the wording by task type.
pub fn notification_text(work: &BackgroundWork, result: Option<&str>) -> String {
    if work.task_type == TASK_TYPE_LOCAL_AGENT {
        agent_notification_text(work, result)
    } else {
        bash_notification_text(work)
    }
}

/// The delivery model: the registry calls this once after the task settles (`claim_notification` guarantees exactly one call).
pub trait BackgroundNotifier: Send + Sync {
    fn notify(&self, work: &BackgroundWork);
}

/// Pick the final text out of the JSON (the sub-agent output file's content is the body of the model-visible `content` list).
pub fn result_text_from_file(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .filter(|text| !text.trim().is_empty())
}

/// The status word used by unit tests and projections.
pub fn status_word(status: &str) -> &'static str {
    notification_status(status)
}

/// Read `modelOnly` from the wire value (shared by the tests and the delivery path).
pub fn is_model_only(payload: &Value) -> bool {
    payload
        .get("modelOnly")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn work(task_type: &'static str, status: &'static str) -> BackgroundWork {
        BackgroundWork {
            work_id: "bash-ses-1-1".to_string(),
            command: "npm run dev".to_string(),
            output_path: PathBuf::from("/tmp/background/bash-ses-1-1.log"),
            status,
            started_at: 1,
            ended_at: Some(2),
            exit_code: Some(0),
            error: None,
            pid: None,
            tool_call_id: Some("toolu-1".to_string()),
            task_type,
            description: None,
            notified: false,
            cancel: None,
        }
    }

    #[test]
    fn bash_notification_matches_the_ts_shape() {
        let text =
            bash_notification_text(&work(crate::background::TASK_TYPE_LOCAL_BASH, BG_COMPLETED));
        assert_eq!(
            text,
            "<task-notification>\n\
             <task-id>bash-ses-1-1</task-id>\n\
             <tool-use-id>toolu-1</tool-use-id>\n\
             <output-file>/tmp/background/bash-ses-1-1.log</output-file>\n\
             <status>completed</status>\n\
             <summary>Background command \"npm run dev\" completed (exit code 0)</summary>\n\
             </task-notification>"
        );
    }

    #[test]
    fn failed_and_cancelled_use_the_ts_status_words() {
        let mut failed = work(crate::background::TASK_TYPE_LOCAL_BASH, BG_FAILED);
        failed.exit_code = Some(3);
        let text = bash_notification_text(&failed);
        assert!(text.contains("<status>failed</status>"), "{text}");
        assert!(text.contains("failed with exit code 3"), "{text}");

        let cancelled = work(crate::background::TASK_TYPE_LOCAL_BASH, BG_CANCELLED);
        let text = bash_notification_text(&cancelled);
        assert!(text.contains("<status>killed</status>"), "{text}");
        assert!(text.contains("was stopped"), "{text}");
    }

    #[test]
    fn agent_notification_carries_result_and_error() {
        let mut agent = work(crate::background::TASK_TYPE_LOCAL_AGENT, BG_COMPLETED);
        agent.work_id = "session-9".to_string();
        agent.command = "analyse the repo".to_string();
        agent.description = Some("analyse the repo".to_string());
        let text = agent_notification_text(&agent, Some("found 3 issues"));
        assert!(text.contains("<task-id>session-9</task-id>"), "{text}");
        assert!(
            text.contains("<summary>Agent &quot;analyse the repo&quot; completed</summary>"),
            "{text}"
        );
        assert!(text.contains("<result>found 3 issues</result>"), "{text}");

        let mut failed = agent.clone();
        failed.status = BG_FAILED;
        failed.error = Some("provider 429".to_string());
        let text = agent_notification_text(&failed, None);
        assert!(text.contains("<error>provider 429</error>"), "{text}");
    }

    #[test]
    fn xml_is_escaped() {
        let mut agent = work(crate::background::TASK_TYPE_LOCAL_AGENT, BG_COMPLETED);
        agent.description = Some("a < b & \"c\"".to_string());
        let text = agent_notification_text(&agent, Some("<script>"));
        assert!(!text.contains("<script>"), "{text}");
        assert!(text.contains("a &lt; b &amp; &quot;c&quot;"), "{text}");
        assert!(text.contains("&lt;script&gt;"), "{text}");
    }
}
