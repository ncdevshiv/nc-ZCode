//! hook 运行时：匹配、准入判定、执行与输出解释（`configured-runner*.ts` +
//! `runner.ts` + `output.ts` 的 Rust 版）。
//!
//! 分工：`admission.rs` 回答"这条声明能不能跑"，本模块只回答"怎么跑、输出怎么解释"。
//! 执行的是**用户/仓库声明的外部进程**，所以每条纪律都要写死：
//! - 准入不通过（未信任/被策略阻断/快照不匹配）→ **跳过**，不是"拒绝执行整轮"；
//! - 超时/取消 → 杀进程树，`timed_out=true`，绝不无限等待；
//! - stdout 读取有上限（`resolvedMaxOutputBytes`），超出部分继续排空但不入账；
//! - 退出码语义：0 = 解析 stdout JSON（非 JSON/坏 JSON 视为无输出，不能因为 hook
//!   打印日志就让动作失败）；2 = 阻断（stderr→stdout→默认文案）；其它非零 = hook
//!   进程失败（调用方按 typed fault 处理）。
//!
//! `async` 的 command hook 是 fire-and-forget：返回值只记一条 `background` 调用记录，
//! 输出不参与决策。

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::config::HookEventName;
use super::digest::{BundleSnapshot, CanonicalEntry, EntryType};
use super::display::execution_descriptor;

pub use super::admission::SnapshotEvaluation;

const OUTPUT_POLL_MS: u64 = 10;

/// 一次 hook 事件的输入（`HookInput`）。
#[derive(Debug, Clone, Default)]
pub struct HookInput {
    pub session_id: String,
    pub cwd: String,
    pub mode: String,
    pub tool_name: Option<String>,
    pub tool_input: Option<Value>,
    pub tool_call_id: Option<String>,
    pub tool_response: Option<Value>,
    pub error: Option<Value>,
    pub is_interrupt: Option<bool>,
    pub prompt: Option<String>,
    pub source: Option<String>,
    pub response_text: Option<String>,
    pub stop_hook_active: Option<bool>,
}

impl HookInput {
    /// `createCompatibleHookStdin` 的 JSON 形状：ZCode camelCase 字段 + 无损
    /// snake_case 别名（外部 hook 生态按 Claude 形状读）。
    pub fn to_json(&self, event: HookEventName) -> Value {
        let mut value = json!({
            "sessionId": self.session_id,
            "cwd": self.cwd,
            "mode": self.mode,
            "hookEventName": event.as_str(),
            // 兼容别名
            "hook_event_name": event.as_str(),
            "session_id": self.session_id,
            "permission_mode": self.mode,
        });
        if let Some(name) = &self.tool_name {
            value["toolName"] = json!(name);
            value["tool_name"] = json!(name);
        }
        if let Some(input) = &self.tool_input {
            value["toolInput"] = input.clone();
            value["tool_input"] = input.clone();
        }
        if let Some(call_id) = &self.tool_call_id {
            value["toolCallId"] = json!(call_id);
            value["tool_use_id"] = json!(call_id);
        }
        if let Some(response) = &self.tool_response {
            value["toolResponse"] = response.clone();
            value["tool_response"] = response.clone();
        }
        if let Some(error) = &self.error {
            value["error"] = error.clone();
            value["error_details"] = error.clone();
        }
        if let Some(is_interrupt) = self.is_interrupt {
            value["is_interrupt"] = json!(is_interrupt);
        }
        if let Some(prompt) = &self.prompt {
            value["prompt"] = json!(prompt);
        }
        if let Some(source) = &self.source {
            value["source"] = json!(source);
        }
        if let Some(text) = &self.response_text {
            value["responseText"] = json!(text);
            value["last_assistant_message"] = json!(text);
        }
        if let Some(active) = self.stop_hook_active {
            value["stop_hook_active"] = json!(active);
        }
        value
    }
}

