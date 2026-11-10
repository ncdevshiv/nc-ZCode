//! Turn 执行：admission → context 装配 → 模型/工具循环 → settle 的完整生命周期。
//!
//! 与 TS runtime 的关系（specs/rust-native-crates.md S3 验收）：本 increment 的
//! turn 是「多回合 agent 循环」——模型回合与工具回合交替直到模型给出无工具调用
//! 的终态文本。admission 语义完整（串行 gate、幂等重提、cancel 走
//! `zcode-core` CommandInbox）；工具层见 `tools/`（六工具 + 权限闸门）；context
//! 装配见 `turn_context.rs`。
//!
//! fail-closed 纪律：没有 provider 配置时不发起任何模型调用，turn 以
//! `ModelErrorCode::ProviderNotConfigured` 终态投影（turnHeader failed +
//! sessionErrorInfo），命令 settle 为 failed——绝不伪造 provider、绝不吞错误。
//!
//! 防跑飞（不是 tool-count 硬停止）：连续 3 次相同 (tool, input) 签名 → typed
//! fault 终止；单 turn 模型/工具回合上限 64 → typed fault 终止。

use std::sync::Arc;

use nc_code_core::inbox::Admitted;
use nc_code_core::protocol::{CommandFinal, CommandStatus};
use nc_code_provider::executor::{self, StreamRequest};
use nc_code_provider::http_transport::HttpTransport;
use nc_code_provider::request::{
    build_anthropic_messages_body, build_openai_compatible_body, Message, MessageRole,
    ProviderKind, RequestBodyOptions, ToolCallRequest, ToolDefinition,
};
use nc_code_provider::transport::{RealClock, TransportRequest};
use nc_code_provider::{AdapterError, ModelErrorCode, ModelStreamEvent, ModelToolCall, ModelUsage};
use nc_code_service_core::logger::ServiceLogger;
use serde_json::{json, Value};

use crate::provider_config::ProviderConfig;
use crate::rows;
use crate::session::Session;
use crate::snapshot;
use crate::store::now_ms;
use crate::tools::{ToolContext, ToolError, ToolErrorCode, ToolRegistry};
use crate::turn_context::{self, ContextFacts};

/// host 定义的 fault code（TS 侧 fault 词表的完整移植尚未完成，README 差异表记录）。
pub const FAULT_PROVIDER_NOT_CONFIGURED: &str = "fault.model.providerNotConfigured";
pub const FAULT_TURN_STOPPED: &str = "fault.turn.stopped";
pub const FAULT_MODEL_FAILED: &str = "fault.model.requestFailed";
pub const FAULT_TOOL_ROUND_LIMIT: &str = "fault.turn.roundLimitExceeded";
pub const FAULT_REPEATED_TOOL_CALL: &str = "fault.turn.repeatedToolCall";
/// 本轮输入/回合增量无法落进 transcript：会话记忆不可信，turn 以 typed fault
/// 结束（review MAJOR-1 的 fail-closed 出口）。不用更泛的 `fault.model.*` 是因为
/// 根因不是模型——模型根本没被正确告知用户在说什么。
pub const FAULT_HISTORY_NOT_PERSISTED: &str = "fault.turn.historyNotPersisted";
/// 用户提示被 UserPromptSubmit hook 阻断（动作未执行）。
pub const FAULT_HOOK_PROMPT_BLOCKED: &str = "hook.promptBlocked";
/// hook 进程失败（非 0/2 退出、超时、输出不合法）。
pub const FAULT_HOOK_FAILED: &str = "hook.failed";

const MAX_TOOL_ROUNDS: usize = 64;
const REPEATED_TOOL_CALL_LIMIT: usize = 3;
/// TS `MAX_STOP_HOOK_CONTINUATIONS`：Stop hook 最多把同一轮延长 3 次。
const MAX_STOP_HOOK_CONTINUATIONS: u32 = 3;
/// 触发 LLM 摘要压缩的历史消息条数（> 该值时压缩旧部分，保留最近若干条）。
const COMPACT_HISTORY_THRESHOLD_MESSAGES: usize = 24;

/// turn 依赖（host 级只读事实，每 host 构建一次）。
pub struct TurnDeps {
    pub registry: Arc<ToolRegistry>,
    pub context: ContextFacts,
    /// `--full-access`：destructive 工具调用自动允许；缺省 fail-closed。
    pub full_access: bool,
    /// host 级 provider 配置（ListModels 的事实来源）。
    pub provider_config: Option<Arc<ProviderConfig>>,
    /// Self-reference for tools that need host-level services (plan controller,
    /// subagent runner, session context). Set once in `HostServer::new`.
    pub host: std::sync::OnceLock<std::sync::Weak<crate::HostServer>>,
    /// Per-run model/tool round ceiling. `None` = the host default (64).
    pub max_tool_rounds: Option<usize>,
}

struct RoundResult {
    text: String,
    tool_calls: Vec<ModelToolCall>,
    /// Measured generation throughput for this round (`assistantText.throughput`);
    /// `None` when no streaming window was observed.
    throughput: Option<Value>,
}

/// 一轮的输入形态。`GoalContinuation` 是 **model-only** 输入（TS
/// `inputVisibility: "model-only"`）：它进模型历史，但不产生可见的 userInput 行。
/// `BackgroundResult` 是宿主自己发起的后台完成通知轮：输入是
/// `<task-notification>` XML，既有可见的 userInput 行（origin=backgroundResult），
/// 也进模型历史（下一轮模型据此读取 TaskOutput）。
pub(crate) enum TurnInput<'a> {
    User {
        text: &'a str,
        client_id: &'a str,
        /// `AttachmentRef[]` from the sendText payload (resolved into content parts).
        attachments: &'a [Value],
    },
    GoalContinuation {
        prompt: &'a str,
    },
    BackgroundResult {
        text: &'a str,
    },
}

/// 一轮的执行请求。`admitted` 为 None 的是宿主自己发起的轮（goal 续跑），
/// 没有 inbox ticket 可 settle。
pub(crate) struct TurnRequest<'a> {
    pub input: TurnInput<'a>,
    pub turn_id: &'a str,
    pub source_command_id: &'a str,
    pub origin: &'a str,
    pub admitted: Option<Admitted>,
}

/// 轮的结果（goal 循环据此决定续跑 / 暂停 / 结束）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnState {
    Completed,
    Interrupted,
    Failed,
}

pub(crate) struct TurnReport {
    pub lines: Vec<String>,
    pub state: TurnState,
    /// Final assistant text (the last completed assistant row of the turn).
    /// Subagent runs persist this as `subagent-output.txt`.
    pub final_text: String,
}

/// Run one turn for a delegated child session. The child has no inbox ticket: its
/// turn is host-scheduled, exactly like a goal continuation. The returned final text
/// is what `subagent-output.txt` (TaskOutput) exposes to the parent.
pub(crate) async fn run_subagent_turn(
    session: Arc<Session>,
    provider: Option<&ProviderConfig>,
    deps: &TurnDeps,
    prompt: &str,
    turn_id: &str,
    source_command_id: &str,
) -> TurnReport {
    run_turn_request(
        session,
        provider,
        deps,
        TurnRequest {
            input: TurnInput::User {
                text: prompt,
                client_id: "subagent",
                attachments: &[],
            },
            turn_id,
            source_command_id,
            origin: rows::TURN_ORIGIN_USER_INPUT,
            admitted: None,
        },
    )
    .await
}

/// 执行一个 admitted 的 sendText turn。返回需要广播的帧行（由调用方写入 stdout）。
pub async fn run_turn(
    session: Arc<Session>,
    provider: Option<&ProviderConfig>,
    deps: &TurnDeps,
    admitted: Admitted,
) -> Vec<String> {
    let text = admitted
        .envelope
        .payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let source_command_id = admitted.envelope.command_id.clone();
    // turnHeader.origin：edit/retry 的重放轮标成 editRerun，UI 与审计据此区分
    // 「重跑」与「新输入」，不用文本内容猜（TS 的 origin 闭集）。
    let origin = if matches!(
        admitted.envelope.command_type.as_str(),
        "retryTurn" | "editUserQuery"
    ) {
        rows::TURN_ORIGIN_EDIT_RERUN
    } else {
        rows::TURN_ORIGIN_USER_INPUT
    };
    let client_id = admitted.envelope.client_id.clone();
    // Attachments enter the row via the projected shape (the preview/share
    // authorization source) and the model context via the same objects.
    let attachments: Vec<Value> =
        crate::attachment::projectable_attachment_refs(&admitted.envelope.payload);
    let turn_id = source_command_id.clone();
    run_turn_request(
        session,
        provider,
        deps,
        TurnRequest {
            input: TurnInput::User {
                text: &text,
                client_id: &client_id,
                attachments: &attachments,
            },
            turn_id: &turn_id,
            source_command_id: &source_command_id,
            origin,
            admitted: Some(admitted),
        },
    )
    .await
    .lines
}

