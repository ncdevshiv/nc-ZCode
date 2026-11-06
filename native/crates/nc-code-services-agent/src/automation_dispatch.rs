//! The automation scheduling dispatcher: it really delivers the due definitions in automations as session turns,
//! and writes back automation_runs / automations following the "claim → dispatch → settle" state machine.
//!
//! The two TS counterparts that collaborate here:
//! - `packages/desktop/src/scheduler/index.ts`: tick polling with claimDue, misfire skip,
//!   runId generation, success counting/failure backoff;
//! - `dispatchCronRun` in `packages/desktop/src/host/index.ts`: creates the task/session in the target
//!   workspace and sends the first prompt with the automation's model selection and mode; the terminal state is written
//!   back into the run outcome by `trackCronRunOutcome`.
//!
//! The process-boundary difference from TS: in TS the scheduler and the host are two processes, and the scheduler only
//! postMessages the request to main without waiting for the turn; this crate's host is the `AgentRuntime` in the same process,
//! so the dispatch completes the whole "create session → send prompt → observe the terminal state" chain right here. Terminal observation
//! goes through the phase of the `v4` session projection (the same source of truth as host's `finish_turn` write point), and never synthesizes completion.
//!
//! `claim_manual_runs` is not ported: TS needs it because "run now" is written as a manual run by the host process and
//! claimed by the scheduler process' polling (a cross-process queue). Rust's [`AutomationDispatcher::run_now`]
//! completes claim → dispatch → settle within the same call, so there is no "inserted but not dispatched" cross-process window,
//! and adding a claiming method would only be dead code.

use std::time::Duration;

use nc_code_service_core::ServiceError;
use nc_code_services_automation::repo::{FailKind, SkipAndRescheduleParams};
use nc_code_services_automation::types::{
    Automation, ModelSelection, RunDispatchStatus, RunOutcome, Trigger,
};
use nc_code_services_automation::{
    compute_automation_next_run_at, AutomationService, Scope,
    ServiceError as AutomationServiceError,
};
use serde_json::{json, Value};

use crate::service::AgentService;

/// The misfire grace: a next_run_at earlier than now by more than this value counts as "a window missed while the machine was shut down/asleep".
/// The value is clearly larger than one normal polling delay yet still covers short hangs (scheduler/index.ts:19-23).
pub const MISFIRE_GRACE_MS: i64 = 5 * 60_000;
/// The skip reason is a cross-platform stable vocabulary (scheduler/index.ts:150); the UI shows "missed window" from it.
pub const MISFIRE_SKIP_REASON: &str = "computer_asleep_or_app_not_running";
/// The polling interval for observing the turn's terminal state (the same order of magnitude as AgentService::wait_command_settled).
pub const TURN_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// The cap for observing the turn's terminal state. A timeout only stops the observation, it does not forge a terminal state (the run stays running).
pub const TURN_OBSERVE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The settlement statistics of one tick (for host logs/test assertions; it is not business state).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickReport {
    /// The number of automations claimed by claim_due.
    pub claimed: usize,
    /// The number that created a session + sent the prompt successfully and already wrote the dispatch ledger.
    pub dispatched: usize,
    /// The number of runs whose trigger window was missed and that were recorded as skipped runs.
    pub skipped: usize,
    /// The number whose dispatch failed and that were settled by backoff or as terminal.
    pub failed: usize,
}

/// The claim result of "run now" (the TS ZCodeAgentRunAutomationNowResult).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunNowStatus {
    /// Claimed and dispatch completed (both the session and the first prompt were accepted by the host).
    Queued { run_id: String, session_id: String },
    /// Rejected by single-flight: this automation already has an in-flight run, so nothing new was created.
    Duplicate,
}

/// The turn terminal-state observation parameters. Tests may shorten the window; production uses the defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnWait {
    pub timeout: Duration,
    pub poll_interval: Duration,
}

impl Default for TurnWait {
    fn default() -> Self {
        Self {
            timeout: TURN_OBSERVE_TIMEOUT,
            poll_interval: TURN_POLL_INTERVAL,
        }
    }
}

/// An accepted session plus the observed (or unobserved) terminal state.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalOutcome {
    outcome: RunOutcome,
    error: Option<String>,
}

/// A pre-dispatch failure; `kind` decides whether mark_dispatch_failed backs off or settles terminally.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DispatchFailure {
    message: String,
    kind: FailKind,
}

impl DispatchFailure {
    fn transient(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: FailKind::Transient,
        }
    }

    fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: FailKind::Permanent,
        }
    }
}

/// The scheduling dispatcher. It borrows `AgentService` (the automation service and runtime of the same instance),
/// so it copies no repo and introduces no second state owner.
pub struct AutomationDispatcher<'a> {
    agent: &'a AgentService,
    automation: &'a AutomationService,
    wait: TurnWait,
}

impl<'a> AutomationDispatcher<'a> {
    pub fn new(agent: &'a AgentService) -> Self {
        Self {
            agent,
            automation: agent.automation(),
            wait: TurnWait::default(),
        }
    }

    /// Override the terminal-state observation window (for tests; production keeps the default).
    pub fn with_turn_wait(mut self, wait: TurnWait) -> Self {
        self.wait = wait;
        self
    }

    /// One poll: claim the due items → misfire skip / dispatch → settle.
    ///
    /// A dispatch failure of one automation is written to the database per its backoff and the other items of the same
    /// batch keep being processed; only a repo write failure (DB unavailable) aborts the whole round — the same error boundary as the TS
    /// tick. When aborted, the claimed but unsettled running=1 rows are reclaimed by CLAIM_STALE_MS, so no task is lost forever.
    pub async fn tick_once(&self, now_ms: i64) -> Result<TickReport, ServiceError> {
        let claimed = self
            .automation
            .repo()
            .claim_due(now_ms)
            .map_err(repo_error)?;
        let mut report = TickReport {
            claimed: claimed.len(),
            ..Default::default()
        };
        for automation in claimed {
            self.handle_claimed(&automation, now_ms, &mut report)
                .await?;
        }
        Ok(report)
    }