/// `matchesHookMatcher`：`*`/空 → 命中；纯标识符集合按 `|` 精确匹配；否则当正则。
///
/// 正则**按 matcher 字符串缓存**：工具回合每个 tool call 都要匹配一次，重复编译
/// （正则编译是微秒级但每个 call 一次）在长回合里是实打实的浪费；非法正则按"永不命中"
/// 缓存成 None。
pub fn matches_hook_matcher(matcher: Option<&str>, match_value: Option<&str>) -> bool {
    let Some(matcher) = matcher else {
        return true;
    };
    if matcher.is_empty() || matcher == "*" {
        return true;
    }
    let Some(match_value) = match_value else {
        return false;
    };
    if matcher
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'|')
    {
        return matcher.split('|').any(|candidate| candidate == match_value);
    }
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Option<regex::Regex>>>,
    > = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut guard = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let compiled = guard
        .entry(matcher.to_string())
        .or_insert_with(|| regex::Regex::new(matcher).ok());
    compiled
        .as_ref()
        .is_some_and(|regex| regex.is_match(match_value))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionBehavior {
    Allow,
    Deny,
    Ask,
}

/// 一次 hook 运行的归一化结果（`HookRunResult`）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookResult {
    pub additional_contexts: Vec<String>,
    pub block_requested: bool,
    pub stop_reason: Option<String>,
    pub prevent_continuation: bool,
    pub stop_should_continue: Option<bool>,
    pub permission_behavior: Option<PermissionBehavior>,
    pub permission_decision_reason: Option<String>,
    pub updated_input: Option<Value>,
}

impl HookResult {
    pub fn merge(&mut self, next: HookResult) {
        let HookResult {
            additional_contexts,
            block_requested,
            stop_reason,
            prevent_continuation,
            stop_should_continue,
            permission_behavior,
            permission_decision_reason,
            updated_input,
        } = next;
        self.additional_contexts.extend(additional_contexts);
        if block_requested {
            self.block_requested = true;
            self.stop_reason = stop_reason.clone().or(self.stop_reason.take());
        }
        if prevent_continuation {
            self.prevent_continuation = true;
            self.stop_reason = stop_reason.clone().or(self.stop_reason.take());
        }
        if stop_should_continue.is_some() {
            self.stop_should_continue = stop_should_continue;
            self.stop_reason = stop_reason.or(self.stop_reason.take());
        }
        let _ = stop_reason;
        if updated_input.is_some() {
            self.updated_input = updated_input;
        }
        if permission_decision_reason.is_some() {
            self.permission_decision_reason = permission_decision_reason;
        }
        if let Some(behavior) = permission_behavior {
            self.permission_behavior =
                merge_permission_behavior(self.permission_behavior, behavior);
        }
    }
}

fn merge_permission_behavior(
    current: Option<PermissionBehavior>,
    next: PermissionBehavior,
) -> Option<PermissionBehavior> {
    Some(match (current, next) {
        (Some(PermissionBehavior::Deny), _) | (_, PermissionBehavior::Deny) => {
            PermissionBehavior::Deny
        }
        (Some(PermissionBehavior::Ask), _) | (_, PermissionBehavior::Ask) => {
            PermissionBehavior::Ask
        }
        _ => next,
    })
}

fn is_permission_event(event: HookEventName) -> bool {
    matches!(
        event,
        HookEventName::PreToolUse | HookEventName::PermissionRequest
    )
}

fn should_prevent_continuation(event: HookEventName) -> bool {
    matches!(
        event,
        HookEventName::PreToolUse
            | HookEventName::PermissionRequest
            | HookEventName::UserPromptSubmit
    )
}