/// goal 续跑轮：model-only 输入 + `turnHeader.origin = goalContinuation`，
/// 不 settle 任何命令（宿主自己的轮）。
pub(crate) async fn run_goal_turn(
    session: Arc<Session>,
    provider: Option<&ProviderConfig>,
    deps: &TurnDeps,
    prompt: &str,
    turn_id: &str,
    source_command_id: &str,
) -> TurnReport {
    run_model_only_turn(
        session,
        provider,
        deps,
        prompt,
        turn_id,
        source_command_id,
        rows::TURN_ORIGIN_GOAL_CONTINUATION,
    )
    .await
}

/// 后台任务完成通知轮：宿主自己发起，输入是 `<task-notification>` XML；
/// `turnHeader` / `userInput` 都用 `backgroundResult` origin，不 settle 任何命令。
pub(crate) async fn run_background_result_turn(
    session: Arc<Session>,
    provider: Option<&ProviderConfig>,
    deps: &TurnDeps,
    text: &str,
    turn_id: &str,
    source_command_id: &str,
) -> TurnReport {
    run_turn_request(
        session,
        provider,
        deps,
        TurnRequest {
            input: TurnInput::BackgroundResult { text },
            turn_id,
            source_command_id,
            origin: rows::TURN_ORIGIN_BACKGROUND_RESULT,
            admitted: None,
        },
    )
    .await
}

async fn run_model_only_turn(
    session: Arc<Session>,
    provider: Option<&ProviderConfig>,
    deps: &TurnDeps,
    prompt: &str,
    turn_id: &str,
    source_command_id: &str,
    origin: &'static str,
) -> TurnReport {
    run_turn_request(
        session,
        provider,
        deps,
        TurnRequest {
            input: TurnInput::GoalContinuation { prompt },
            turn_id,
            source_command_id,
            origin,
            admitted: None,
        },
    )
    .await
}

async fn run_turn_request(
    session: Arc<Session>,
    provider: Option<&ProviderConfig>,
    deps: &TurnDeps,
    request: TurnRequest<'_>,
) -> TurnReport {
    let TurnRequest {
        input,
        turn_id,
        source_command_id,
        origin,
        admitted,
    } = request;
    let (mut history_input, user_row) = match &input {
        TurnInput::User {
            text,
            client_id,
            attachments,
        } => (
            (*text).to_string(),
            Some(((*text).to_string(), *client_id, *attachments)),
        ),
        TurnInput::GoalContinuation { prompt } => {
            (crate::goal::wrap_continuation_prompt(prompt), None)
        }
        TurnInput::BackgroundResult { text } => (
            (*text).to_string(),
            Some(((*text).to_string(), "background", &[][..])),
        ),
    };
    let input_origin = match &input {
        TurnInput::BackgroundResult { .. } => rows::TURN_ORIGIN_BACKGROUND_RESULT,
        _ => "realUser",
    };
    let mut lines = Vec::new();
    session.set_phase("running");
    session.set_active_command(Some(source_command_id.to_string()));
    session.set_eager_frames(true);
    // Usage fact window starts here: request/error deltas and the first-content
    // timestamp below are the per-turn facts the app-usage aggregate reads.
    session.begin_turn_usage();
    lines.extend(session.project_turn_started(turn_id, source_command_id, origin));
    if let Some((text, client_id, attachments)) = &user_row {
        lines.extend(session.project_user_input_with_origin(
            turn_id,
            text,
            source_command_id,
            client_id,
            input_origin,
            attachments,
        ));
    }
    // SessionStart 的 additionalContext 只注入本会话第一条用户消息；UserPromptSubmit
    // hook 在用户提示进入模型历史**之前**运行：block/deny 时本轮不调模型（动作未执行）。
    let mut hook_block: Option<(String, String)> = None;
    if matches!(input, TurnInput::User { .. }) {
        let mut contexts: Vec<String> = Vec::new();
        if let Some(session_context) = session.take_session_start_context() {
            contexts.push(session_context);
        }
        let hook_input = crate::workspace_hooks::runtime::HookInput {
            session_id: session.id.clone(),
            cwd: session.workspace_root.to_string_lossy().to_string(),
            mode: session.collaboration_mode(),
            prompt: Some(history_input.clone()),
            ..Default::default()
        };
        let outcome = session
            .run_hook_event_blocking(
                crate::workspace_hooks::HookEventName::UserPromptSubmit,
                None,
                hook_input,
                turn_id.to_string(),
                "assistantWork",
                None,
            )
            .await;
        contexts.extend(outcome.result.additional_contexts.iter().cloned());
        if let Some(error) = &outcome.hook_error {
            hook_block = Some((FAULT_HOOK_FAILED.to_string(), error.clone()));
        }
        if outcome.result.block_requested
            || outcome.result.permission_behavior
                == Some(crate::workspace_hooks::runtime::PermissionBehavior::Deny)
        {
            hook_block = Some((
                FAULT_HOOK_PROMPT_BLOCKED.to_string(),
                outcome
                    .result
                    .stop_reason
                    .clone()
                    .unwrap_or_else(|| "blocked by workspace hook".to_string()),
            ));
        }
        let prelude = wrap_hook_context(&contexts);
        if !prelude.is_empty() {
            history_input = format!("{prelude}{history_input}");
        }
    }

    let model_label = provider.map(|config| config.model.as_str());
    let response_id = format!("resp-{source_command_id}");
    let row_id = session.project_assistant_row(turn_id, &response_id, model_label);

    // 会话模型历史由 Session 拥有（specs/rust-native-crates.md S3 session memory
    // 规则 1）：本轮输入**追加**到既有历史末尾，而不是用 `vec![user text]` 重新
    // 起头。旧实现每轮重建 vector，模型在第 2 轮之后就失忆了。
    //
    // 写失败 = 本轮输入无法被记住 = **fail-closed 结束本轮**（review MAJOR-1）。
    // 继续跑的话，请求体来自 `history_snapshot()`，里面**没有**用户这句话，
    // 模型会去回答一个从未收到的问题，而 UI 上仍显示用户的问题与一个对不上的
    // 回答。宁可这一轮失败，也不能伪造上下文。
    let mut user_message = Message::text(MessageRole::User, history_input.as_str());
    if let Some((_, _, attachments)) = &user_row {
        if !attachments.is_empty() {
            if let Some(host) = deps.host.get().and_then(|weak| weak.upgrade()) {
                user_message
                    .parts
                    .extend(crate::attachment::model_parts_for_attachments(
                        &host.attachments,
                        &session.id,
                        attachments,
                    ));
            }
        }
    }
    let input_persisted = match session.append_history(user_message) {
        Ok(()) => true,
        Err(error) => {
            // 与中途/压缩写失败同样要**出声**：typed fault 会走协议，但诊断也要有，
            // 否则运维只能从用户报的「答非所问」往回推。
            ServiceLogger::new("zcode-host").error(
                None,
                format!(
                    "{FAULT_HISTORY_NOT_PERSISTED} session={} turn={turn_id} command={source_command_id}: user input could not be persisted: {error}",
                    session.id
                ),
            );
            false
        }
    };
    let mut history_persist_failed = false;
    let mut streamed_text = String::new();
    let mut final_throughput: Option<Value> = None;

    let outcome = match provider {
        Some(config) if input_persisted && hook_block.is_none() => {
            // 从会话历史起手（含本轮已追加的 user 输入）：工作副本。锁中毒 = 无
            // 法保证所有权，**不**用空历史凑合（见 `history_snapshot` 的注释）。
            let history = match session.history_snapshot() {
                Ok(history) => history,
                Err(error) => {
                    // 已走 fail_history_turn typed fault（协议侧可见），这里补
                    // session/turn/command 上下文让运维日志能定位到具体轮次。
                    ServiceLogger::new("zcode-host").error(
                        None,
                        format!(
                            "{FAULT_HISTORY_NOT_PERSISTED} session={} turn={turn_id} command={source_command_id}: {error}",
                            session.id
                        ),
                    );
                    return fail_history_turn(
                        &session,
                        admitted,
                        row_id,
                        &response_id,
                        model_label,
                        turn_id,
                        source_command_id,
                        streamed_text,
                        lines,
                    );
                }
            };
            let mut history = history;
            let mut new_messages: Vec<Message> = Vec::new();
            let mut compacted = false;
            let outcome = run_agent_loop(
                &session,
                config,
                deps,
                turn_id,
                &mut history,
                &mut new_messages,
                &mut compacted,
                &mut streamed_text,
                &mut final_throughput,
                row_id,
                &mut lines,
            )
            .await;
            // 持久化本轮增量。无论本轮成功、失败还是被取消都照实落盘：模型确实收到
            // 过这些内容，下一轮理应看到，否则等于伪造上下文。压缩发生过的轮次走
            // 整体替换（工作副本就是压缩后的真相），否则追加——顺序即落盘顺序，
            // 重启回放得到同一份历史。两条写路径的失败都并进同一份记账
            // （review round2 NEW MAJOR-A：压缩写失败同样会永久分叉，不能静默）。
            let persist_result = if compacted {
                session.replace_history(&history)
            } else {
                persist_history_increment(&session, &new_messages)
            };
            if let Err(error) = persist_result {
                eprintln!(
                    "[zcode-host] {FAULT_HISTORY_NOT_PERSISTED} ({source_command_id}): {error}"
                );
                history_persist_failed = true;
            }
            outcome
        }
        None => Err(AdapterError::new(
            ModelErrorCode::ProviderNotConfigured,
            "no provider configured; pass --provider-config to enable model calls",
        )),
        _ => Err(AdapterError::new(
            ModelErrorCode::ModelRequestFailed,
            FAULT_HISTORY_NOT_PERSISTED.to_string(),
        )),
    };
    // 本轮历史无法持久化时的 typed fault。它是**独立字段**而不是塞进
    // `AdapterError::new(code, message)` 的 message：settle 映射按 `error.code`
    // 选择 fault（见下），放进 message 会被 `FAULT_MODEL_FAILED` 覆盖，fault 词表
    // 里就多一条任何消费者都读不到的字符串（review round2 MAJOR-1 的实测反证）。
    let history_fault = (history_persist_failed || !input_persisted)
        .then(|| FAULT_HISTORY_NOT_PERSISTED.to_string());

    let stopped = session.take_cancel();
    let (final_text, row_state, failure) = if stopped {
        let info = snapshot::session_error_info(
            FAULT_TURN_STOPPED,
            "turn stopped by user",
            true,
            "runtime",
            now_ms(),
        );
        (streamed_text, "interrupted", Some(info))
    } else if let Some((reason, message)) = hook_block.clone() {
        let info = snapshot::session_error_info(&reason, &message, false, "hook", now_ms());
        (streamed_text, "failed", Some(info))
    } else if let Some(fault) = history_fault.clone() {
        // 历史丢失优先于模型错误表达：即使本轮已经因为别的原因失败，用户更需要知道
        // 「这段对话没存住」，否则重启后他会以为模型只是不记得。
        let info = snapshot::session_error_info(
            &fault,
            "conversation history could not be persisted; this turn was not recorded",
            false,
            "runtime",
            now_ms(),
        );
        (streamed_text, "failed", Some(info))
    } else {
        match outcome {
            Ok(()) => (streamed_text, "complete", None),
            Err(error) => {
                let code = match error.code {
                    ModelErrorCode::ProviderNotConfigured => FAULT_PROVIDER_NOT_CONFIGURED,
                    _ => FAULT_MODEL_FAILED,
                };
                let info =
                    snapshot::session_error_info(code, &error.message, false, "provider", now_ms());
                (streamed_text, "failed", Some(info))
            }
        }
    };

    session.set_last_error(failure.clone());
    lines.extend(session.project_assistant_final(
        row_id,
        &response_id,
        &final_text,
        row_state,
        model_label,
        final_throughput.as_ref(),
    ));

    let (header_state, turn_phase, status, reason_code, result) = if stopped {
        (
            "completedInterrupted",
            "completedInterrupted",
            CommandStatus::Failed,
            Some(FAULT_TURN_STOPPED.to_string()),
            None,
        )
    } else if failure.is_some() {
        (
            "failed",
            "error",
            CommandStatus::Failed,
            failure.as_ref().and_then(|error| {
                error
                    .get("code")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }),
            None,
        )
    } else {
        (
            "completedSuccess",
            "completedSuccess",
            CommandStatus::Accepted,
            None,
            Some(input_accepted_result(source_command_id)),
        )
    };

    finish_turn(
        &session,
        turn_id,
        source_command_id,
        header_state,
        turn_phase,
        model_label,
        &mut lines,
    );
    if let Some(admitted) = admitted {
        settle(&session, admitted, status, reason_code, result);
    }
    let state = match header_state {
        "completedInterrupted" => TurnState::Interrupted,
        "failed" => TurnState::Failed,
        _ => TurnState::Completed,
    };
    TurnReport {
        lines,
        state,
        final_text,
    }
}

