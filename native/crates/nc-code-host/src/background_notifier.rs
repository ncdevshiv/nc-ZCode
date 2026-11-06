//! Background-task completion delivery: the missing half of `notifications.rs`.
//!
//! The registry (`background.rs`) settles a work item and calls
//! `notify_settled`; this module turns that into a `backgroundResult` turn in the
//! parent session, which is what lets the model fetch the result with `TaskOutput`
//! (TS `runtime-task/notification.ts` delivery model).
//!
//! Serialization: a notification never runs concurrently with a user turn. It is
//! queued on the session, drained when the session is idle, and the turn finisher
//! schedules another drain. A single `background_drain_active` bit keeps the
//! "notify while idle" and "turn finished" paths from starting two loops.

use std::sync::{Arc, Weak};

use serde_json::Value;

use crate::background::{BackgroundWork, ToolOutputPreviewSink};
use crate::notifications::{notification_text, result_text_from_file, BackgroundNotifier};
use crate::session::Session;
use crate::turn::TurnState;
use crate::HostServer;

/// Delivery point of a running Bash row's `outputPreview` (TS `projectToolActivity`).
/// The background watchdog runs on a plain OS thread, so it talks to the session
/// through this small sink instead of borrowing the host.
pub(crate) struct HostToolPreviewSink {
    host: Weak<HostServer>,
    session_id: String,
}

impl HostToolPreviewSink {
    pub(crate) fn new(host: Weak<HostServer>, session_id: String) -> Self {
        Self { host, session_id }
    }
}

impl ToolOutputPreviewSink for HostToolPreviewSink {
    fn output_preview(&self, turn_id: &str, tool_call_id: &str, preview: Value) {
        let Some(host) = self.host.upgrade() else {
            return;
        };
        let Some(session) = host.session(&self.session_id) else {
            return;
        };
        // `project_tool_preview` already writes through the session's eager frame
        // sink during a turn; the returned lines are a fallback for non-eager mode.
        for line in session.project_tool_preview(turn_id, tool_call_id, preview) {
            let _ = host.outbox.send(line);
        }
    }
}

/// Per-session delivery point installed on the session's `BackgroundRegistry`.
pub(crate) struct HostBackgroundNotifier {
    host: Weak<HostServer>,
    session_id: String,
}

impl HostBackgroundNotifier {
    pub(crate) fn new(host: Weak<HostServer>, session_id: String) -> Self {
        Self { host, session_id }
    }
}

impl BackgroundNotifier for HostBackgroundNotifier {
    fn notify(&self, work: &BackgroundWork) {
        let Some(host) = self.host.upgrade() else {
            return;
        };
        let Some(session) = host.session(&self.session_id) else {
            return;
        };
        session.queue_background_result(work.clone());
        host.schedule_background_result_drain(session);
    }
}

impl HostServer {
    /// Start a drain unless one is already running or a turn owns the session
    /// (the turn finisher calls this again when it settles).
    pub(crate) fn schedule_background_result_drain(self: &Arc<Self>, session: Arc<Session>) {
        if session.is_running() {
            return;
        }
        if !session.begin_background_drain() {
            return;
        }
        let server = Arc::clone(self);
        let drain_session = Arc::clone(&session);
        let task = async move {
            server.drain_background_results(drain_session).await;
        };
        // Background workers are plain OS threads (bash reader / subagent settle),
        // so there is usually no reactor on the delivering thread.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(task);
            return;
        }
        if let Some(handle) = self.runtime.get() {
            handle.spawn(task);
            return;
        }
        // No runtime at all: leave the work queued for the next turn boundary.
        session.end_background_drain();
    }

    /// Consume queued notifications FIFO. Stops when a turn starts running
    /// (a new user turn won), when the queue is empty, or when a notification
    /// turn fails (fail closed instead of retrying the same input forever).
    pub(crate) async fn drain_background_results(self: &Arc<Self>, session: Arc<Session>) {
        loop {
            if session.is_running() {
                break;
            }
            let Some(work) = session.pop_background_result() else {
                break;
            };
            let result = result_text_from_file(&work.output_path);
            let text = notification_text(&work, result.as_deref());
            let turn_id = format!("{}#bg-{}", session.id, work.work_id);
            let source_command_id = format!("background:{}", work.work_id);
            let report = crate::turn::run_background_result_turn(
                Arc::clone(&session),
                self.provider.as_ref(),
                &self.turn_deps,
                &text,
                &turn_id,
                &source_command_id,
            )
            .await;
            for line in report.lines {
                let _ = self.outbox.send(line);
            }
            if report.state == TurnState::Failed {
                break;
            }
        }
        session.end_background_drain();
        // A notification that landed between the last `pop` and `end` would
        // otherwise wait for the next user turn.
        if session.background_results_len() > 0 && !session.is_running() {
            self.schedule_background_result_drain(session);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn basic_test() {
        assert!(true);
    }
}