/// `processHookOutput`：把 hook 的 JSON 输出解释成归一化结果。
///
/// `hookSpecificOutput.hookEventName` 与当前事件不一致是**错误**（不是忽略）：
/// 那说明 hook 拿错了输入或写错了输出，静默接受会把决定落到错误的事件上。
pub fn process_hook_output(event: HookEventName, output: &Value) -> Result<HookResult, String> {
    let mut result = HookResult::default();
    let object = output.as_object();
    let output_string = |key: &str| {
        object
            .and_then(|object| object.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    };

    let continue_flag = object
        .and_then(|object| object.get("continue"))
        .and_then(Value::as_bool);
    if continue_flag == Some(false) && event != HookEventName::Stop {
        result.block_requested = true;
        result.stop_reason = output_string("stopReason").or_else(|| output_string("reason"));
        if should_prevent_continuation(event) {
            result.prevent_continuation = true;
        }
        if is_permission_event(event) {
            result.permission_behavior = Some(PermissionBehavior::Deny);
        }
    }
    if event == HookEventName::Stop && continue_flag == Some(true) {
        result.stop_should_continue = Some(true);
        result.stop_reason = output_string("stopReason").or_else(|| output_string("reason"));
    }
    let decision = output_string("decision");
    if decision.as_deref() == Some("approve") && is_permission_event(event) {
        result.permission_behavior = Some(PermissionBehavior::Allow);
    }
    if decision.as_deref() == Some("block") {
        result.block_requested = true;
        result.stop_reason = output_string("stopReason")
            .or_else(|| output_string("reason"))
            .or_else(|| output_string("systemMessage"));
        if is_permission_event(event) {
            result.permission_behavior = Some(PermissionBehavior::Deny);
        }
        if should_prevent_continuation(event) {
            result.prevent_continuation = true;
        }
        if event == HookEventName::Stop {
            result.stop_should_continue = Some(true);
            for key in ["systemMessage", "reason"] {
                if let Some(text) = output_string(key) {
                    result.additional_contexts.push(text);
                }
            }
        }
    }
    for key in ["additionalContext", "additional_context"] {
        if let Some(text) = output_string(key) {
            result.additional_contexts.push(text);
        }
    }

    let Some(specific) = object.and_then(|object| object.get("hookSpecificOutput")) else {
        return Ok(result);
    };
    let specific_object = specific
        .as_object()
        .ok_or_else(|| "hookSpecificOutput must be an object".to_string())?;
    let name = specific_object
        .get("hookEventName")
        .and_then(Value::as_str)
        .ok_or_else(|| "hookSpecificOutput.hookEventName is required".to_string())?;
    if name != event.as_str() {
        return Err(format!(
            "Hook returned wrong event name (expected {}, got {name})",
            event.as_str()
        ));
    }
    let specific_string = |key: &str| {
        specific_object
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    match event {
        HookEventName::PreToolUse => {
            if let Some(decision) = specific_string("permissionDecision") {
                result.permission_behavior = Some(match decision.as_str() {
                    "allow" => PermissionBehavior::Allow,
                    "deny" => PermissionBehavior::Deny,
                    "ask" => PermissionBehavior::Ask,
                    other => return Err(format!("unknown permissionDecision: {other}")),
                });
                result.permission_decision_reason = specific_string("permissionDecisionReason");
            }
            if let Some(updated) = specific_object.get("updatedInput") {
                if !updated.is_null() {
                    result.updated_input = Some(updated.clone());
                }
            }
            if let Some(text) = specific_string("additionalContext") {
                result.additional_contexts.push(text);
            }
        }
        HookEventName::PermissionRequest => {
            // `decision: {behavior: allow|deny|ask, message?}` 是最终权限决定；
            // 只把它塞进 reason 而不改 permissionBehavior 会让 broker 继续弹窗。
            if let Some(decision) = specific_object.get("decision") {
                if let Some(behavior) = decision.get("behavior").and_then(Value::as_str) {
                    result.permission_behavior = Some(match behavior {
                        "allow" => PermissionBehavior::Allow,
                        "deny" => PermissionBehavior::Deny,
                        "ask" => PermissionBehavior::Ask,
                        other => return Err(format!("unknown permission behavior: {other}")),
                    });
                }
                result.permission_decision_reason = decision
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| Some(decision.to_string()));
            }
        }
        _ => {
            if let Some(text) = specific_string("additionalContext") {
                result.additional_contexts.push(text);
            }
        }
    }
    Ok(result)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvocationStatus {
    Ran,
    Blocked,
    Error,
    /// 准入未通过（未信任/策略/快照不匹配）：跳过，不影响动作本身。
    Skipped {
        reason_code: String,
    },
    Background,
}

impl InvocationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            InvocationStatus::Ran => "ran",
            InvocationStatus::Blocked => "blocked",
            InvocationStatus::Error => "error",
            InvocationStatus::Skipped { .. } => "skipped",
            InvocationStatus::Background => "background",
        }
    }
}

/// 一条 hook 调用记录（时间线 `hookInvocation` 行与诊断的来源）。
#[derive(Debug, Clone)]
pub struct HookInvocation {
    pub review_item_id: String,
    pub event: HookEventName,
    pub command_display: String,
    pub status: InvocationStatus,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub stderr_preview: Option<String>,
    pub stdout_preview: Option<String>,
    pub reason_code: Option<String>,
}

#[derive(Debug, Default)]
pub struct HookEventOutcome {
    pub result: HookResult,
    pub invocations: Vec<HookInvocation>,
    /// hook 进程失败（非 0/2 退出、超时、schema 错误）的可读原因；调用方决定是否 fatal。
    pub hook_error: Option<String>,
}