/// `formatLifecycleHookAdditionalContextBody`：Stop 续跑的上下文带编号与标题。
fn format_stop_hook_context(contexts: &[String]) -> String {
    let body = contexts
        .iter()
        .enumerate()
        .map(|(index, context)| format!("#{}\n{}", index + 1, context.trim()))
        .collect::<Vec<_>>()
        .join("\n\n");
    wrap_hook_context(&[format!("Stop hook additional context: \n{body}")])
}

/// hook 追加的上下文以 system-reminder 围栏注入模型输入（与 TS 的注入形状一致）。
fn wrap_hook_context(contexts: &[String]) -> String {
    let body = contexts
        .iter()
        .map(|context| context.trim())
        .filter(|context| !context.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if body.is_empty() {
        return String::new();
    }
    format!("<system-reminder>\n{body}\n</system-reminder>\n")
}

fn input_accepted_result(command_id: &str) -> Value {
    json!({
        "type": "inputAccepted",
        "delivery": "startNow",
        "inputId": command_id,
    })
}

/// 逐条追加本轮增量，第一条失败就返回该错误（后续消息不再尝试——内存与磁盘已经
/// 分叉，继续写只会把「缺了哪一段」变得更难解释）。
fn persist_history_increment(
    session: &Arc<Session>,
    new_messages: &[Message],
) -> Result<(), crate::store::StoreError> {
    for message in new_messages {
        session.append_history(message.clone())?;
    }
    Ok(())
}

/// 会话记忆不可用（历史锁中毒）时的 fail-closed 出口：投影失败终态并 settle，
/// **不**发起任何模型调用。与其它失败路径共用同一套终态形状。
#[allow(clippy::too_many_arguments)]
fn fail_history_turn(
    session: &Arc<Session>,
    admitted: Option<Admitted>,
    row_id: u64,
    response_id: &str,
    model_label: Option<&str>,
    turn_id: &str,
    source_command_id: &str,
    streamed_text: String,
    mut lines: Vec<String>,
) -> TurnReport {
    let info = snapshot::session_error_info(
        FAULT_HISTORY_NOT_PERSISTED,
        "conversation history is unavailable; this turn was not run",
        false,
        "runtime",
        now_ms(),
    );
    session.set_last_error(Some(info));
    lines.extend(session.project_assistant_final(
        row_id,
        response_id,
        &streamed_text,
        "failed",
        model_label,
        None,
    ));
    // 终态收口复用 `finish_turn`：行终态、相位、revision 与 state.updated 帧的顺序
    // 只有一份实现，失败轮不会因为少投影一帧而让 revision 与客户端水位分叉。
    finish_turn(
        session,
        turn_id,
        source_command_id,
        "failed",
        "error",
        model_label,
        &mut lines,
    );
    if let Some(admitted) = admitted {
        settle(
            session,
            admitted,
            CommandStatus::Failed,
            Some(FAULT_HISTORY_NOT_PERSISTED.to_string()),
            None,
        );
    }
    TurnReport {
        lines,
        state: TurnState::Failed,
        final_text: String::new(),
    }
}

/// 一条新消息同时进本轮工作副本与本轮增量表。
///
/// 两个下游形状不同、不能只留一份：`history` 组装下一次 provider 请求体
/// （模型必须看到刚生成的 tool_calls/tool_result），`new_messages` 是 durable
/// 增量（只含本轮新产生的消息）。**唯一追加点**是本函数——漏掉任何一处 push
/// 都会让「模型看到的」与「持久化的」分叉，而那正是 session memory 要消灭的
/// 那种不一致。
fn push_turn_message(
    history: &mut Vec<Message>,
    new_messages: &mut Vec<Message>,
    message: Message,
) {
    history.push(message.clone());
    new_messages.push(message);
}

/// 生效鉴权样式（配置加载时已校验；这里是纵深防御：坏样式退回 kind 原生并告警）。
pub(crate) fn resolved_auth_style(
    config: &ProviderConfig,
    provider_kind: ProviderKind,
) -> &'static str {
    match config.auth_style() {
        Ok(style) => style,
        Err(error) => {
            eprintln!("[zcode-host] invalid authStyle ({error}); using provider default");
            match provider_kind {
                ProviderKind::Anthropic => crate::provider_config::AUTH_STYLE_X_API_KEY,
                ProviderKind::OpenaiCompatible => crate::provider_config::AUTH_STYLE_BEARER,
            }
        }
    }
}