    /// Run now: claim a manual run → create session + send prompt → write the terminal state back.
    ///
    /// A manual run does not advance cron / maxRuns / lifecycle (the same note as TS automationService.runNow),
    /// and the single-flight claim releases after the real terminal state; a dispatch failure releases immediately so the claim does not hang.
    pub async fn run_now(
        &self,
        automation_id: &str,
        scope: &Scope,
    ) -> Result<RunNowStatus, ServiceError> {
        let Some((automation, run)) = self
            .automation
            .run_now(automation_id, scope)
            .map_err(automation_error)?
        else {
            return Ok(RunNowStatus::Duplicate);
        };
        let workspace_key = automation.workspace_key.clone();
        let selection = match self.resolve_run_model_selection(&run.run_id, &automation) {
            Ok(selection) => selection,
            Err(error) => {
                // The claim already stands; a ledger read/write failure must release single-flight as well, otherwise
                // only CLAIM_STALE reclamation can free it.
                self.settle_manual_failure(&automation, &run.run_id, &error.message)?;
                return Err(error);
            }
        };
        match self
            .start_dispatch(&automation, &run.run_id, selection.as_ref())
            .await
        {
            Ok(session_id) => {
                let dispatched_at = now_ms_i64();
                self.record_accepted(&run.run_id, &session_id, dispatched_at, true)?;
                let terminal = self.await_turn_terminal(&workspace_key, &session_id).await;
                let observed_terminal = terminal.is_some();
                self.record_terminal(&run.run_id, terminal)?;
                if observed_terminal {
                    // TS settleCronRunTerminalOutcome: a manual run releases its claim only after a real terminal state,
                    // so that single-flight covers the whole queued + turn-execution window.
                    self.automation
                        .repo()
                        .release_manual_claim(&automation.automation_id, &automation.workspace_key)
                        .map_err(repo_error)?;
                } else {
                    // An observation timeout is not the same as the turn ending: keep the claim and let stale reclamation be the fallback,
                    // to avoid dispatching two manual runs concurrently for the same automation.
                    eprintln!(
                        "[nc-code-automation] manual claim kept after observation timeout \
                         automation={} run={} session={}",
                        automation.automation_id, run.run_id, session_id
                    );
                }
                Ok(RunNowStatus::Queued {
                    run_id: run.run_id,
                    session_id,
                })
            }
            Err(failure) => {
                self.settle_manual_failure(&automation, &run.run_id, &failure.message)?;
                Err(ServiceError::named(
                    "AutomationDispatchFailed",
                    failure.message,
                ))
            }
        }
    }

    /// The settlement of a manual dispatch failure: writes only the run ledger and releases the claim; it does not change the automation's cron dispatch state
    /// (TS settleManualDispatchFailureBestEffort).
    fn settle_manual_failure(
        &self,
        automation: &Automation,
        run_id: &str,
        message: &str,
    ) -> Result<(), ServiceError> {
        let repo = self.automation.repo();
        repo.mark_run_dispatch(
            run_id,
            RunDispatchStatus::FailedToDispatch,
            None,
            Some(message),
        )
        .map_err(repo_error)?;
        repo.mark_run_outcome(run_id, RunOutcome::Failed, Some(message))
            .map_err(repo_error)?;
        repo.release_manual_claim(&automation.automation_id, &automation.workspace_key)
            .map_err(repo_error)?;
        eprintln!(
            "[nc-code-automation] manual dispatch failed automation={} run={} error={}",
            automation.automation_id, run_id, message
        );
        Ok(())
    }

    async fn handle_claimed(
        &self,
        automation: &Automation,
        now_ms: i64,
        report: &mut TickReport,
    ) -> Result<(), ServiceError> {
        let scheduled_at = resolve_scheduled_at(automation, now_ms);
        let run_id = build_run_id(&automation.automation_id, scheduled_at);
        // A retry reuses the same scheduledAt (next_run_at stays unchanged during backoff), so misfire
        // applies only to the first round; otherwise a normal retry point while backing off would be misjudged as "a missed window".
        let is_retry = automation.dispatch_attempts > 0;
        let missed = !is_retry
            && automation
                .next_run_at
                .is_some_and(|next| next <= now_ms - MISFIRE_GRACE_MS);
        if missed {
            let finalize = is_one_shot(automation);
            let next_run_at = if finalize {
                None
            } else {
                compute_automation_next_run_at(
                    &automation.cron_expr,
                    automation.schedule_rule.as_ref(),
                    now_ms,
                )
            };
            self.automation
                .repo()
                .skip_and_reschedule(&SkipAndRescheduleParams {
                    automation_id: &automation.automation_id,
                    run_id: &run_id,
                    workspace_key: &automation.workspace_key,
                    scheduled_at: Some(scheduled_at),
                    reason: MISFIRE_SKIP_REASON,
                    next_run_at,
                    finalize,
                })
                .map_err(repo_error)?;
            eprintln!(
                "[nc-code-automation] skip missed window automation={} scheduledAt={}{}",
                automation.automation_id,
                scheduled_at,
                if finalize { " finalized=one-shot" } else { "" }
            );
            report.skipped += 1;
            return Ok(());
        }

        // Land/update the run ledger (claimed) first, then dispatch. model_selection is fixed at the first write
        // (the upsert COALESCE semantics match TS fixRunModelSelection), so a backoff retry does
        // not swap the model.
        self.automation
            .repo()
            .upsert_run_claimed(
                &run_id,
                &automation.automation_id,
                &automation.workspace_key,
                Some(scheduled_at),
                Trigger::Schedule,
                automation.model_selection.as_ref(),
            )
            .map_err(repo_error)?;

        // The first dispatch pins the model selection in the run ledger; a backoff retry reads the same value back, so a single
        // run never changes model because of an account/default-model change (the fixRunModelSelection semantics).
        let selection = self.resolve_run_model_selection(&run_id, automation)?;
        match self
            .start_dispatch(automation, &run_id, selection.as_ref())
            .await
        {
            Ok(session_id) => {
                let dispatched_at = now_ms_i64();
                self.record_accepted(&run_id, &session_id, dispatched_at, false)?;
                let next_run_at = compute_automation_next_run_at(
                    &automation.cron_expr,
                    automation.schedule_rule.as_ref(),
                    dispatched_at,
                );
                // mark_dispatched resets running, clears the backoff and advances run_count; a finite task that
                // reaches maxRuns turns completed (next_run_at passed as None lets the repo settle it).
                self.automation
                    .repo()
                    .mark_dispatched(&automation.automation_id, dispatched_at, next_run_at)
                    .map_err(repo_error)?;
                eprintln!(
                    "[nc-code-automation] dispatched automation={} run={} session={}",
                    automation.automation_id, run_id, session_id
                );
                let terminal = self
                    .await_turn_terminal(&automation.workspace_key, &session_id)
                    .await;
                self.record_terminal(&run_id, terminal)?;
                report.dispatched += 1;
            }
            Err(failure) => {
                let failed_at = now_ms_i64();
                self.automation
                    .repo()
                    .mark_run_dispatch(
                        &run_id,
                        RunDispatchStatus::FailedToDispatch,
                        None,
                        Some(&failure.message),
                    )
                    .map_err(repo_error)?;
                self.automation
                    .repo()
                    .mark_run_outcome(&run_id, RunOutcome::Failed, Some(&failure.message))
                    .map_err(repo_error)?;
                let next_run_at = compute_automation_next_run_at(
                    &automation.cron_expr,
                    automation.schedule_rule.as_ref(),
                    failed_at,
                );
                self.automation
                    .repo()
                    .mark_dispatch_failed(
                        &automation.automation_id,
                        failed_at,
                        &failure.message,
                        failure.kind,
                        next_run_at,
                    )
                    .map_err(repo_error)?;
                eprintln!(
                    "[nc-code-automation] dispatch failed automation={} run={} kind={:?} error={}",
                    automation.automation_id, run_id, failure.kind, failure.message
                );
                report.failed += 1;
            }
        }
        Ok(())
    }