struct Execution {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    timed_out: bool,
    duration_ms: u64,
    spawn_error: Option<String>,
}

/// `command` 型 hook 的 shell 调用：显式 `shell` 用 `<shell> /C|-c`，否则平台默认。
fn shell_invocation(entry: &CanonicalEntry) -> (String, Vec<String>) {
    let command = entry.command.clone();
    let explicit_shell = match &entry.shell {
        Some(super::config::ShellConfig::Script(shell)) => Some(shell.clone()),
        _ => None,
    };
    #[cfg(windows)]
    {
        let shell = explicit_shell
            .unwrap_or_else(|| std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()));
        (shell, vec!["/D".to_string(), "/C".to_string(), command])
    }
    #[cfg(not(windows))]
    {
        let shell = explicit_shell.unwrap_or_else(|| "/bin/sh".to_string());
        (shell, vec!["-c".to_string(), command])
    }
}

fn build_command(entry: &CanonicalEntry, cwd: &Path, session_id: &str) -> Command {
    let mut command = match entry.entry_type {
        EntryType::Process => {
            let mut command = Command::new(&entry.command);
            command.args(entry.args.clone().unwrap_or_default());
            command
        }
        EntryType::Command => {
            let (shell, args) = shell_invocation(entry);
            let mut command = Command::new(shell);
            command.args(args);
            command
        }
    };
    command.current_dir(cwd);
    command.env("ZCODE_PROJECT_DIR", cwd);
    command.env("ZCODE_SESSION_ID", session_id);
    command.env("CLAUDE_PROJECT_DIR", cwd);
    command.env("CLAUDE_SESSION_ID", session_id);
    command.env("CLAUDE_CODE_SESSION_ID", session_id);
    command
}

fn read_capped(
    mut stream: impl Read + Send + 'static,
    cap: usize,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut collected = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if collected.len() < cap {
                        let room = cap - collected.len();
                        collected.extend_from_slice(&buffer[..read.min(room)]);
                    }
                    // 超出上限的字节继续排空但不入账，避免子进程写满管道阻塞。
                }
                Err(_) => break,
            }
        }
        collected
    })
}