/// 多回合循环：模型回合 → 工具回合 → 模型回合……直到无工具调用或 typed fault。
///
/// `history` 是本轮的**工作副本**，不是会话事实的唯一来源：唯一所有者在
/// `Session`（session memory 规则 1）。每条新消息同时进 `history`（组装下一次
/// 请求体用）和 `new_messages`（本轮增量，落盘用），两者靠 `push_turn_message`
/// 保持同步。压缩只改 `history`，并在 `compacted` 里回报它确实重写了——调用方
/// 据此决定是「追加增量」还是「整体替换」，不需要按位置猜差集。
#[allow(clippy::too_many_arguments)]
async fn run_agent_loop(
    session: &Arc<Session>,
    config: &ProviderConfig,
    deps: &TurnDeps,
    turn_id: &str,
    history: &mut Vec<Message>,
    new_messages: &mut Vec<Message>,
    compacted: &mut bool,
    streamed_text: &mut String,
    final_throughput: &mut Option<Value>,
    row_id: u64,
    lines: &mut Vec<String>,
) -> Result<(), AdapterError> {
    let tools = deps.registry.tool_definitions();
    let mut last_signature: Option<String> = None;
    let mut repeated = 0usize;
    let mut stop_hook_count: u32 = 0;

    let max_rounds = deps.max_tool_rounds.unwrap_or(MAX_TOOL_ROUNDS);
    for _round in 0..max_rounds {
        if session.take_cancel() {
            return Ok(());
        }
        // 1) 机械 microcompact：阈值内直接裁剪旧工具结果（无需模型）。就地修改本轮工作
        //    副本——`microcompact_messages` 返回「实际压缩了几条」，所以无需先克隆
        //    再比较（早期版本每轮克隆整份历史，长会话上是每轮 O(n) 分配）。
        //    记账（compacted=true）供调用方做一次 durable 替换，避免每轮写一条。
        if turn_context::microcompact_messages(history) > 0 {
            *compacted = true;
        }
        // 2) LLM 摘要压缩：历史超阈值时用同一模型摘要旧历史（保留最近若干条）。
        //    摘要失败**不静默**：保留原历史继续本轮（宁可长，不丢上下文）。
        if history.len() > COMPACT_HISTORY_THRESHOLD_MESSAGES {
            let cancel_probe: Arc<dyn Fn() -> bool + Send + Sync> = {
                let session = Arc::clone(session);
                Arc::new(move || session.cancel.load(std::sync::atomic::Ordering::SeqCst))
            };
            // 绑定名与外层 `compacted: &mut bool` 刻意区分：`summary` 是新历史，
            // `compacted` 是「本轮历史被重写过」这个标志。
            if let Ok(summary) =
                crate::compact::compact_history(config, &deps.context, history, cancel_probe).await
            {
                if summary.len() < history.len() {
                    *history = summary;
                    *compacted = true;
                }
            }
        }
        let round = run_model_roundtrip(
            session, config, deps, &tools, history, row_id, turn_id, lines,
        )
        .await?;
        if round.throughput.is_some() {
            *final_throughput = round.throughput.clone();
        }
        if !round.text.is_empty() {
            streamed_text.push_str(&round.text);
            lines.extend(session.project_assistant_delta(row_id, &round.text));
        }
        if round.tool_calls.is_empty() {
            // Stop hook：TS 在同一 product turn 内跑（`finishModelStepWithoutToolCalls`）。
            // `continue:true` + 非空 additionalContext + 未超过 3 层 → 把上下文作为
            // user 消息注入历史并继续下一次模型请求；`stop_hook_active` 标记续跑。
            if stop_hook_count < MAX_STOP_HOOK_CONTINUATIONS {
                let hook_input = crate::workspace_hooks::runtime::HookInput {
                    session_id: session.id.clone(),
                    cwd: session.workspace_root.to_string_lossy().to_string(),
                    mode: session.collaboration_mode(),
                    response_text: Some(streamed_text.clone()),
                    stop_hook_active: Some(stop_hook_count > 0),
                    ..Default::default()
                };
                let outcome = session
                    .run_hook_event_blocking(
                        crate::workspace_hooks::HookEventName::Stop,
                        None,
                        hook_input,
                        turn_id.to_string(),
                        "assistantWork",
                        None,
                    )
                    .await;
                let should_continue = outcome.hook_error.is_none()
                    && outcome.result.stop_should_continue == Some(true)
                    && !outcome.result.additional_contexts.is_empty();
                if should_continue {
                    stop_hook_count += 1;
                    let body = format_stop_hook_context(&outcome.result.additional_contexts);
                    push_turn_message(
                        history,
                        new_messages,
                        Message::text(MessageRole::User, body),
                    );
                    continue;
                }
            }
            // 终态回答也是历史（规则 3）：否则下一轮问「你刚说了什么」只能重读
            // 工作区。
            if !streamed_text.is_empty() {
                push_turn_message(
                    history,
                    new_messages,
                    Message::text(MessageRole::Assistant, streamed_text.clone()),
                );
            }
            return Ok(());
        }

        // assistant 回合带 tool_calls 入史（provider 请求体回灌的必需形状）。
        // 正文不重复入史：本轮中途的模型文字只是工具回合的前奏，工具回合的
        // tool_use block 由 tool_calls 承载，额外再塞一条文本会让下一轮请求体
        // 出现「有文本却无对应 tool_use」的悬空 assistant 回合。
        push_turn_message(
            history,
            new_messages,
            Message {
                role: MessageRole::Assistant,
                parts: Vec::new(),
                tool_call_id: None,
                tool_calls: to_tool_call_requests(&round.tool_calls),
                reasoning: Vec::new(),
            },
        );

        for call in &round.tool_calls {
            let signature = format!("{}::{}", call.name, call.input);
            if last_signature.as_deref() == Some(signature.as_str()) {
                repeated += 1;
            } else {
                repeated = 1;
                last_signature = Some(signature);
            }
            if repeated >= REPEATED_TOOL_CALL_LIMIT {
                return Err(AdapterError::new(
                    ModelErrorCode::InvalidModelRequest,
                    format!(
                        "{FAULT_REPEATED_TOOL_CALL}: {REPEATED_TOOL_CALL_LIMIT} identical consecutive calls to {}",
                        call.name
                    ),
                ));
            }

            let input_text = call.input.to_string();
            lines.extend(session.project_tool_call(
                turn_id,
                &call.id,
                &call.name,
                "running",
                &input_text,
                None,
                None,
                None,
                None,
            ));
            // PreToolUse：deny/block 时工具**不执行**（模型收到的是一条错误结果），
            // allow 时跳过 permission 询问；updatedInput 覆盖模型给的参数。
            let mut hook_input = crate::workspace_hooks::runtime::HookInput {
                session_id: session.id.clone(),
                cwd: session.workspace_root.to_string_lossy().to_string(),
                mode: session.collaboration_mode(),
                tool_name: Some(call.name.clone()),
                tool_input: Some(call.input.clone()),
                tool_call_id: Some(call.id.clone()),
                ..Default::default()
            };
            let pre = session
                .run_hook_event_blocking(
                    crate::workspace_hooks::HookEventName::PreToolUse,
                    Some(call.name.clone()),
                    hook_input.clone(),
                    turn_id.to_string(),
                    "toolBefore",
                    Some(call.id.clone()),
                )
                .await;
            let mut effective_call = call.clone();
            if let Some(updated) = &pre.result.updated_input {
                if updated.is_object() {
                    effective_call.input = updated.clone();
                }
            }
            let pre_denied = pre.result.block_requested
                || pre.result.permission_behavior
                    == Some(crate::workspace_hooks::runtime::PermissionBehavior::Deny);
            let pre_allowed = pre.result.permission_behavior
                == Some(crate::workspace_hooks::runtime::PermissionBehavior::Allow);
            let pre_context = wrap_hook_context(&pre.result.additional_contexts);
            let output = if let Some(error) = &pre.hook_error {
                Err(ToolError::new(
                    ToolErrorCode::ExecutionFailed,
                    format!("workspace hook failed: {error}"),
                ))
            } else if pre_denied {
                Err(ToolError::new(
                    ToolErrorCode::PermissionDenied,
                    format!(
                        "blocked by workspace hook: {}",
                        pre.result
                            .stop_reason
                            .as_deref()
                            .unwrap_or("no reason given")
                    ),
                ))
            } else {
                execute_tool(&effective_call, session, deps, turn_id, pre_allowed).await
            };
            let (status, error) = match &output {
                Ok(_) => ("success".to_string(), None),
                Err(error) => (
                    if matches!(error.code, ToolErrorCode::Cancelled) {
                        "cancelled".to_string()
                    } else {
                        "error".to_string()
                    },
                    Some((error.code.as_str().to_string(), error.message.clone())),
                ),
            };
            let error_ref = error
                .as_ref()
                .map(|(code, message)| (code.as_str(), message.as_str()));
            let output_text = match &output {
                Ok(text) => Some(text.as_str()),
                // The error message is the row's visible output too; the UI renders
                // `output.text` for both success and failure.
                Err(error) => Some(error.message.as_str()),
            };
            let display = output.as_ref().ok().and_then(|text| {
                if let Some(tool) = deps.registry.get(&call.name) {
                    tool.result_display_for(&effective_call.input, text)
                } else {
                    deps.registry
                        .get_async(&call.name)
                        .and_then(|tool| tool.result_display_for(&effective_call.input, text))
                }
            });
            let work_id = output
                .as_ref()
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
                .and_then(|value| {
                    value["workId"]
                        .as_str()
                        .or_else(|| value["agentId"].as_str())
                        .or_else(|| value["backgroundTaskId"].as_str())
                        .map(str::to_string)
                });
            lines.extend(session.project_tool_call(
                turn_id,
                &call.id,
                &call.name,
                &status,
                &input_text,
                error_ref,
                output_text,
                display,
                work_id.as_deref(),
            ));
            // TodoWrite owns session state, not a workspace file: persist it here so a
            // restart can read the list back (TS `sessionStore.updateTodos`).
            if call.name == "TodoWrite" && output.is_ok() {
                if let Err(error) = session.persist_todos() {
                    eprintln!("[zcode-host] TodoWrite persistence failed ({turn_id}): {error}");
                }
            }

            // PostToolUse / PostToolUseFailure：附加上下文（含阻断理由）随工具结果回灌。
            hook_input.tool_response = output
                .as_ref()
                .ok()
                .map(|text| serde_json::Value::String(text.clone()));
            hook_input.error = output
                .as_ref()
                .err()
                .map(|error| json!({"message": error.message, "code": error.code.as_str()}));
            let post_event = if output.is_ok() {
                crate::workspace_hooks::HookEventName::PostToolUse
            } else {
                crate::workspace_hooks::HookEventName::PostToolUseFailure
            };
            let post = session
                .run_hook_event_blocking(
                    post_event,
                    Some(call.name.clone()),
                    hook_input,
                    turn_id.to_string(),
                    "toolAfter",
                    Some(call.id.clone()),
                )
                .await;
            let mut result_text = match &output {
                Ok(text) => text.clone(),
                Err(error) => error.to_model_text(),
            };
            if !pre_context.is_empty() {
                result_text = format!("{result_text}\n\n{pre_context}");
            }
            let post_context = wrap_hook_context(&post.result.additional_contexts);
            if !post_context.is_empty() {
                result_text = format!("{result_text}\n\n{post_context}");
            }
            if let Some(error) = &post.hook_error {
                result_text = format!("{result_text}\n\n[workspace hook failed] {error}");
            }
            let result_text = model_budgeted_result(deps, &call.name, result_text);
            let mut tool_message = Message::text(MessageRole::Tool, result_text);
            tool_message.tool_call_id = Some(call.id.clone());
            push_turn_message(history, new_messages, tool_message);
        }
    }

    Err(AdapterError::new(
        ModelErrorCode::ModelRequestFailed,
        format!(
            "{FAULT_TOOL_ROUND_LIMIT}: {max_rounds} model/tool rounds without a terminal answer"
        ),
    ))
}