    /// Create the session (the automation's workspace / model selection / mode) and send the first prompt.
    /// Returns the sessionId confirmed by the host; the dispatch counts as successful once the prompt is accepted (the turn's terminal state is observed separately).
    async fn start_dispatch(
        &self,
        automation: &Automation,
        run_id: &str,
        model_selection: Option<&ModelSelection>,
    ) -> Result<String, DispatchFailure> {
        // A missing workspace is a deterministic configuration error: retrying will not make the directory appear. It is checked before the runtime is created,
        // to avoid creating a host runtime for a directory that does not exist (TS dispatchCronRun fails before createTask).
        if !std::path::Path::new(&automation.workspace_path).is_dir() {
            return Err(DispatchFailure::permanent(format!(
                "automation workspace does not exist: {}",
                automation.workspace_path
            )));
        }
        let mut create = json!({ "workspacePath": automation.workspace_path });
        let mut send = json!({
            "workspacePath": automation.workspace_path,
            "content": automation.prompt,
            // traceId=runId: the same prompt trace identity as TS dispatchCronRun's.
            "inputId": run_id,
        });
        if let Some(identity) = &automation.workspace_identity {
            create["workspaceIdentity"] = json!(identity);
            send["workspaceIdentity"] = json!(identity);
        }
        if let Some(selection) = model_selection {
            create["model"] = serde_json::to_value(selection).map_err(|error| {
                DispatchFailure::permanent(format!("invalid modelSelection: {error}"))
            })?;
        }
        // An automation bound to a session appends to the original session (the targetTaskId branch of TS dispatchCronRun:
        // resumeTask + applyCronRunConfigToExistingTask), rather than opening a new conversation.
        let session_id = if let Some(target) = &automation.target_task_id {
            let mut resume = json!({
                "workspacePath": automation.workspace_path,
                "sessionId": target,
            });
            if let Some(identity) = &automation.workspace_identity {
                resume["workspaceIdentity"] = json!(identity);
            }
            if let Some(selection) = model_selection {
                resume["model"] = serde_json::to_value(selection).map_err(|error| {
                    DispatchFailure::permanent(format!("invalid modelSelection: {error}"))
                })?;
            }
            self.agent.resume_session(&resume).await.map_err(|error| {
                DispatchFailure::transient(format!("resumeSession({target}) failed: {error}"))
            })?;
            target.clone()
        } else {
            // create_session is responsible for creating the session + subscribing + applying the model + returning the real snapshot.
            let snapshot = self.agent.create_session(&create).await.map_err(|error| {
                DispatchFailure::transient(format!("createSession failed: {error}"))
            })?;
            snapshot
                .get("session")
                .and_then(|session| session.get("sessionId"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    DispatchFailure::transient("createSession snapshot missing sessionId")
                })?
                .to_string()
        };
        if let Some(mode) = &automation.mode {
            // mode must be sent separately after createSession: create_session's
            // apply_session_config reads baseRevision only once, and sending
            // switchCollaborationMode within the same session creation would be rejected by the host as proto.staleRevision.
            // set_model_config takes the revision from a fresh session projection every time (the only reading for a CAS command).
            let mut set_mode = json!({
                "workspacePath": automation.workspace_path,
                "sessionId": session_id,
                "mode": mode,
            });
            if let Some(identity) = &automation.workspace_identity {
                set_mode["workspaceIdentity"] = json!(identity);
            }
            self.agent
                .set_model_config(&set_mode, "switchCollaborationMode")
                .await
                .map_err(|error| {
                    DispatchFailure::transient(format!("switchCollaborationMode failed: {error}"))
                })?;
        }
        send["sessionId"] = json!(session_id);
        // The same v4 sendText path as the channel's sendPrompt member; the service method is called directly here,
        // avoiding the recursive async call chain call_inner → automation_channel → dispatcher.
        self.agent
            .send_prompt(&send)
            .await
            .map_err(|error| DispatchFailure::transient(format!("sendPrompt failed: {error}")))?;
        Ok(session_id)
    }