fn execute(
    entry: &CanonicalEntry,
    stdin_json: &str,
    cwd: &Path,
    cancel: &Arc<dyn Fn() -> bool + Send + Sync>,
) -> Execution {
    let started = Instant::now();
    let mut command = build_command(entry, cwd, &stdin_json_session(stdin_json));
    let mut child = match command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return Execution {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
                timed_out: false,
                duration_ms: started.elapsed().as_millis() as u64,
                spawn_error: Some(error.to_string()),
            };
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(stdin_json.as_bytes());
        let _ = stdin.write_all(b"\n");
    }
    let cap = entry.resolved_max_output_bytes;
    let stdout_reader = child.stdout.take().map(|stdout| read_capped(stdout, cap));
    let stderr_reader = child.stderr.take().map(|stderr| read_capped(stderr, cap));
    let deadline = started + Duration::from_millis(entry.resolved_timeout_ms);
    let mut timed_out = false;
    let status = loop {
        if cancel() {
            timed_out = true;
            let _ = nc_code_proc::kill_tree(child.id(), 2_000);
            break child.wait().ok();
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = nc_code_proc::kill_tree(child.id(), 2_000);
            break child.wait().ok();
        }
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(OUTPUT_POLL_MS)),
            Err(_) => break None,
        }
    };
    let stdout = stdout_reader
        .and_then(|reader| reader.join().ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
        .unwrap_or_default();
    let stderr = stderr_reader
        .and_then(|reader| reader.join().ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
        .unwrap_or_default();
    Execution {
        stdout,
        stderr,
        exit_code: status.and_then(|status| status.code()),
        timed_out,
        duration_ms: started.elapsed().as_millis() as u64,
        spawn_error: None,
    }
}

/// 从 stdin JSON 反查 sessionId（构建 command 时用；避免再传一份参数）。
fn stdin_json_session(stdin_json: &str) -> String {
    serde_json::from_str::<Value>(stdin_json)
        .ok()
        .and_then(|value| {
            value
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

fn preview(text: &str, limit: usize) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(limit).collect())
}

/// 执行一个事件的全部匹配 hook（按 snapshot 顺序），汇总结果与调用记录。
pub fn run_event(
    event: HookEventName,
    match_value: Option<&str>,
    input: &HookInput,
    evaluation: &SnapshotEvaluation,
    snapshot: &BundleSnapshot,
    cwd: &Path,
    cancel: Arc<dyn Fn() -> bool + Send + Sync>,
) -> HookEventOutcome {
    let stdin_json = input.to_json(event).to_string();
    let mut outcome = HookEventOutcome::default();
    for entry in snapshot
        .hooks
        .iter()
        .filter(|entry| entry.event == event)
        .filter(|entry| matches_hook_matcher(entry.matcher.as_deref(), match_value))
    {
        let item = evaluation.item(&entry.review_item_id);
        let runnable = item.is_some_and(|item| item.effective_runnable);
        let descriptor = execution_descriptor(
            entry.entry_type.as_str(),
            &entry.command,
            entry.args.as_deref(),
            entry.asynchronous == Some(true),
            entry.resolved_timeout_ms,
        );
        if !runnable {
            outcome.invocations.push(HookInvocation {
                review_item_id: entry.review_item_id.clone(),
                event,
                command_display: descriptor.command_display,
                status: InvocationStatus::Skipped {
                    reason_code: item
                        .map(|item| item.reason_code.to_string())
                        .unwrap_or_else(|| "workspace_hooks_not_applicable".to_string()),
                },
                exit_code: None,
                duration_ms: 0,
                stderr_preview: None,
                stdout_preview: None,
                reason_code: item.map(|item| item.reason_code.to_string()),
            });
            continue;
        }
        if entry.entry_type == EntryType::Command && entry.asynchronous == Some(true) {
            spawn_background(entry, &stdin_json, cwd);
            outcome.invocations.push(HookInvocation {
                review_item_id: entry.review_item_id.clone(),
                event,
                command_display: descriptor.command_display,
                status: InvocationStatus::Background,
                exit_code: None,
                duration_ms: 0,
                stderr_preview: None,
                stdout_preview: None,
                reason_code: None,
            });
            continue;
        }
        let execution = execute(entry, &stdin_json, cwd, &cancel);
        let mut invocation = HookInvocation {
            review_item_id: entry.review_item_id.clone(),
            event,
            command_display: descriptor.command_display,
            status: InvocationStatus::Ran,
            exit_code: execution.exit_code,
            duration_ms: execution.duration_ms,
            stderr_preview: preview(&execution.stderr, 400),
            stdout_preview: None,
            reason_code: None,
        };
        if let Some(error) = &execution.spawn_error {
            invocation.status = InvocationStatus::Error;
            invocation.reason_code = Some("workspace_hook_spawn_failed".to_string());
            outcome.hook_error = Some(format!("hook could not start: {error}"));
            outcome.invocations.push(invocation);
            continue;
        }
        if execution.timed_out {
            invocation.status = InvocationStatus::Error;
            invocation.reason_code = Some("workspace_hook_timeout".to_string());
            outcome.hook_error = Some(format!(
                "hook timed out after {}ms",
                entry.resolved_timeout_ms
            ));
            outcome.invocations.push(invocation);
            continue;
        }
        match execution.exit_code {
            Some(0) => match parse_hook_stdout(event, &execution.stdout) {
                Ok(Some(output)) => match process_hook_output(event, &output) {
                    Ok(result) => {
                        // JSON 里给出的 deny/block 也是"这次调用产生了阻断"，
                        // 调用记录必须如实标成 blocked（不是 success）。
                        if result.block_requested
                            || result.permission_behavior == Some(PermissionBehavior::Deny)
                        {
                            invocation.status = InvocationStatus::Blocked;
                            invocation.stdout_preview = preview(&execution.stdout, 400);
                        }
                        outcome.result.merge(result);
                    }
                    Err(error) => {
                        invocation.status = InvocationStatus::Error;
                        invocation.reason_code = Some("workspace_hook_invalid_output".to_string());
                        outcome.hook_error = Some(error);
                    }
                },
                Ok(None) => {}
                Err(error) => {
                    invocation.status = InvocationStatus::Error;
                    invocation.reason_code = Some("workspace_hook_invalid_output".to_string());
                    outcome.hook_error = Some(error);
                }
            },
            Some(2) => {
                let reason = preview(&execution.stderr, 400)
                    .or_else(|| preview(&execution.stdout, 400))
                    .unwrap_or_else(|| "Hook blocked execution".to_string());
                invocation.status = InvocationStatus::Blocked;
                invocation.stdout_preview = preview(&execution.stdout, 400);
                outcome.result.block_requested = true;
                outcome.result.stop_reason = Some(reason.clone());
                if should_prevent_continuation(event) {
                    outcome.result.prevent_continuation = true;
                }
                if is_permission_event(event) {
                    outcome.result.permission_behavior = Some(PermissionBehavior::Deny);
                }
                if event == HookEventName::Stop {
                    outcome.result.stop_should_continue = Some(true);
                    outcome.result.additional_contexts.push(reason);
                }
            }
            other => {
                invocation.status = InvocationStatus::Error;
                invocation.reason_code = Some("workspace_hook_failed".to_string());
                outcome.hook_error = Some(format!(
                    "hook process failed (exit {}): {}",
                    other
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "signal".to_string()),
                    preview(&execution.stderr, 400).unwrap_or_default()
                ));
            }
        }
        outcome.invocations.push(invocation);
    }
    outcome
}

fn spawn_background(entry: &CanonicalEntry, stdin_json: &str, cwd: &Path) {
    let mut command = build_command(entry, cwd, &stdin_json_session(stdin_json));
    let child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = child {
        if let Some(mut stdin) = child.stdin.take() {
            let payload = stdin_json.to_string();
            std::thread::spawn(move || {
                let _ = stdin.write_all(payload.as_bytes());
                let _ = stdin.write_all(b"\n");
            });
        }
        // 后台 hook 不等待：探测进程回收交给操作系统；不保留 Child 句柄。
    }
}

/// `parseHookStdout`：只有以 `{` 开头的 stdout 才当 JSON；非 JSON 是诊断文本。
pub fn parse_hook_stdout(_event: HookEventName, stdout: &str) -> Result<Option<Value>, String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() || !trimmed.starts_with('{') {
        return Ok(None);
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(value) if value.is_object() => Ok(Some(value)),
        Ok(_) => Err("Hook stdout JSON must be an object".to_string()),
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_hooks::config::{
        read_sources, RuntimeRoot, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_TIMEOUT_MS,
    };
    use crate::workspace_hooks::digest::build_bundle;
    use std::path::PathBuf;

    fn hook_config(entry_json: &str) -> String {
        format!(
            r#"{{"hooks":{{"enabled":true,"events":{{"PreToolUse":[{{"matcher":"Bash","hooks":[{entry_json}]}}]}}}}}}"#
        )
    }

    fn build_workspace(tag: &str, config: &str) -> (PathBuf, BundleSnapshot) {
        let base =
            std::env::temp_dir().join(format!("zcode-hook-runtime-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join(".zcode")).unwrap();
        std::fs::write(base.join(".zcode").join("config.json"), config).unwrap();
        let (sources, errors) = read_sources(&base, None);
        assert!(errors.is_empty());
        let runtime = RuntimeRoot {
            enabled: true,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        };
        let snapshot = build_bundle("ws-rt", &base, &sources, &runtime, "t").expect("bundle");
        (base, snapshot)
    }

    /// 进程型 hook：`node hook.js <extra>`（脚本与 cwd 都在工作区）。
    fn workspace_with_process_hook(
        tag: &str,
        script: &str,
        extra: &str,
    ) -> (PathBuf, BundleSnapshot) {
        let base =
            std::env::temp_dir().join(format!("zcode-hook-runtime-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join(".zcode")).unwrap();
        std::fs::write(base.join("hook.js"), script).unwrap();
        let entry = format!(r#"{{"type":"process","command":"node","args":["hook.js"]{extra}}}"#);
        std::fs::write(base.join(".zcode").join("config.json"), hook_config(&entry)).unwrap();
        let (sources, errors) = read_sources(&base, None);
        assert!(errors.is_empty());
        let runtime = RuntimeRoot {
            enabled: true,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        };
        let snapshot = build_bundle("ws-rt", &base, &sources, &runtime, "t").expect("bundle");
        (base, snapshot)
    }

    /// 直接构造评估结果：本模块测执行与解释，准入推导在 admission 的测试里。
    fn evaluation_with(snapshot: &BundleSnapshot, trusted: bool) -> SnapshotEvaluation {
        SnapshotEvaluation {
            workspace_identity: snapshot.workspace_identity.clone(),
            bundle_digest: snapshot.bundle_digest.clone(),
            policy: crate::workspace_hooks::HookPolicy::user_decides(),
            store_status: crate::workspace_hooks::TrustStoreStatus::Ok,
            items: snapshot
                .hooks
                .iter()
                .map(|entry| crate::workspace_hooks::EffectiveState {
                    review_item_id: entry.review_item_id.clone(),
                    trust_state: if trusted {
                        crate::workspace_hooks::TrustState::TrustedPersistent
                    } else {
                        crate::workspace_hooks::TrustState::PendingTrust
                    },
                    admission_class: if trusted {
                        crate::workspace_hooks::AdmissionClass::Admitted
                    } else {
                        crate::workspace_hooks::AdmissionClass::Pending
                    },
                    effective_runnable: trusted,
                    reason_code: if trusted {
                        "workspace_hooks_trusted_persistent"
                    } else {
                        "workspace_hooks_pending_trust"
                    },
                    workspace_identity: snapshot.workspace_identity.clone(),
                    bundle_digest: snapshot.bundle_digest.clone(),
                    hook_declaration_digest: entry.hook_declaration_digest.clone(),
                    source_path: entry.source_relative_path.clone(),
                })
                .collect(),
        }
    }

    fn cancel_never() -> Arc<dyn Fn() -> bool + Send + Sync> {
        Arc::new(|| false)
    }

    #[test]
    fn matcher_rules_match_the_typescript_implementation() {
        assert!(matches_hook_matcher(None, Some("Bash")));
        assert!(matches_hook_matcher(Some("*"), None));
        assert!(matches_hook_matcher(Some("Bash|Write"), Some("Write")));
        assert!(!matches_hook_matcher(Some("Bash|Write"), Some("Read")));
        assert!(!matches_hook_matcher(Some("Bash"), None));
        assert!(matches_hook_matcher(Some("^Ba.*$"), Some("Bash")));
        assert!(!matches_hook_matcher(Some("^(unclosed"), Some("Bash")));
    }

    #[test]
    fn output_processing_maps_blocks_permissions_and_context() {
        let blocked = process_hook_output(
            HookEventName::PreToolUse,
            &json!({"continue": false, "stopReason": "nope"}),
        )
        .unwrap();
        assert!(blocked.block_requested);
        assert!(blocked.prevent_continuation);
        assert_eq!(blocked.permission_behavior, Some(PermissionBehavior::Deny));
        assert_eq!(blocked.stop_reason.as_deref(), Some("nope"));

        let deny = process_hook_output(
            HookEventName::PreToolUse,
            &json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": "no writes",
                    "additionalContext": "seen",
                    "updatedInput": {"file_path": "x"}
                }
            }),
        )
        .unwrap();
        assert_eq!(deny.permission_behavior, Some(PermissionBehavior::Deny));
        assert_eq!(
            deny.permission_decision_reason.as_deref(),
            Some("no writes")
        );
        assert_eq!(deny.additional_contexts, vec!["seen".to_string()]);
        assert_eq!(deny.updated_input, Some(json!({"file_path": "x"})));

        let wrong_event = process_hook_output(
            HookEventName::PreToolUse,
            &json!({"hookSpecificOutput": {"hookEventName": "Stop"}}),
        )
        .unwrap_err();
        assert!(wrong_event.contains("wrong event name"));

        let stop = process_hook_output(
            HookEventName::Stop,
            &json!({"decision": "block", "reason": "keep going", "systemMessage": "extra"}),
        )
        .unwrap();
        assert_eq!(stop.stop_should_continue, Some(true));
        assert_eq!(
            stop.additional_contexts,
            vec!["extra".to_string(), "keep going".to_string()]
        );

        let merged = {
            let mut first = process_hook_output(
                HookEventName::PreToolUse,
                &json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "ask"}}),
            )
            .unwrap();
            let second = process_hook_output(
                HookEventName::PreToolUse,
                &json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny"}}),
            )
            .unwrap();
            first.merge(second);
            first
        };
        assert_eq!(
            merged.permission_behavior,
            Some(PermissionBehavior::Deny),
            "deny 优先"
        );
    }

    #[test]
    fn trusted_hook_runs_and_its_json_output_is_applied() {
        let (base, snapshot) = workspace_with_process_hook(
            "run",
            "process.stdin.resume();process.stdout.write(JSON.stringify({hookSpecificOutput:{hookEventName:'PreToolUse',permissionDecision:'deny',permissionDecisionReason:'scripted'},additionalContext:'from hook'}));",
            "",
        );
        let evaluation = evaluation_with(&snapshot, true);
        let input = HookInput {
            session_id: "session-1".into(),
            cwd: base.to_string_lossy().to_string(),
            mode: "build".into(),
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "ls"})),
            tool_call_id: Some("call-1".into()),
            ..HookInput::default()
        };
        let outcome = run_event(
            HookEventName::PreToolUse,
            Some("Bash"),
            &input,
            &evaluation,
            &snapshot,
            &base,
            cancel_never(),
        );
        assert_eq!(outcome.invocations.len(), 1, "{:?}", outcome.invocations);
        assert_eq!(outcome.invocations[0].status, InvocationStatus::Blocked);
        assert_eq!(
            outcome.result.permission_behavior,
            Some(PermissionBehavior::Deny)
        );
        assert_eq!(
            outcome.result.additional_contexts,
            vec!["from hook".to_string()]
        );
        assert!(outcome.hook_error.is_none(), "{:?}", outcome.hook_error);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn untrusted_hook_is_skipped_without_affecting_the_action() {
        let (base, snapshot) = build_workspace(
            "skip",
            &hook_config(r#"{"type":"command","command":"exit 1"}"#),
        );
        let evaluation = evaluation_with(&snapshot, false);
        let outcome = run_event(
            HookEventName::PreToolUse,
            Some("Bash"),
            &HookInput {
                session_id: "s".into(),
                ..HookInput::default()
            },
            &evaluation,
            &snapshot,
            &base,
            cancel_never(),
        );
        assert_eq!(outcome.invocations.len(), 1);
        assert!(matches!(
            outcome.invocations[0].status,
            InvocationStatus::Skipped { .. }
        ));
        assert!(!outcome.result.block_requested, "跳过不等于阻断");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn exit_code_two_blocks_with_stderr_and_other_failures_are_errors() {
        let (base, snapshot) = build_workspace(
            "exit2",
            &hook_config(r#"{"type":"command","command":"echo blocked reason 1>&2 & exit 2"}"#),
        );
        let evaluation = evaluation_with(&snapshot, true);
        let outcome = run_event(
            HookEventName::PreToolUse,
            Some("Bash"),
            &HookInput::default(),
            &evaluation,
            &snapshot,
            &base,
            cancel_never(),
        );
        assert!(outcome.result.block_requested);
        assert_eq!(
            outcome.result.permission_behavior,
            Some(PermissionBehavior::Deny)
        );
        assert!(outcome
            .result
            .stop_reason
            .as_deref()
            .unwrap_or_default()
            .contains("blocked reason"));
        assert_eq!(outcome.invocations[0].status, InvocationStatus::Blocked);

        let (base2, snapshot2) = build_workspace(
            "exit1",
            &hook_config(r#"{"type":"command","command":"exit 1"}"#),
        );
        let evaluation2 = evaluation_with(&snapshot2, true);
        let outcome2 = run_event(
            HookEventName::PreToolUse,
            Some("Bash"),
            &HookInput::default(),
            &evaluation2,
            &snapshot2,
            &base2,
            cancel_never(),
        );
        assert_eq!(outcome2.invocations[0].status, InvocationStatus::Error);
        assert!(outcome2.hook_error.is_some());
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&base2);
    }

    #[test]
    fn timeout_kills_the_hook_and_reports_a_typed_error() {
        let (base, snapshot) = workspace_with_process_hook(
            "timeout",
            "setTimeout(()=>{}, 5000);",
            r#","timeoutMs":150"#,
        );
        let evaluation = evaluation_with(&snapshot, true);
        let started = std::time::Instant::now();
        let outcome = run_event(
            HookEventName::PreToolUse,
            Some("Bash"),
            &HookInput::default(),
            &evaluation,
            &snapshot,
            &base,
            cancel_never(),
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(4),
            "必须在超时后尽快返回，实际 {:?}",
            started.elapsed()
        );
        assert_eq!(outcome.invocations[0].status, InvocationStatus::Error);
        assert!(outcome
            .hook_error
            .as_deref()
            .unwrap_or_default()
            .contains("timed out"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