/// Apply the tool's `maxModelBytes` / `previewDirection` to the text that enters the
/// model context. The row projection keeps the full output; only the model-facing
/// copy is bounded, and the truncation is announced (never silent).
fn model_budgeted_result(deps: &TurnDeps, tool_name: &str, text: String) -> String {
    let contract = deps
        .registry
        .get(tool_name)
        .map(|tool| tool.contract())
        .or_else(|| {
            deps.registry
                .get_async(tool_name)
                .map(|tool| tool.contract())
        });
    let Some(contract) = contract else {
        return text;
    };
    let max = contract.max_model_bytes.max(1);
    if text.len() <= max {
        return text;
    }
    let tag = format!("[Tool output truncated by resultBudget: maxModelBytes={max}]");
    if contract.preview_direction == "tail" {
        let budget = max.saturating_sub(tag.len() + 1);
        let mut start = text.len().saturating_sub(budget);
        while start > 0 && !text.is_char_boundary(start) {
            start -= 1;
        }
        format!("{tag}\n{}", &text[start..])
    } else {
        let budget = max.saturating_sub(tag.len() + 1);
        let mut end = budget.min(text.len());
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n{tag}", &text[..end])
    }
}

fn to_tool_call_requests(calls: &[ModelToolCall]) -> Vec<ToolCallRequest> {
    calls
        .iter()
        .map(|call| ToolCallRequest {
            id: call.id.clone(),
            name: call.name.clone(),
            input: call.input.clone(),
        })
        .collect()
}

/// 权限闸门 + 执行。错误以稳定文本回灌模型（模型需要能据此修正，而不是猜）。
async fn execute_tool(
    call: &ModelToolCall,
    session: &Arc<Session>,
    deps: &TurnDeps,
    turn_id: &str,
    hook_allowed: bool,
) -> Result<String, ToolError> {
    let sync_tool = deps.registry.get(&call.name);
    let async_tool = deps.registry.get_async(&call.name);
    if sync_tool.is_none() && async_tool.is_none() {
        return Err(ToolError::new(
            ToolErrorCode::InvalidInput,
            format!("no tool named {} in this host increment", call.name),
        ));
    }
    let host = deps.host.get().and_then(|weak| weak.upgrade());
    let ctx = ToolContext {
        workspace_root: session.workspace_root.clone(),
        // 生效授权 = 启动 flag 或用户在 permission 交互里授予的会话级授权。
        full_access: deps.full_access || session.full_access_granted(),
        // 有订阅者才可能有人应答 permission；headless 时工具层 fail-closed。
        interactive: session.has_subscribers(),
        trace_id: session.log_epoch(),
        turn_id: turn_id.to_string(),
        session_id: session.id.clone(),
        model_ref: deps
            .provider_config
            .as_ref()
            .map(|config| config.model.clone()),
        tool_call_id: Some(call.id.clone()),
        plan_mode: session.is_plan_mode(),
        read_state: Arc::clone(&session.read_state),
        cancel: Arc::clone(&session.cancel),
        todos: Arc::clone(&session.todos),
        provider_config: deps.provider_config.clone(),
        ask_user: host.as_ref().map(|_| {
            Arc::new(crate::tools::ask_user::AskUserPrompter::new(Arc::clone(
                session,
            )))
        }),
        plan: host.as_ref().map(|host| {
            Arc::new(crate::tools::plan::PlanController::new(
                Arc::clone(host),
                Arc::clone(session),
            ))
        }),
        subagent: host.as_ref().map(|host| {
            Arc::new(crate::subagent::SubagentRunner::new(
                Arc::clone(host),
                Arc::clone(session),
            ))
        }),
        background: Some(Arc::clone(&session.background)),
        sessions: host.as_ref().map(|host| {
            Arc::new(crate::tools::read_session_context::HostSessionContext::new(
                Arc::clone(host),
            )) as Arc<dyn crate::tools::read_session_context::SessionContextPort>
        }),
        workflow: host.as_ref().and_then(|host| host.workflow_port()),
        automation: host.as_ref().and_then(|host| host.automation_port()),
        browser: host.as_ref().and_then(|host| host.browser_port()),
        computer: host.as_ref().and_then(|host| host.computer_port()),
        offpeak: None,
    };
    let authorization = match (&sync_tool, &async_tool) {
        (Some(tool), _) => deps.registry.authorize(tool.as_ref(), &call.input, &ctx),
        (None, Some(tool)) => deps
            .registry
            .authorize_async(tool.as_ref(), &call.input, &ctx),
        (None, None) => unreachable!("checked above"),
    };
    match authorization {
        crate::tools::Authorization::Allow => {}
        // PreToolUse hook 明确 allow：跳过询问（hook 就是这次的授权决定）。
        crate::tools::Authorization::Ask if hook_allowed => {}
        crate::tools::Authorization::Ask => {
            // PermissionRequest hook：allow/deny 是最终决定；未表态才进 broker。
            let permission_hook = session
                .run_hook_event_blocking(
                    crate::workspace_hooks::HookEventName::PermissionRequest,
                    Some(call.name.clone()),
                    crate::workspace_hooks::runtime::HookInput {
                        session_id: session.id.clone(),
                        cwd: session.workspace_root.to_string_lossy().to_string(),
                        mode: session.collaboration_mode(),
                        tool_name: Some(call.name.clone()),
                        tool_input: Some(call.input.clone()),
                        tool_call_id: Some(call.id.clone()),
                        ..Default::default()
                    },
                    turn_id.to_string(),
                    "toolBefore",
                    Some(call.id.clone()),
                )
                .await;
            if let Some(error) = &permission_hook.hook_error {
                return Err(ToolError::new(
                    ToolErrorCode::ExecutionFailed,
                    format!("workspace hook failed: {error}"),
                ));
            }
            match permission_hook.result.permission_behavior {
                Some(crate::workspace_hooks::runtime::PermissionBehavior::Deny) => {
                    return Err(ToolError::new(
                        ToolErrorCode::PermissionDenied,
                        format!(
                            "{} was denied by a workspace hook{}",
                            call.name,
                            permission_hook
                                .result
                                .permission_decision_reason
                                .as_deref()
                                .map(|reason| format!(": {reason}"))
                                .unwrap_or_default()
                        ),
                    ));
                }
                Some(crate::workspace_hooks::runtime::PermissionBehavior::Allow) => {}
                _ => {}
            }
            if permission_hook.result.permission_behavior
                == Some(crate::workspace_hooks::runtime::PermissionBehavior::Allow)
            {
                // hook 已授权，跳过 broker。
            } else {
                let summary = format!("{} requires your approval", call.name);
                match session
                    .request_permission(&call.id, &call.name, &summary, &call.input)
                    .await
                {
                    crate::host_interactions::PermissionOutcome::Allowed => {}
                    crate::host_interactions::PermissionOutcome::Denied => {
                        return Err(ToolError::new(
                            ToolErrorCode::PermissionDenied,
                            format!("{} was denied by the user", call.name),
                        ));
                    }
                    crate::host_interactions::PermissionOutcome::Cancelled => {
                        return Err(ToolError::new(
                            ToolErrorCode::Cancelled,
                            format!("{} approval was cancelled", call.name),
                        ));
                    }
                }
            }
        }
        crate::tools::Authorization::Deny(message) => {
            return Err(ToolError::new(ToolErrorCode::PermissionDenied, message));
        }
    }
    // 文件变更台账：必须在执行**之前**抓"改动前"快照——工具落盘之后就再也拿不到
    // 旧内容，而没有旧内容就没有安全的回滚（rewind 会把它判成不可回滚）。
    let contract = sync_tool
        .as_ref()
        .map(|tool| tool.contract())
        .or_else(|| async_tool.as_ref().map(|tool| tool.contract()))
        .expect("a tool was selected above");
    let pending_change = crate::file_ledger::capture_before(
        &ctx,
        contract.read_only,
        contract.side_effect_scope,
        &contract.name,
        &call.input,
    );
    let input = call.input.clone();
    let result = if let Some(tool) = sync_tool {
        // 阻塞池：同步工具会跑子进程 / nc-tools Kernel / HTTP，不能占住 turn 的
        // async 运行时线程；串行语义由调用点保证。
        let tool = Arc::clone(&tool);
        tokio::task::spawn_blocking(move || tool.execute(&input, &ctx))
            .await
            .map_err(|error| {
                ToolError::new(
                    ToolErrorCode::ExecutionFailed,
                    format!("tool task failed: {error}"),
                )
            })?
    } else if let Some(tool) = async_tool {
        // 交互/子代理/工作流工具必须等待运行时事件，不能进阻塞池。
        tool.execute(&input, &ctx).await
    } else {
        unreachable!("checked above")
    };
    // 只有工具真的成功才记账：失败的调用没有改变工作区，记进去会让 rewind
    // 去"恢复"一个从未存在过的版本。
    if let (Some(pending), Ok(_)) = (pending_change, &result) {
        // 记账失败不推翻本轮：文件已经真的改了，工具结果也已经是真的。但台账缺一块
        // 就意味着这一条永远回滚不了，所以必须出声（typed fault 名进 stderr）。
        if let Err(error) = crate::file_ledger::record_after(
            session.file_ledger(),
            turn_id,
            &call.id,
            &contract.name,
            pending,
        ) {
            eprintln!(
                "[zcode-host] fault.fileLedgerNotPersisted ({turn_id}/{contract_name}):                  the file changed but cannot be rewound: {error}",
                contract_name = contract.name
            );
        }
    }
    result
}