    /// Fix (read back) the model selection of this run. When the automation specifies no selection, nothing is written to the ledger and no
    /// workspace default is forged — the Rust side has no model-selection registry to resolve.
    fn resolve_run_model_selection(
        &self,
        run_id: &str,
        automation: &Automation,
    ) -> Result<Option<ModelSelection>, ServiceError> {
        let Some(selection) = automation.model_selection.as_ref() else {
            return Ok(None);
        };
        self.automation
            .repo()
            .fix_run_model_selection(run_id, selection)
            .map_err(repo_error)
    }

    /// The dispatch was accepted by the host: writes the run ledger (dispatched + running), and manual additionally accumulates the count.
    ///
    /// manual goes through `mark_manual_run_dispatched` (it writes dispatched + sessionId itself), and `mark_run_dispatch` must not
    /// be written first — the latter would set dispatch_status to dispatched prematurely, making
    /// mark_manual_run_dispatched's idempotency boundary misjudge it as "already settled" and lose the run_count
    /// (the TS scheduler's settleDispatchResult likewise calls only this one entry for the manual branch).
    fn record_accepted(
        &self,
        run_id: &str,
        session_id: &str,
        dispatched_at: i64,
        manual: bool,
    ) -> Result<(), ServiceError> {
        let repo = self.automation.repo();
        repo.mark_run_outcome(run_id, RunOutcome::Running, None)
            .map_err(repo_error)?;
        if manual {
            repo.mark_manual_run_dispatched(run_id, Some(session_id), dispatched_at)
                .map_err(repo_error)?;
        } else {
            repo.mark_run_dispatch(
                run_id,
                RunDispatchStatus::Dispatched,
                Some(session_id),
                None,
            )
            .map_err(repo_error)?;
        }
        Ok(())
    }

    /// Observe the turn's terminal state: poll the phase of the host's session projection. A timeout returns None (the run stays running),
    /// and never synthesizes succeeded/failed.
    async fn await_turn_terminal(
        &self,
        workspace_key: &str,
        session_id: &str,
    ) -> Option<TerminalOutcome> {
        let deadline = tokio::time::Instant::now() + self.wait.timeout;
        loop {
            if let Some(view) = session_view(self.agent, workspace_key, session_id).await {
                match view.get("phase").and_then(Value::as_str) {
                    Some("completedSuccess") => {
                        return Some(TerminalOutcome {
                            outcome: RunOutcome::Succeeded,
                            error: None,
                        })
                    }
                    Some("completedInterrupted") => {
                        return Some(TerminalOutcome {
                            outcome: RunOutcome::Stopped,
                            error: last_error_message(&view),
                        })
                    }
                    Some("error") => {
                        return Some(TerminalOutcome {
                            outcome: RunOutcome::Failed,
                            error: Some(
                                last_error_message(&view)
                                    .unwrap_or_else(|| "automation turn failed".to_string()),
                            ),
                        })
                    }
                    _ => {}
                }
            }
            if tokio::time::Instant::now() >= deadline {
                eprintln!(
                    "[nc-code-automation] turn observation timed out session={} after {:?}; \
                     run outcome stays running",
                    session_id, self.wait.timeout
                );
                return None;
            }
            tokio::time::sleep(self.wait.poll_interval).await;
        }
    }

    fn record_terminal(
        &self,
        run_id: &str,
        terminal: Option<TerminalOutcome>,
    ) -> Result<(), ServiceError> {
        let Some(terminal) = terminal else {
            return Ok(());
        };
        self.automation
            .repo()
            .mark_run_outcome(run_id, terminal.outcome, terminal.error.as_deref())
            .map_err(repo_error)
    }
}

/// The dispatch timestamp: next_run_at takes priority (unchanged during backoff, which keeps the runId stable), falling back to retry_at / now
/// (scheduler/index.ts:78-82 resolveScheduledAt).
fn resolve_scheduled_at(automation: &Automation, now_ms: i64) -> i64 {
    automation
        .next_run_at
        .or(automation.retry_at)
        .unwrap_or(now_ms)
}

/// runId = `${automationId}:${scheduledAt}` (scheduler/index.ts:84-86).
fn build_run_id(automation_id: &str, scheduled_at: i64) -> String {
    format!("{automation_id}:{scheduled_at}")
}

/// A purely one-shot task: non-recurring and executed at most once (automationCron.ts:44-51).
fn is_one_shot(automation: &Automation) -> bool {
    !automation.recurring && automation.max_runs.unwrap_or(1) <= 1
}

async fn session_view(
    agent: &AgentService,
    workspace_key: &str,
    session_id: &str,
) -> Option<Value> {
    let runtime = agent.runtime_for(workspace_key).await?;
    runtime.session_view(session_id)
}

