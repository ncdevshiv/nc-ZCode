//! 会话创建的唯一路径：`createSession`、`forkAssistant` 与
//! `createSelectionSideSession` 都只调用 [`HostServer::open_session`]。
//!
//! WHY 抽出来：三条命令的差别只在"这个会话一开始带着什么内容"，落盘顺序、id 生成、
//! 安全校验、注册与 ticker 派发生要是各写一遍，就会出现"fork 出来的 child 忘了挂
//! frame sink → 子会话的帧永远发不出去"这一类只在某一条路径上发生的 bug。
//! 现在这些只有一份。

use std::sync::Arc;

use nc_code_provider::request::Message;
use serde_json::Value;

use crate::file_ledger::FileChangeRecord;
use crate::rpc::{RpcError, CODE_INTERNAL};
use crate::session::{Session, SessionInit};
use crate::store::{is_safe_session_id, now_ms, SessionMeta};
use crate::HostServer;

/// 新会话的初始内容。字段都是"事实"，不含命令语义。
pub struct SessionSeed {
    pub title: String,
    pub title_source: String,
    pub followup_mode: String,
    pub collaboration_mode: String,
    pub provider: String,
    pub model: String,
    pub thought: String,
    pub rows: Vec<Value>,
    pub history: Vec<Message>,
    pub turn_history_starts: Vec<(String, usize)>,
    pub file_changes: Vec<FileChangeRecord>,
    /// 继承的目标（fork 克隆；其余会话为 None）。
    pub goal: Option<crate::goal::GoalRecord>,
    /// `subagent_child` for delegated child sessions; `None` for user sessions.
    pub task_type: Option<String>,
    /// Parent session id for forked/child sessions.
    pub parent_session_id: Option<String>,
}

impl SessionSeed {
    /// 空会话（`createSession` 的缺省形态）。
    pub fn blank() -> Self {
        Self {
            title: String::new(),
            title_source: "default".to_string(),
            followup_mode: "queue".to_string(),
            collaboration_mode: "build".to_string(),
            provider: String::new(),
            model: String::new(),
            thought: String::new(),
            rows: Vec::new(),
            history: Vec::new(),
            turn_history_starts: Vec::new(),
            file_changes: Vec::new(),
            goal: None,
            task_type: None,
            parent_session_id: None,
        }
    }

    /// 继承父会话的运行配置（模型档位、协作模式等），但**不**带任何内容。
    /// TS 的 `createSelectionSideSession` 注释就是这条：服务端从父 record 派生完整运行配置。
    pub fn from_parent(parent: &Arc<Session>) -> Self {
        Self {
            followup_mode: parent.followup_mode(),
            collaboration_mode: parent.collaboration_mode(),
            provider: parent.provider(),
            model: parent.model(),
            thought: parent.thought(),
            ..Self::blank()
        }
    }
}

impl HostServer {
    /// 落盘 → 构造 → 注册（挂 frame sink + 派生 flush ticker）的唯一序列。
    pub fn open_session(&self, seed: SessionSeed) -> Result<Arc<Session>, RpcError> {
        let session_id = self.next_id("session");
        if !is_safe_session_id(&session_id) {
            return Err(RpcError::new(
                CODE_INTERNAL,
                "generated session id failed safety check",
            ));
        }
        let meta = SessionMeta {
            session_id: session_id.clone(),
            title: seed.title.clone(),
            title_source: seed.title_source.clone(),
            log_epoch: self.next_id("epoch"),
            revision: 0,
            workspace_identity: self.workspace_identity.clone(),
            created_at: now_ms(),
            updated_at: now_ms(),
            task_type: seed.task_type.clone(),
            parent_session_id: seed.parent_session_id.clone(),
            followup_mode: seed.followup_mode.clone(),
            collaboration_mode: seed.collaboration_mode.clone(),
            plan_previous_mode: String::new(),
            provider: seed.provider.clone(),
            model: seed.model.clone(),
            thought: seed.thought.clone(),
            goal: seed.goal.clone(),
        };
        self.store
            .create_session(&meta)
            .map_err(|error| RpcError::new(CODE_INTERNAL, error.to_string()))?;
        // 继承来的内容必须先 durable 再注册：注册后客户端立刻能 subscribe 拿到 snapshot，
        // 那一刻它看到的行必须与磁盘一致。
        for row in &seed.rows {
            self.store
                .append_row(&session_id, row)
                .map_err(|error| RpcError::new(CODE_INTERNAL, error.to_string()))?;
        }
        for message in &seed.history {
            self.store
                .append_history(&session_id, message)
                .map_err(|error| RpcError::new(CODE_INTERNAL, error.to_string()))?;
        }
        for record in &seed.file_changes {
            self.store
                .append_file_change(&session_id, record)
                .map_err(|error| RpcError::new(CODE_INTERNAL, error.to_string()))?;
        }
        let next_row_id = seed
            .rows
            .iter()
            .filter_map(|row| row.get("rowId").and_then(Value::as_u64))
            .max()
            .map(|max| max + 1)
            .unwrap_or(0);
        let session = Session::new(SessionInit {
            id: session_id,
            workspace_root: self.workspace.clone(),
            workspace_identity: self.workspace_identity.clone(),
            created_at: meta.created_at,
            task_type: meta.task_type.clone(),
            parent_session_id: meta.parent_session_id.clone(),
            log_epoch: meta.log_epoch,
            title: meta.title,
            title_source: meta.title_source,
            rows: seed.rows,
            revision: 0,
            transcript_acks: std::collections::HashMap::new(),
            next_row_id,
            history: seed.history,
            turn_history_starts: seed.turn_history_starts,
            file_changes: seed.file_changes,
            followup_mode: seed.followup_mode,
            collaboration_mode: seed.collaboration_mode,
            provider: seed.provider,
            model: seed.model,
            thought: seed.thought,
            goal: seed.goal,
            hooks: self.build_hook_admission(&self.workspace_identity),
            store: self.store.clone(),
        });
        session.ensure_hook_review(now_ms());
        session.run_session_start_hooks("startup");
        // Background completions must reach the model as a `backgroundResult` turn;
        // the registry holds the delivery point, the host owns the turn scheduler.
        if let Some(host) = self.self_weak.get().and_then(|weak| weak.upgrade()) {
            session.background.set_notifier(Arc::new(
                crate::background_notifier::HostBackgroundNotifier::new(
                    Arc::downgrade(&host),
                    session.id.clone(),
                ),
            ));
            // A running Bash row's `outputPreview` is projected from the background
            // watchdog (plain OS thread); the sink is the same weak delivery pattern.
            session.background.set_preview_sink(Arc::new(
                crate::background_notifier::HostToolPreviewSink::new(
                    Arc::downgrade(&host),
                    session.id.clone(),
                ),
            ));
        }
        self.register_session(Arc::clone(&session));
        Ok(session)
    }
}