/// 单模型回合（含 provider executor 的重试/分类/流式拼装）。
#[allow(clippy::too_many_arguments)]
async fn run_model_roundtrip(
    session: &Arc<Session>,
    config: &ProviderConfig,
    deps: &TurnDeps,
    tools: &[ToolDefinition],
    history: &[Message],
    row_id: u64,
    turn_id: &str,
    lines: &mut Vec<String>,
) -> Result<RoundResult, AdapterError> {
    let response_id = format!("resp-{turn_id}");
    let provider_kind = config.provider_kind().map_err(|error| {
        AdapterError::new(ModelErrorCode::ProviderNotConfigured, error.to_string())
    })?;
    let options = RequestBodyOptions {
        model: &config.model,
        system: &deps.context.system_blocks,
        messages: history,
        tools,
        max_tokens: config.max_tokens,
        stream: config.stream,
    };
    let body = match provider_kind {
        ProviderKind::Anthropic => build_anthropic_messages_body(&options),
        ProviderKind::OpenaiCompatible => build_openai_compatible_body(&options),
    }?;
    let body_json = serde_json::to_string(&body).map_err(|error| {
        AdapterError::new(ModelErrorCode::InvalidModelRequest, error.to_string())
    })?;

    let mut headers = auth_headers(&config.api_key, resolved_auth_style(config, provider_kind));
    // 自定义头（TS `api.headers` → `ProviderConfig.headers`）在鉴权头之后追加；
    // 同名头后者胜出（大小写不敏感），对齐 `mergeModelRequestHeaders`。
    for (name, value) in &config.headers {
        let name = name.trim().to_string();
        let value = value.trim().to_string();
        if name.is_empty() || value.is_empty() {
            continue;
        }
        let lower = name.to_lowercase();
        headers.retain(|(existing, _)| existing.to_lowercase() != lower);
        headers.push((name, value));
    }
    let transport_request = TransportRequest {
        url: config.endpoint_url(),
        // 鉴权头按 config 的 authStyle 选择（缺省 kind 原生：anthropic → x-api-key +
        // anthropic-version，openai-compatible → Authorization: Bearer）。
        headers,
        body: body_json,
    };
    let stream_request = StreamRequest {
        transport_request,
        provider_kind,
        retry: nc_code_provider::retry::RetryOptions::default(),
        retry_budget: None,
        off_peak: false,
        preserve_provider_stream_boundaries: false,
        stream_idle_timeout_base_ms: None,
        stream_idle_timeout_retry_number: None,
        is_cancelled: Arc::new({
            let session = Arc::clone(session);
            move || session.cancel.load(std::sync::atomic::Ordering::SeqCst)
        }),
    };

    let timing = nc_code_provider::timing::StreamTiming::new();
    let mut transport =
        nc_code_provider::timing::TimedTransport::new(HttpTransport::new(), timing.clone());
    let clock = RealClock;
    session.record_model_request();
    let execution = executor::execute(&mut transport, &clock, &stream_request).await;
    let request_id = format!("req-{turn_id}");
    if let Some(host) = deps.host.get().and_then(|weak| weak.upgrade()) {
        host.ttft_mark_request(
            turn_id,
            &request_id,
            &config.base_url,
            &config.model,
            &execution.traces,
        );
    }
    if let Some(failure) = execution.failure {
        session.record_model_error();
        return Err(failure);
    }

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut usage: Option<ModelUsage> = None;
    let mut first_output_marked = false;
    // 当前打开的 reasoning 行：(rowId, 累计正文, 开启时刻 ms)。
    let mut reasoning: Option<(u64, String, i64)> = None;
    for event in &execution.events {
        match event {
            ModelStreamEvent::TextDelta { text: chunk, .. } => {
                text.push_str(chunk);
                if !first_output_marked {
                    first_output_marked = true;
                    if let Some(host) = deps.host.get().and_then(|weak| weak.upgrade()) {
                        host.ttft_mark_first_output(turn_id, "text");
                    }
                }
            }
            ModelStreamEvent::ReasoningStart { .. } => {
                if let Some((row_id, acc, started)) = reasoning.take() {
                    lines.extend(session.project_reasoning_final(
                        row_id,
                        turn_id,
                        &response_id,
                        &acc,
                        "complete",
                        Some(now_ms() - started),
                    ));
                }
                let (row_id, mut open_lines) =
                    session.project_reasoning_start(turn_id, &response_id);
                lines.append(&mut open_lines);
                reasoning = Some((row_id, String::new(), now_ms()));
            }
            ModelStreamEvent::ReasoningDelta { text: delta, .. } => {
                if reasoning.is_none() {
                    let (row_id, mut open_lines) =
                        session.project_reasoning_start(turn_id, &response_id);
                    lines.append(&mut open_lines);
                    reasoning = Some((row_id, String::new(), now_ms()));
                }
                if let Some((row_id, acc, _)) = reasoning.as_mut() {
                    acc.push_str(delta);
                    let row_id = *row_id;
                    lines.extend(session.project_reasoning_delta(row_id, delta));
                }
            }
            ModelStreamEvent::ReasoningEnd { .. } => {
                if let Some((row_id, acc, started)) = reasoning.take() {
                    lines.extend(session.project_reasoning_final(
                        row_id,
                        turn_id,
                        &response_id,
                        &acc,
                        "complete",
                        Some(now_ms() - started),
                    ));
                }
            }
            ModelStreamEvent::ToolCall { tool_call } => tool_calls.push(tool_call.clone()),
            ModelStreamEvent::Finish {
                usage: finish_usage,
                ..
            } => {
                // 累加而非覆盖：多轮 tool loop 每轮都有 Finish，覆盖会丢前几轮用量。
                session.accumulate_usage(&cumulative_usage(finish_usage));
                usage = Some(finish_usage.clone());
            }
            ModelStreamEvent::Error { error } => {
                if let Some((row_id, acc, started)) = reasoning.take() {
                    lines.extend(session.project_reasoning_final(
                        row_id,
                        turn_id,
                        &response_id,
                        &acc,
                        "interrupted",
                        Some(now_ms() - started),
                    ));
                }
                return Err(AdapterError::new(
                    ModelErrorCode::InvalidModelResponse,
                    error.to_string(),
                ));
            }
            _ => {}
        }
    }
    // 流结束时仍打开的 reasoning 收口为 complete（正常路径）。
    if let Some((row_id, acc, started)) = reasoning.take() {
        lines.extend(session.project_reasoning_final(
            row_id,
            turn_id,
            &response_id,
            &acc,
            "complete",
            Some(now_ms() - started),
        ));
    }
    if !text.is_empty() {
        lines.extend(session.project_assistant_delta(row_id, &text));
    }
    // The debug ledger observes this physical request: attempt traces are the
    // transport facts, `usage` is the provider's own Finish event.
    let started_at = execution
        .traces
        .first()
        .map(|trace| trace.started_at_ms)
        .unwrap_or(0);
    let ended_at = execution
        .traces
        .last()
        .and_then(|trace| trace.ended_at_ms)
        .unwrap_or(started_at);
    let provider_kind_name = match provider_kind {
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::OpenaiCompatible => "openai-compatible",
    };
    if let Ok(mut ledger) = session.debug_ledger.lock() {
        ledger.record_request(
            &crate::session_debug::NetworkContext {
                trace_id: &session.log_epoch(),
                request_id: &request_id,
                query_source: "main",
                provider_id: config.provider_id.as_deref(),
                model_id: &config.model,
                provider_kind: provider_kind_name,
                base_url: &config.base_url,
                max_attempts: 1,
            },
            &execution.traces,
            Some(crate::session_debug::RoundInput {
                request_id: &request_id,
                started_at_ms: started_at,
                ended_at_ms: ended_at,
                first_content_at_ms: None,
                usage: usage.as_ref(),
            }),
        );
    }

    let throughput = measured_throughput(&timing, usage.as_ref());
    Ok(RoundResult {
        text,
        tool_calls,
        throughput,
    })
}