fn last_error_message(view: &Value) -> Option<String> {
    view.get("lastError")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn repo_error(message: String) -> ServiceError {
    ServiceError::named("AutomationRepoError", message)
}

fn automation_error(error: AutomationServiceError) -> ServiceError {
    ServiceError::new(error.to_string())
}

fn now_ms_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use nc_code_rpc::WireValue;
    use nc_code_service_core::AppPaths;
    use nc_code_services_automation::repo::{UpdateOptions, UpdateParams};
    use nc_code_services_automation::types::{LifecycleStatus, ModelSelection, RunDispatchStatus};
    use nc_code_services_automation::CreateInput;
    use nc_code_services_provider::store::{
        default_model_config, ModelRecord, ProviderRecord, ProviderStore,
    };
    use serde_json::json;

    /// An openai-compatible streaming reply: one body segment + stop + [DONE]. The same technique as the
    /// scripted provider in nc-code-host/tests/common (real HTTP + SSE).
    const OPENAI_TEXT_SSE: &str = concat!(
        "data: {\"id\":\"chatcmpl-native\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Automation complete.\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-native\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,\"total_tokens\":13}}\n\n",
        "data: [DONE]\n\n",
    );

    /// The scripted provider double: records every request body and replies with the same fixed SSE.
    struct ScriptedProvider {
        bodies: Arc<Mutex<Vec<String>>>,
        requests: Arc<AtomicUsize>,
    }

    impl ScriptedProvider {
        fn spawn() -> (Self, u16) {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let requests = Arc::new(AtomicUsize::new(0));
            let bodies_for_thread = Arc::clone(&bodies);
            let requests_for_thread = Arc::clone(&requests);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let bodies = Arc::clone(&bodies_for_thread);
                    let requests = Arc::clone(&requests_for_thread);
                    std::thread::spawn(move || handle_connection(&mut stream, bodies, requests));
                }
            });
            (Self { bodies, requests }, port)
        }

        fn request_bodies(&self) -> Vec<String> {
            self.bodies
                .lock()
                .map(|bodies| bodies.clone())
                .unwrap_or_default()
        }

        fn request_count(&self) -> usize {
            self.requests.load(Ordering::SeqCst)
        }
    }

    fn handle_connection(
        stream: &mut TcpStream,
        bodies: Arc<Mutex<Vec<String>>>,
        requests: Arc<AtomicUsize>,
    ) {
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        loop {
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                return;
            }
            let mut content_length = 0usize;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 {
                    return;
                }
                let trimmed = header.trim();
                if trimmed.is_empty() {
                    break;
                }
                if let Some((name, value)) = trimmed.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        content_length = value.trim().parse().unwrap_or(0);
                    }
                }
            }
            let mut body = vec![0u8; content_length];
            if reader.read_exact(&mut body).is_err() {
                return;
            }
            bodies
                .lock()
                .expect("bodies lock")
                .push(String::from_utf8_lossy(&body).to_string());
            requests.fetch_add(1, Ordering::SeqCst);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n{}",
                OPENAI_TEXT_SSE.len(),
                OPENAI_TEXT_SSE
            );
            if stream.write_all(response.as_bytes()).is_err() {
                return;
            }
            let _ = stream.flush();
        }
    }

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "zcode-automation-dispatch-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).expect("mkdir");
        root
    }

    async fn service_with_provider(tag: &str, port: u16) -> (Arc<AgentService>, PathBuf) {
        let root = temp_root(tag);
        let paths = AppPaths::new(root.join("home"), root.join("data"));
        let store = ProviderStore::new(paths.clone());
        let base_url = format!("http://127.0.0.1:{port}/v1");
        store
            .mutate(|snapshot| {
                snapshot.providers.push(ProviderRecord {
                    provider_id: "personal:1".to_string(),
                    provider_name: "Test Provider".to_string(),
                    template_id: None,
                    enabled: true,
                    config: json!({
                        "group": "standard-personal",
                        "visibility": "visible",
                        "api": {"type": "openai-chat-completions", "baseUrl": base_url},
                        "access": {"type": "api-key", "apiKey": "test-key"}
                    }),
                    // The first enabled model is the model the host turn actually calls; the second one is only used
                    // to assert that the automation's modelSelection got written into the session.
                    models: vec![
                        ModelRecord {
                            model_id: "test-model".to_string(),
                            config: default_model_config(),
                            enabled: true,
                            use_recommended: true,
                        },
                        ModelRecord {
                            model_id: "automation-model".to_string(),
                            config: default_model_config(),
                            enabled: true,
                            use_recommended: true,
                        },
                    ],
                });
                snapshot.provider_order.push("personal:1".to_string());
                Ok(())
            })
            .await
            .expect("store");
        (AgentService::new(paths, store), root)
    }

    async fn service_without_provider(tag: &str) -> (Arc<AgentService>, PathBuf) {
        let root = temp_root(tag);
        let paths = AppPaths::new(root.join("home"), root.join("data"));
        (
            AgentService::new(paths.clone(), ProviderStore::new(paths)),
            root,
        )
    }

    fn create_automation(
        service: &AgentService,
        workspace: &Path,
        prompt: &str,
        recurring: bool,
        model_selection: Option<ModelSelection>,
        mode: Option<&str>,
    ) -> Automation {
        let workspace_path = workspace.to_string_lossy().to_string();
        service
            .automation()
            .create(
                &CreateInput {
                    title: "scheduled report".to_string(),
                    cron_expr: "30 9 * * *".to_string(),
                    prompt: prompt.to_string(),
                    model_selection,
                    mode: mode.map(str::to_string),
                    workspace_path: workspace_path.clone(),
                    recurring,
                    ..Default::default()
                },
                &Scope {
                    workspace_path: Some(workspace_path),
                    workspace_identity: None,
                },
            )
            .expect("create automation")
    }

    fn set_next_run_at(service: &AgentService, automation: &Automation, next_run_at: i64) {
        let repo = service.automation().repo();
        repo.update(
            &automation.automation_id,
            &UpdateParams::default(),
            &UpdateOptions {
                next_run_at: Some(Some(next_run_at)),
                ..Default::default()
            },
            Some(&automation.workspace_key),
        )
        .expect("update next_run_at");
    }

    fn get_automation(service: &AgentService, automation_id: &str) -> Automation {
        service
            .automation()
            .repo()
            .get(automation_id, None)
            .expect("get")
            .expect("automation exists")
    }

    fn run_rows(service: &AgentService, automation_id: &str) -> Vec<AutomationRunRow> {
        service
            .automation()
            .repo()
            .list_runs(automation_id, None)
            .expect("list runs")
    }

    type AutomationRunRow = nc_code_services_automation::types::AutomationRun;

    fn now_ms() -> i64 {
        super::now_ms_i64()
    }

    fn wait_policy() -> TurnWait {
        TurnWait {
            // The scripted provider replies immediately; the cap only guards against the test hanging.
            timeout: Duration::from_secs(20),
            poll_interval: Duration::from_millis(10),
        }
    }

    #[tokio::test]
    async fn due_automation_is_claimed_and_dispatched_exactly_once() {
        let (provider, port) = ScriptedProvider::spawn();
        let (service, root) = service_with_provider("once", port).await;
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let selection = ModelSelection {
            provider_id: "personal:1".to_string(),
            model_id: "automation-model".to_string(),
            options: None,
        };
        let automation = create_automation(
            &service,
            &workspace,
            "run the thing",
            true,
            Some(selection.clone()),
            Some("plan"),
        );
        let now = now_ms();
        set_next_run_at(&service, &automation, now - 1_000);

        let dispatcher = AutomationDispatcher::new(&service).with_turn_wait(wait_policy());
        let report = dispatcher.tick_once(now).await.expect("tick");
        assert_eq!(report.claimed, 1);
        assert_eq!(report.dispatched, 1);
        assert_eq!(report.skipped, 0);
        assert_eq!(report.failed, 0);

        let after = get_automation(&service, &automation.automation_id);
        assert_eq!(after.run_count, 1, "Successful distribution counts once");
        assert_eq!(
            after.dispatch_status,
            nc_code_services_automation::DispatchStatus::Dispatched
        );
        assert_eq!(after.dispatch_attempts, 0);
        assert_eq!(after.retry_at, None);
        assert_eq!(after.lifecycle_status, LifecycleStatus::Active);
        assert!(
            after.next_run_at.is_some_and(|next| next > now),
            "The recurring task must move forward to the next future trigger point"
        );
        assert!(after.last_run_at.is_some());

        let runs = run_rows(&service, &automation.automation_id);
        assert_eq!(runs.len(), 1);
        let run = &runs[0];
        assert_eq!(run.trigger, Trigger::Schedule);
        assert_eq!(run.dispatch_status, RunDispatchStatus::Dispatched);
        assert_eq!(run.outcome, Some(RunOutcome::Succeeded));
        assert!(
            run.session_id.as_deref().is_some_and(|id| !id.is_empty()),
            "The run ledger must backfill the real sessionId"
        );
        assert_eq!(
            run.run_id,
            format!("{}:{}", automation.automation_id, now - 1_000),
            "runId = automationId:scheduledAt"
        );
        assert_eq!(run.model_selection, Some(selection.clone()));

        let (_, key) = AgentService::workspace_of(&json!({
            "workspacePath": workspace.to_string_lossy()
        }))
        .expect("key");
        let view = service
            .runtime_for(&key)
            .await
            .expect("runtime")
            .session_view(run.session_id.as_deref().expect("session id"))
            .expect("session view");
        assert_eq!(
            view["model"],
            json!("automation-model"),
            "Automation model selection is implemented"
        );
        assert_eq!(view["mode"], json!("plan"), "automation mode implemented");
        assert_eq!(view["phase"], json!("completedSuccess"));

        let bodies = provider.request_bodies();
        assert_eq!(bodies.len(), 1, "The model is only adjusted once per turn");
        assert!(
            bodies[0].contains("run the thing"),
            "The automation prompt must be delivered to the model as user input: {}",
            bodies[0]
        );

        // The second tick: next_run_at is already ahead, so it must not be claimed/dispatched again.
        let second = dispatcher.tick_once(now).await.expect("second tick");
        assert_eq!(second.claimed, 0, "The settled automation is no longer due");
        assert_eq!(second.dispatched, 0);
        assert_eq!(run_rows(&service, &automation.automation_id).len(), 1);
        assert_eq!(
            provider.request_count(),
            1,
            "Turn is not allowed on the second tick"
        );
    }

    #[tokio::test]
    async fn one_shot_schedule_finalizes_after_a_successful_dispatch() {
        let (provider, port) = ScriptedProvider::spawn();
        let (service, root) = service_with_provider("one-shot-success", port).await;
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let automation = create_automation(&service, &workspace, "only once", false, None, None);
        let now = now_ms();
        set_next_run_at(&service, &automation, now - 1_000);

        let dispatcher = AutomationDispatcher::new(&service).with_turn_wait(wait_policy());
        let report = dispatcher.tick_once(now).await.expect("tick");
        assert_eq!(report.dispatched, 1);

        let after = get_automation(&service, &automation.automation_id);
        assert_eq!(after.run_count, 1);
        assert_eq!(
            after.lifecycle_status,
            LifecycleStatus::Completed,
            "After a one-time task is successfully dispatched, it must be transferred to completed"
        );
        assert!(!after.enabled);
        assert_eq!(after.next_run_at, None);
        assert_eq!(provider.request_count(), 1);

        let second = dispatcher.tick_once(now).await.expect("second tick");
        assert_eq!(
            second.claimed, 0,
            "The one-time task in the final state is no longer claimed"
        );
    }

    #[tokio::test]
    async fn transient_dispatch_failure_backs_off_without_losing_the_automation() {
        let (service, root) = service_without_provider("transient").await;
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let automation = create_automation(&service, &workspace, "never runs", true, None, None);
        let now = now_ms();
        set_next_run_at(&service, &automation, now - 1_000);

        let dispatcher = AutomationDispatcher::new(&service).with_turn_wait(wait_policy());
        let report = dispatcher.tick_once(now).await.expect("tick");
        assert_eq!(report.claimed, 1);
        assert_eq!(report.failed, 1);
        assert_eq!(report.dispatched, 0);

        let after = get_automation(&service, &automation.automation_id);
        assert_eq!(
            after.dispatch_status,
            nc_code_services_automation::DispatchStatus::FailedToDispatch
        );
        assert_eq!(after.dispatch_attempts, 1);
        assert_eq!(
            after.next_run_at,
            Some(now - 1_000),
            "nextRunAt remains unchanged during backoff"
        );
        assert_eq!(after.lifecycle_status, LifecycleStatus::Active);
        assert!(
            after.enabled,
            "automation must not be disabled if transient fails"
        );

        let runs = run_rows(&service, &automation.automation_id);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].dispatch_status, RunDispatchStatus::FailedToDispatch);
        assert_eq!(runs[0].outcome, Some(RunOutcome::Failed));
        assert!(runs[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("Provider not configured")));

        // The millisecond granularity of std::time makes failedAt slightly larger than the test's now; retry_at lands in the
        // [now+30s, now+31s] range.
        let retry_at = after.retry_at.expect("retry_at");
        assert!(retry_at >= now + 30_000 && retry_at <= now + 31_000);

        // No duplicate claim during the backoff window; once the time arrives it retries and the backoff grows exponentially (30s → 60s).
        let second = dispatcher.tick_once(now).await.expect("second tick");
        assert_eq!(second.claimed, 0);
        // retry_at is written by the repo from the real clock (the tick argument only decides the claim judgment), so the 60s backoff is
        // asserted against the real clock at the moment "the retry happens".
        let before_retry = now_ms();
        let retried = dispatcher.tick_once(retry_at).await.expect("retry tick");
        assert_eq!(retried.claimed, 1);
        let after_retry = get_automation(&service, &automation.automation_id);
        assert_eq!(after_retry.dispatch_attempts, 2);
        let retry_after_backoff = after_retry.retry_at.expect("retry_at after backoff");
        assert!(
            retry_after_backoff >= before_retry + 60_000&& retry_after_backoff <= before_retry + 61_000,
            "The second backoff must be enlarged to 60s, actual {retry_after_backoff} vs {before_retry}"
        );
        assert!(retry_after_backoff > retry_at);
        assert_eq!(
            run_rows(&service, &automation.automation_id).len(),
            1,
            "Retry to reuse the same runId (without creating a new account)"
        );
    }

    #[tokio::test]
    async fn misfire_skips_and_reschedules_recurring_and_finalizes_one_shot() {
        let (service, root) = service_without_provider("misfire").await;
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let recurring = create_automation(&service, &workspace, "recurring", true, None, None);
        let one_shot = create_automation(&service, &workspace, "one shot", false, None, None);
        let now = now_ms();
        let missed_at = now - 10 * 60_000;
        set_next_run_at(&service, &recurring, missed_at);
        set_next_run_at(&service, &one_shot, missed_at);

        let dispatcher = AutomationDispatcher::new(&service).with_turn_wait(wait_policy());
        let report = dispatcher.tick_once(now).await.expect("tick");
        assert_eq!(report.claimed, 2);
        assert_eq!(report.skipped, 2);
        assert_eq!(report.dispatched, 0);
        assert_eq!(report.failed, 0);

        let recurring_after = get_automation(&service, &recurring.automation_id);
        assert_eq!(
            recurring_after.run_count, 0,
            "Skip does not count run_count"
        );
        assert_eq!(
            recurring_after.dispatch_attempts, 0,
            "Skipping does not count as failure"
        );
        assert_eq!(
            recurring_after.next_run_at.map(|next| next > now),
            Some(true)
        );
        assert_eq!(recurring_after.lifecycle_status, LifecycleStatus::Active);
        assert!(recurring_after.enabled);
        let one_shot_after = get_automation(&service, &one_shot.automation_id);
        assert_eq!(one_shot_after.lifecycle_status, LifecycleStatus::Completed);
        assert!(
            !one_shot_after.enabled,
            "One-time task misses the window and is the final state"
        );
        assert_eq!(one_shot_after.next_run_at, None);
        assert_eq!(one_shot_after.run_count, 0);

        for automation in [&recurring, &one_shot] {
            let runs = run_rows(&service, &automation.automation_id);
            assert_eq!(runs.len(), 1);
            assert_eq!(runs[0].dispatch_status, RunDispatchStatus::Skipped);
            assert_eq!(runs[0].error.as_deref(), Some(MISFIRE_SKIP_REASON));
            assert_eq!(runs[0].outcome, None, "Skipping is not the result of a run");
            assert_eq!(
                runs[0].run_id,
                format!("{}:{}", automation.automation_id, missed_at)
            );
        }

        // The next tick after a skip must not claim again (the recurring one moved ahead, the one-shot is disabled).
        let second = dispatcher.tick_once(now).await.expect("second tick");
        assert_eq!(second.claimed, 0);
    }

    #[tokio::test]
    async fn run_automation_now_dispatches_and_records_the_run_outcome() {
        let (provider, port) = ScriptedProvider::spawn();
        let (service, root) = service_with_provider("run-now", port).await;
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let selection = ModelSelection {
            provider_id: "personal:1".to_string(),
            model_id: "automation-model".to_string(),
            options: None,
        };
        let automation = create_automation(
            &service,
            &workspace,
            "run me now",
            true,
            Some(selection.clone()),
            Some("plan"),
        );
        let scheduled_before = automation.next_run_at;

        let result = service
            .call_inner(
                "runAutomationNow",
                vec![WireValue::Json(json!({
                    "automationId": automation.automation_id,
                    "workspacePath": workspace.to_string_lossy(),
                }))],
            )
            .await
            .expect("runAutomationNow");
        let WireValue::Json(result) = result else {
            panic!("runAutomationNow must return JSON");
        };
        assert_eq!(result, json!({"status": "queued"}));

        let after = get_automation(&service, &automation.automation_id);
        assert_eq!(
            after.run_count, 1,
            "manual run is included in the cumulative number of times"
        );
        assert!(after.last_run_at.is_some());
        assert_eq!(
            after.next_run_at, scheduled_before,
            "manual does not change the cron plan"
        );
        assert_eq!(after.lifecycle_status, LifecycleStatus::Active);

        let runs = run_rows(&service, &automation.automation_id);
        assert_eq!(runs.len(), 1);
        let run = &runs[0];
        assert_eq!(run.trigger, Trigger::Manual);
        assert!(run.run_id.contains(":manual:"));
        assert_eq!(run.dispatch_status, RunDispatchStatus::Dispatched);
        assert_eq!(run.outcome, Some(RunOutcome::Succeeded));
        assert_eq!(run.model_selection, Some(selection));
        let session_id = run.session_id.clone().expect("session id recorded");

        let (_, key) = AgentService::workspace_of(&json!({
            "workspacePath": workspace.to_string_lossy()
        }))
        .expect("key");
        let view = service
            .runtime_for(&key)
            .await
            .expect("runtime")
            .session_view(&session_id)
            .expect("session view");
        assert_eq!(view["phase"], json!("completedSuccess"));
        assert_eq!(view["model"], json!("automation-model"));
        assert_eq!(view["mode"], json!("plan"));
        assert!(provider.request_bodies()[0].contains("run me now"));

        // single-flight: while an in-flight claim exists the channel returns duplicate and creates no new run.
        let (_, extra_run) = service
            .automation()
            .run_now(
                &automation.automation_id,
                &Scope {
                    workspace_path: Some(workspace.to_string_lossy().to_string()),
                    workspace_identity: None,
                },
            )
            .expect("pre-claim")
            .expect("claimed");
        let duplicate = service
            .call_inner(
                "runAutomationNow",
                vec![WireValue::Json(json!({
                    "automationId": automation.automation_id,
                    "workspacePath": workspace.to_string_lossy(),
                }))],
            )
            .await
            .expect("duplicate is not an error");
        let WireValue::Json(duplicate) = duplicate else {
            panic!("runAutomationNow must return JSON");
        };
        assert_eq!(duplicate, json!({"status": "duplicate"}));
        let listed = run_rows(&service, &automation.automation_id);
        assert_eq!(listed.len(), 2, "duplicate cannot be created new run");
        assert!(listed
            .iter()
            .any(|candidate| candidate.run_id == extra_run.run_id
                && candidate.dispatch_status == RunDispatchStatus::Claimed));
        service
            .automation()
            .repo()
            .release_manual_claim(&automation.automation_id, &automation.workspace_key)
            .expect("release pre-claim");
    }

    #[tokio::test]
    async fn run_now_appends_to_the_bound_target_session() {
        let (provider, port) = ScriptedProvider::spawn();
        let (service, root) = service_with_provider("target", port).await;
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let created = service
            .call_inner(
                "createSession",
                vec![WireValue::Json(json!({
                    "workspacePath": workspace.to_string_lossy(),
                }))],
            )
            .await
            .expect("create target session");
        let WireValue::Json(created) = created else {
            panic!("createSession must return JSON");
        };
        let target = created["session"]["sessionId"]
            .as_str()
            .expect("target session id")
            .to_string();
        let workspace_path = workspace.to_string_lossy().to_string();
        let automation = service
            .automation()
            .create(
                &CreateInput {
                    title: "bound".to_string(),
                    cron_expr: "30 9 * * *".to_string(),
                    prompt: "append to target".to_string(),
                    workspace_path: workspace_path.clone(),
                    target_task_id: Some(target.clone()),
                    recurring: true,
                    ..Default::default()
                },
                &Scope {
                    workspace_path: Some(workspace_path.clone()),
                    workspace_identity: None,
                },
            )
            .expect("create automation");

        let result = service
            .call_inner(
                "runAutomationNow",
                vec![WireValue::Json(json!({
                    "automationId": automation.automation_id,
                    "workspacePath": workspace_path,
                }))],
            )
            .await
            .expect("runAutomationNow");
        assert_eq!(result, WireValue::Json(json!({"status": "queued"})));

        let runs = run_rows(&service, &automation.automation_id);
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].session_id.as_deref(),
            Some(target.as_str()),
            "The task of binding a session must be appended to the original session rather than creating a new session"
        );
        assert_eq!(runs[0].outcome, Some(RunOutcome::Succeeded));
        assert_eq!(provider.request_count(), 1);
        assert!(provider.request_bodies()[0].contains("append to target"));
    }

    #[tokio::test]
    async fn run_now_failure_releases_the_manual_claim() {
        let (service, root) = service_without_provider("run-now-failure").await;
        let missing = root.join("missing-workspace");
        // A workspace that does not exist = a permanent dispatch failure; the directory is not created.
        let automation = create_automation(&service, &missing, "never", true, None, None);

        let error = service
            .call_inner(
                "runAutomationNow",
                vec![WireValue::Json(json!({
                    "automationId": automation.automation_id,
                    "workspacePath": missing.to_string_lossy(),
                }))],
            )
            .await
            .expect_err("missing workspace must fail");
        assert!(error.to_string().contains("does not exist"), "{}", error);

        let runs = run_rows(&service, &automation.automation_id);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].dispatch_status, RunDispatchStatus::FailedToDispatch);
        assert_eq!(runs[0].outcome, Some(RunOutcome::Failed));

        // The claim is released: the repo can claim again (running=1 does not hang).
        let reclaimed = service
            .automation()
            .run_now(
                &automation.automation_id,
                &Scope {
                    workspace_path: Some(missing.to_string_lossy().to_string()),
                    workspace_identity: None,
                },
            )
            .expect("run_now")
            .is_some();
        assert!(reclaimed, "Manual claim must be released if dispatch fails");
        service
            .automation()
            .repo()
            .release_manual_claim(&automation.automation_id, &automation.workspace_key)
            .expect("release");
    }

    #[test]
    fn helpers_match_the_typescript_scheduler_contract() {
        assert_eq!(MISFIRE_GRACE_MS, 5 * 60_000);
        assert_eq!(MISFIRE_SKIP_REASON, "computer_asleep_or_app_not_running");
        assert_eq!(
            build_run_id("automation-1", 1_730_000_000_000),
            "automation-1:1730000000000"
        );

        let mut automation = minimal_automation(true, None);
        assert_eq!(resolve_scheduled_at(&automation, 99), 50);
        assert!(!is_one_shot(&automation));
        automation.recurring = false;
        assert!(
            is_one_shot(&automation),
            "Unexplicit maxRuns are treated as one-shot"
        );
        automation.max_runs = Some(2);
        assert!(!is_one_shot(&automation));
    }

    fn minimal_automation(recurring: bool, max_runs: Option<u32>) -> Automation {
        Automation {
            automation_id: "automation-1".to_string(),
            title: "t".to_string(),
            cron_expr: "30 9 * * *".to_string(),
            prompt: "p".to_string(),
            model_selection: None,
            mode: None,
            workspace_key: "/ws".to_string(),
            workspace_path: "/ws".to_string(),
            workspace_identity: None,
            target_task_id: None,
            location_kind: "local".to_string(),
            recurring,
            max_runs,
            end_at: None,
            schedule_rule: None,
            schedule_edited_by_user: None,
            run_count: 0,
            enabled: true,
            lifecycle_status: LifecycleStatus::Active,
            next_run_at: Some(50),
            last_run_at: None,
            dispatch_status: nc_code_services_automation::DispatchStatus::Idle,
            dispatch_attempts: 0,
            retry_at: None,
            last_error: None,
            created_at: 1,
            updated_at: 1,
        }
    }
}