/// `specs/model-catalog-and-tps.md` §3: the host measures tok/s from the real
/// streaming window (first→last chunk) and the provider's own completion-token
/// usage; nothing is fabricated when the window is absent.
fn measured_throughput(
    timing: &nc_code_provider::timing::StreamTiming,
    usage: Option<&ModelUsage>,
) -> Option<Value> {
    let (_, _, _) = timing.window()?;
    let completion_tokens = usage.and_then(|usage| usage.output_tokens).unwrap_or(0);
    let elapsed_ms = timing.generation_ms();
    let tokens_per_second = if elapsed_ms > 0 {
        (completion_tokens as f64) * 1000.0 / (elapsed_ms as f64)
    } else {
        0.0
    };
    Some(json!({
        "completionTokens": completion_tokens,
        "elapsedMs": elapsed_ms,
        "tokensPerSecond": tokens_per_second,
        "windowSource": "generation",
        "estimated": false,
    }))
}

pub fn cumulative_usage(usage: &ModelUsage) -> Value {
    json!({
        "inputTokens": usage.input_tokens.unwrap_or(0),
        "outputTokens": usage.output_tokens.unwrap_or(0),
        "reasoningTokens": usage.reasoning_tokens.unwrap_or(0),
        "cacheReadTokens": usage.cache_read_tokens.unwrap_or(0),
        "cacheWriteTokens": usage.cache_write_tokens.unwrap_or(0),
    })
}

/// 出站鉴权头：由 `authStyle` 决定（`bearer` → Authorization；否则 x-api-key +
/// anthropic-version）。kind 原生缺省在 `ProviderConfig::auth_style` 里解析，
/// 这里只认最终样式——live endpoint 实测抓出的形状错误，回归锁在此。
pub fn auth_headers(api_key: &str, auth_style: &str) -> Vec<(String, String)> {
    match auth_style {
        crate::provider_config::AUTH_STYLE_BEARER => vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("authorization".to_string(), format!("Bearer {api_key}")),
        ],
        _ => vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("x-api-key".to_string(), api_key.to_string()),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
        ],
    }
}

/// 落盘本轮用量事实（`v4/usage/stats` 的唯一 durable 写路径）。
///
/// 事实来源是会话自身的累计器与投影行：token 累计来自每一轮 `Finish`（多轮 tool
/// loop 相加），工具调用/错误数来自本轮 toolCall 行的稳定状态，模型来自会话配置，
/// 起止时间来自 turnHeader。**只写已有事实**：没有 usage 的轮次（例如被 hook 拒绝
/// 后立即失败的轮）照样落一条，token 记 0——读面因此能区分"没有用量"与"缺事实"。
fn persist_usage_fact(
    session: &Arc<Session>,
    turn_id: &str,
    source_command_id: &str,
    header_state: &str,
    model_label: Option<&str>,
) {
    let usage = session.usage();
    let cumulative = usage
        .get("cumulative")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let field = |name: &str| cumulative.get(name).and_then(Value::as_u64).unwrap_or(0);
    let rows = session.rows_snapshot();
    let header = rows.iter().find(|row| {
        row.get("kind").and_then(Value::as_str) == Some("turnHeader")
            && row.get("sourceCommandId").and_then(Value::as_str) == Some(source_command_id)
    });
    let started_at = header
        .and_then(|row| row.get("startedAt").and_then(Value::as_i64))
        .unwrap_or_else(now_ms);
    let ended_at = header
        .and_then(|row| row.get("endedAt").and_then(Value::as_i64))
        .unwrap_or_else(now_ms)
        .max(started_at);
    let tool_rows: Vec<&Value> = rows
        .iter()
        .filter(|row| {
            row.get("kind").and_then(Value::as_str) == Some("toolCall")
                && row.get("turnId").and_then(Value::as_str) == Some(turn_id)
        })
        .collect();
    let tool_call_count = tool_rows.len() as u64;
    let tool_error_count = tool_rows
        .iter()
        .filter(|row| {
            row.get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| status == "error" || status == "cancelled")
        })
        .count() as u64;
    // 工具总耗时：只有终态行带 startedAt/endedAt，缺失的按 0 计（不猜）。
    let tool_total_ms: i64 = tool_rows
        .iter()
        .map(|row| {
            let start = row.get("startedAt").and_then(Value::as_i64).unwrap_or(0);
            let end = row.get("endedAt").and_then(Value::as_i64).unwrap_or(start);
            (end - start).max(0)
        })
        .sum();
    let model = session.model();
    let provider = session.provider();
    // The session's own model config is only populated by a `switchModelConfig` command;
    // a fresh session runs on the host provider config, which is the model actually
    // billed, so prefer the label the turn was executed with.
    let model = if model.is_empty() {
        model_label.unwrap_or_default().to_string()
    } else {
        model
    };
    // Per-tool rows: the aggregate ranks by name without re-scanning the transcript.
    // Built outside `json!` because the duration is a statement (a block is not a value).
    let tool_facts: Vec<Value> = tool_rows
        .iter()
        .map(|row| {
            let start = row.get("startedAt").and_then(Value::as_i64).unwrap_or(0);
            let end = row.get("endedAt").and_then(Value::as_i64).unwrap_or(start);
            json!({
                "name": row.get("toolName").and_then(Value::as_str).unwrap_or(""),
                "error": row
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| status == "error" || status == "cancelled"),
                "durationMs": (end - start).max(0) as u64,
            })
        })
        .collect();
    let fact = json!({
        "turnId": turn_id,
        "sessionId": session.id,
        "state": header_state,
        "startedAt": started_at,
        "endedAt": ended_at,
        "durationMs": (ended_at - started_at).max(0),
        "modelId": if model.is_empty() { Value::Null } else { json!(model) },
        "providerId": if provider.is_empty() { Value::Null } else { json!(provider) },
        "inputTokens": field("inputTokens"),
        "outputTokens": field("outputTokens"),
        "reasoningTokens": field("reasoningTokens"),
        "cacheReadTokens": field("cacheReadTokens"),
        "cacheCreationTokens": field("cacheWriteTokens"),
        "toolCallCount": tool_call_count,
        "toolErrorCount": tool_error_count,
        "toolTotalMs": tool_total_ms,
        "modelRequestCount": session.turn_model_request_count(),
        "modelErrorCount": session.turn_model_error_count(),
        "tools": tool_facts,
    });
    if let Err(error) = session.append_usage_fact(&fact) {
        // durable 写失败不能让本轮失败，但必须出声：静默丢事实会让聚合读面少一段。
        eprintln!("[zcode-host] usage fact persistence failed ({turn_id}): {error}");
    }
}

fn finish_turn(
    session: &Arc<Session>,
    turn_id: &str,
    source_command_id: &str,
    header_state: &str,
    turn_phase: &str,
    model_label: Option<&str>,
    lines: &mut Vec<String>,
) {
    persist_usage_fact(
        session,
        turn_id,
        source_command_id,
        header_state,
        model_label,
    );
    lines.extend(session.project_turn_finished(turn_id, source_command_id, header_state));
    session.set_phase(turn_phase);
    // 队列暂停语义（TS queueState.pauseReason）：stop → stopped 且暂停队列；
    // turn 错误 → error；成功 → 清除暂停原因。
    match header_state {
        "completedInterrupted" => {
            session.set_queue_auto_drain(false);
            session.set_queue_pause_reason(Some("stopped"));
        }
        "failed" => session.set_queue_pause_reason(Some("error")),
        _ => session.set_queue_pause_reason(None),
    }
    // WHY bump 在投影**之前**：revision 必须与客户端收到的权威投影同步前移。
    // 原来由 `settle` 在收帧之后静默 +1，客户端水位永远落后一格，紧接着的
    // CAS 命令（editQueueItem / retryTurn / setAssistantFeedback）会被全部判成
    // `proto.staleRevision` —— e2e `branch_rerun_e2e` 直接复现了这个失败。
    session.bump_revision();
    lines.extend(emit_state_update(Arc::clone(session)));
    // 终态强制 flush：turn 的最后一帧不依赖 ticker 时序，协议上确定性收口。
    lines.extend(session.project_deltas(Vec::new(), true));
}

/// control + revision + usage 的 state.updated 投影（字段级整体替换）。
fn emit_state_update(session: Arc<Session>) -> Vec<String> {
    let phase = session.phase();
    let works = session.active_works();
    let control = snapshot::control(
        &phase,
        session.is_running(),
        session.cancel.load(std::sync::atomic::Ordering::SeqCst),
        session.last_error().as_ref(),
        &works,
    );
    let patch = json!({
        "control": control,
        "revision": session.revision(),
        "usage": session.usage(),
        "queue": session.queue_state(),
        "goal": session.goal_projection(),
        "availability": snapshot::availability(session.goal_status().as_deref()),
        "inputRouting": session.input_routing_value(),
    });
    session.project_deltas(vec![rows::state_updated(patch)], false)
}

/// 手动压缩命令（`compact`）：对会话历史做 microcompact + LLM 摘要压缩，
/// 并以 `replace_history` 持久化。Rust 侧没有 TS 的事件/投影基础设施，因此
/// 只做压缩 + settle，不投影 turn 行（与 Rust host 的简化模型一致）。
pub async fn run_compact(
    session: Arc<Session>,
    provider: Option<&ProviderConfig>,
    deps: &TurnDeps,
    admitted: Admitted,
) -> Vec<String> {
    let source_command_id = admitted.envelope.command_id.clone();
    // compact 是维护命令，不改会话生命周期相位；结束后恢复进入前的合法 phase
    // （sessionPhaseSchema 没有 "idle"，旧实现留下的 "idle" 会让客户端校验失败）。
    let previous_phase = session.phase();
    let mut lines: Vec<String> = Vec::new();
    session.set_phase("running");
    session.set_active_command(Some(source_command_id.clone()));
    session.set_eager_frames(true);

    let result: Result<(), AdapterError> = match provider {
        Some(config) => match session.history_snapshot() {
            Ok(mut history) => {
                let mut compacted = false;
                if turn_context::microcompact_messages(&mut history) > 0 {
                    compacted = true;
                }
                if history.len() > COMPACT_HISTORY_THRESHOLD_MESSAGES {
                    let cancel_probe: Arc<dyn Fn() -> bool + Send + Sync> = {
                        let session = Arc::clone(&session);
                        Arc::new(move || session.cancel.load(std::sync::atomic::Ordering::SeqCst))
                    };
                    if let Ok(summary) = crate::compact::compact_history(
                        config,
                        &deps.context,
                        &history,
                        cancel_probe,
                    )
                    .await
                    {
                        if summary.len() < history.len() {
                            history = summary;
                            compacted = true;
                        }
                    }
                }
                if compacted {
                    if let Err(error) = session.replace_history(&history) {
                        eprintln!(
                            "[zcode-host] {FAULT_HISTORY_NOT_PERSISTED} ({source_command_id}): {error}"
                        );
                        Err(AdapterError::new(
                            ModelErrorCode::ModelRequestFailed,
                            FAULT_HISTORY_NOT_PERSISTED.to_string(),
                        ))
                    } else {
                        Ok(())
                    }
                } else {
                    Ok(())
                }
            }
            Err(error) => {
                eprintln!("[zcode-host] {FAULT_HISTORY_NOT_PERSISTED}: {error}");
                Err(AdapterError::new(
                    ModelErrorCode::ModelRequestFailed,
                    FAULT_HISTORY_NOT_PERSISTED.to_string(),
                ))
            }
        },
        None => Err(AdapterError::new(
            ModelErrorCode::ProviderNotConfigured,
            "no provider configured; pass --provider-config to enable model calls",
        )),
    };

    match result {
        Ok(()) => {
            settle(
                &session,
                admitted,
                CommandStatus::Accepted,
                None,
                Some(json!({"type": "compact"})),
            );
        }
        Err(error) => {
            let code = if error.code == ModelErrorCode::ProviderNotConfigured {
                FAULT_PROVIDER_NOT_CONFIGURED.to_string()
            } else {
                FAULT_HISTORY_NOT_PERSISTED.to_string()
            };
            settle(&session, admitted, CommandStatus::Failed, Some(code), None);
        }
    }

    session.set_phase(&previous_phase);
    // compact 改的是模型历史（客户端不渲染，但确实是权威投影）：与 turn 同规则，
    // revision 必须随一条 `state.updated` 一起前移，不能静默 +1。
    session.bump_revision();
    lines.extend(emit_state_update(Arc::clone(&session)));
    lines
}

/// settle：把终态写回 inbox 并持久化（幂等重提的权威事实）。
fn settle(
    session: &Arc<Session>,
    admitted: Admitted,
    status: CommandStatus,
    reason_code: Option<String>,
    result: Option<Value>,
) {
    let final_ = match (&status, &reason_code) {
        (CommandStatus::Accepted, _) => CommandFinal::accepted(),
        (_, Some(code)) => CommandFinal::with_reason(status, code.clone()),
        (_, None) => CommandFinal::accepted(),
    };
    let final_ = CommandFinal { result, ..final_ };
    let merged = nc_code_core::protocol::merge_final(&admitted.ack, &final_);
    let merged_value = serde_json::to_value(&merged).unwrap_or(Value::Null);
    if let Err(error) = session.persist_ack(&admitted.envelope.command_id, &merged_value) {
        eprintln!(
            "[zcode-host] persist_ack failed for {}: {error}",
            admitted.envelope.command_id
        );
    }
    let _ = session.inbox.settle(admitted.ticket, final_);
    // 前台执行结束：清除 stop 护栏的目标，避免迟到的 stop 误杀下一轮；
    // 同时关闭 eager 帧投递（终态帧已在 finish_turn 的 forced flush 中投递）。
    session.set_active_command(None);
    session.set_eager_frames(false);
    // 注意：这里**不**推进 revision。revision 与"客户端已收到的权威投影"必须一一对应，
    // 静默 +1 会让所有后续 CAS 命令 stale-by-one（见 finish_turn 的注释）。
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_headers_match_provider_shape() {
        let openai = auth_headers("k", crate::provider_config::AUTH_STYLE_BEARER);
        assert!(openai
            .iter()
            .any(|(name, value)| name == "authorization" && value == "Bearer k"));
        assert!(!openai.iter().any(|(name, _)| name == "x-api-key"));

        let anthropic = auth_headers("k", crate::provider_config::AUTH_STYLE_X_API_KEY);
        assert!(anthropic
            .iter()
            .any(|(name, value)| name == "x-api-key" && value == "k"));
        assert!(anthropic
            .iter()
            .any(|(name, value)| name == "anthropic-version" && value == "2023-06-01"));
        assert!(!anthropic.iter().any(|(name, _)| name == "authorization"));
    }

    #[test]
    fn auth_style_override_beats_kind_default() {
        let config = ProviderConfig {
            provider_id: None,
            kind: "anthropic".to_string(),
            base_url: "https://gw.example/v1".to_string(),
            api_key: "k".to_string(),
            model: "m".to_string(),
            max_tokens: 1024,
            auth_style: Some("bearer".to_string()),
            stream: true,
            extra_models: Vec::new(),
            reasoning_levels: Vec::new(),
            headers: Vec::new(),
        };
        assert_eq!(config.auth_style().expect("style"), "bearer");
        let headers = auth_headers(&config.api_key, config.auth_style().expect("style"));
        assert!(headers.iter().any(|(name, _)| name == "authorization"));
        // 缺省时 anthropic 用原生 x-api-key。
        let mut native = config.clone();
        native.auth_style = None;
        assert_eq!(native.auth_style().expect("style"), "x-api-key");
    }
}
