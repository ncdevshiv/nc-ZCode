//! 会话持久化：JSONL transcript。
//!
//! 本 increment 的存储后端（specs/rust-native-crates.md S3 验收明示 OPEN）：TS 侧
//! 的 sqlite 单写者存储尚未移植，host 用追加式 JSONL 落盘行、命令终态与会话模型
//! 历史，保证「重启 → resume 行不丢、幂等重提可回放、模型记忆不丢」这三个验收点
//! 可测。布局：
//!
//! ```text
//! <data-dir>/sessions/<sessionId>/meta.json          # 会话元信息（title/epoch/revision）
//! <data-dir>/sessions/<sessionId>/transcript.jsonl   # 见下表的追加条目
//! ```
//!
//! transcript 的 5 种条目（`t` 标签，载荷形状见 `TranscriptEntry`）：
//!
//! | 标签                 | 语义                                              |
//! | -------------------- | ------------------------------------------------- |
//! | `row`                | 新增一行投影                                      |
//! | `upsert`             | 同行替换（按 `rowId` 覆盖）                       |
//! | `ack`                | 命令终态（幂等重提回源）                          |
//! | `history`            | 追加一条模型历史消息（ENTRY 027）                 |
//! | `historyCompacted`   | 压缩后**替换**整份模型历史（ENTRY 027）           |
//!
//! 写路径只有 append（四类事实都是追加），`historyCompacted` 是**替换事件**而不是
//! 文件重写，所以压缩也不破坏这条不变量。meta.json 是唯一可重写文件，用
//! 「写临时文件 + rename」保证原子性。
//!
//! 回放是**容错**的：坏行与未知标签被跳过并记入 `Transcript::skipped_lines`，其余
//! 行照常恢复（见 `parse_entry` 与 `load_transcript` 的注释）。

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use nc_code_provider::request::Message;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub title: String,
    pub title_source: String,
    pub log_epoch: String,
    pub revision: i64,
    pub workspace_identity: String,
    pub created_at: i64,
    /// Last real state write (ms). Missing in old meta files → 0, callers fall back
    /// to `created_at`.
    #[serde(default)]
    pub updated_at: i64,
    /// `subagent_child` marks a child session excluded from the task list.
    #[serde(default, rename = "taskType", skip_serializing_if = "Option::is_none")]
    pub task_type: Option<String>,
    /// Parent session id for forked/subagent child sessions.
    #[serde(
        default,
        rename = "parentSessionId",
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_session_id: Option<String>,
    /// 会话级配置（setFollowupMode/switchModelConfig/switchCollaborationMode 的
    /// 持久化落点）。serde default 保证旧 meta.json（无这些键）仍可加载。
    #[serde(default = "default_followup_mode")]
    pub followup_mode: String,
    #[serde(default = "default_collaboration_mode")]
    pub collaboration_mode: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub thought: String,
    /// Mode to restore when leaving plan mode (`""` = none).
    #[serde(default, rename = "planPreviousMode")]
    pub plan_previous_mode: String,
    /// 会话目标（TS 的 `SessionGoal` 持久行）。serde default 保证旧 meta.json 仍可加载。
    #[serde(default)]
    pub goal: Option<crate::goal::GoalRecord>,
}

fn default_followup_mode() -> String {
    "queue".to_string()
}

fn default_collaboration_mode() -> String {
    "build".to_string()
}

#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Corrupt {
        path: PathBuf,
        message: String,
    },
    /// 会话历史锁中毒：持锁期间发生过 panic，本轮不能凭空造一段对话继续（见
    /// `Session::history_snapshot`）。
    HistoryPoisoned,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "store io error: {error}"),
            Self::Corrupt { path, message } => {
                write!(f, "store corrupt at {}: {message}", path.display())
            }
            Self::HistoryPoisoned => write!(f, "conversation history lock poisoned"),
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug, Clone)]
pub struct TranscriptStore {
    root: PathBuf,
    /// Tier-3 dual-write mirror. `None` = transcript is the only copy (the default
    /// until the parity gate in `specs/large-session-scaling.md` is signed off).
    read_model: Option<Arc<std::sync::Mutex<crate::read_model::ConversationRows>>>,
}

/// transcript 行：行投影或命令终态 ack。
/// 追加日志语义：Row = 新行；Upsert = 同行替换（重放时按 rowId 覆盖）。
///
/// `History` 是会话模型历史的**唯一 durable 载体**（specs/rust-native-crates.md
/// S3 session memory 规则 2）：turn 追加的每条消息按落盘顺序追加一条记录，回放
/// 时按原序还原。之所以不存「整份历史快照」而是逐条追加，是因为 transcript 是
/// 追加日志——快照需要重写文件，破坏「写路径只有 append」这条不变量，且并发
/// turn 追加时会互相覆盖。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum TranscriptEntry {
    Row {
        row: Value,
    },
    Upsert {
        row: Value,
    },
    Ack {
        command_id: String,
        ack: Value,
    },
    History {
        message: Message,
    },
    /// 压缩后**替换**整份历史（microcompact / LLM 摘要）。作为事件追加而不是
    /// 重写文件，是为了让 transcript 保持「写路径只有 append」这条不变量；回放
    /// 按文件顺序应用，最后一次 compaction 生效（早期 History 条目被这次替换
    /// 覆盖的语义由「置为该列表」表达，不需要回溯删除）。
    HistoryCompacted {
        messages: Vec<Message>,
    },
    FileChange {
        record: crate::file_ledger::FileChangeRecord,
    },
    /// 分支截断（edit/retry）：**丢弃 `from_row_id` 及其之后的所有行**，与
    /// `conversationDeltaSchema` 的 `row.removed{fromRowId}` 语义逐字对齐
    /// （"删除该行及之后所有"）。同样以事件追加而非重写文件，理由与
    /// `HistoryCompacted` 一致：append-only 是本 store 的写路径不变量。
    RowsTruncated {
        from_row_id: u64,
    },
}

/// 手动按 `t` 标签分派，而不是让 serde 直接反序列化整个 enum。
///
/// WHY（review MAJOR-2 的向前兼容半）：serde 的内部标签对**未知标签**是硬错误
/// （unknown variant），所以一个只认识 4 种 entry 的旧二进制，读到未来版本写的
/// 第 5 种 `t` 就会整份 transcript 报废——在 ENTRY 027 之后，那等于静默清空模型
/// 记忆。这里先自己取出标签字符串，未知标签走 `None` 分支跳过并计数，已知标签才
/// 交给 serde 解析 payload。既拿到容错，又保留了 serde 对 payload 形状的校验。
/// 区分「行不是 JSON」「行是 JSON 但没有 `t`」与「标签未知」三种情况。
///
/// 错误文本用**自己的类型**而不是 `serde_json::Error::io(...)`（review round2
/// MINOR-B）：后者 Display 只打印内层 io 消息，坏行会报成 `not json` / `missing t`，
/// 看起来像磁盘故障，还丢掉了 serde 本来会给出的 `expected value at line X
/// column Y` 定位信息。未知标签是**向前兼容**、不是损坏，所以单独一个变体，报告
/// 时能一眼区分。
#[derive(Debug)]
enum EntryParseError {
    /// 真正的 JSON 解析失败；保留 serde 的原始错误（含行列定位）。
    Json(serde_json::Error),
    /// 合法 JSON 但缺少 `t` 标签：schema 违规，属损坏。
    MissingTag,
}

impl std::fmt::Display for EntryParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(error) => write!(f, "malformed json: {error}"),
            Self::MissingTag => write!(f, "valid json without a `t` entry tag"),
        }
    }
}

fn parse_entry(line: &[u8]) -> Result<Option<TranscriptEntry>, EntryParseError> {
    let value: Value = serde_json::from_slice(line).map_err(EntryParseError::Json)?;
    let Some(tag) = value.get("t").and_then(Value::as_str) else {
        return Err(EntryParseError::MissingTag);
    };
    if !matches!(
        tag,
        "row" | "upsert" | "ack" | "history" | "historyCompacted" | "rowsTruncated" | "fileChange"
    ) {
        return Ok(None);
    }
    // 已知标签但 payload 形状不对：同样按损坏处理，并保留 serde 的字段级错误。
    serde_json::from_value(value)
        .map(Some)
        .map_err(EntryParseError::Json)
}

impl TranscriptStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            read_model: None,
        }
    }

    /// Transcript + a live Tier-3 read-model mirror. Every row write is mirrored so
    /// `verify_against` can prove the two copies are identical before the read path
    /// is switched over.
    pub fn with_read_model(
        root: impl Into<PathBuf>,
        read_model: Arc<std::sync::Mutex<crate::read_model::ConversationRows>>,
    ) -> Self {
        Self {
            root: root.into(),
            read_model: Some(read_model),
        }
    }

    pub fn session_dir(&self, session_id: &str) -> PathBuf {
        self.root.join("sessions").join(session_id)
    }

    fn meta_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("meta.json")
    }

    fn transcript_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("transcript.jsonl")
    }

    pub fn create_session(&self, meta: &SessionMeta) -> Result<(), StoreError> {
        fs::create_dir_all(self.session_dir(&meta.session_id))?;
        self.write_meta(meta)?;
        // 空 transcript 也要落盘，resume 与冷启动路径才对称。
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.transcript_path(&meta.session_id))?;
        Ok(())
    }

    pub fn write_meta(&self, meta: &SessionMeta) -> Result<(), StoreError> {
        let path = self.meta_path(&meta.session_id);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(meta).map_err(|error| StoreError::Corrupt {
            path: path.clone(),
            message: error.to_string(),
        })?;
        fs::write(&tmp, json)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn load_meta(&self, session_id: &str) -> Result<SessionMeta, StoreError> {
        let path = self.meta_path(session_id);
        let raw = fs::read(&path)?;
        serde_json::from_slice(&raw).map_err(|error| StoreError::Corrupt {
            path,
            message: error.to_string(),
        })
    }

    /// Snapshot-cached transcript read: when the durable snapshot still matches the
    /// transcript byte length and row count, return it without replaying the JSONL;
    /// otherwise replay and refresh the snapshot. Without a read model this is
    /// exactly [`Self::load_transcript`].
    pub fn load_transcript_cached(&self, session_id: &str) -> Result<Transcript, StoreError> {
        let path = self.transcript_path(session_id);
        let transcript_len = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        if let Some(model) = &self.read_model {
            if let Ok(model) = model.lock() {
                if let Some(mut transcript) = model.read_snapshot(session_id, transcript_len) {
                    // The snapshot intentionally omits rows; the row table is the
                    // second half of the same read model.
                    transcript.rows =
                        model
                            .all_rows(session_id)
                            .map_err(|error| StoreError::Corrupt {
                                path: path.clone(),
                                message: error.to_string(),
                            })?;
                    return Ok(transcript);
                }
            }
        }
        let transcript = self.load_transcript(session_id)?;
        if let Some(model) = &self.read_model {
            if let Ok(model) = model.lock() {
                if let Err(error) =
                    model.write_snapshot(session_id, &transcript, transcript_len, now_ms())
                {
                    eprintln!(
                        "[zcode-host] read-model snapshot write failed for {session_id}: {error}"
                    );
                }
            }
        }
        Ok(transcript)
    }

    pub fn append_row(&self, session_id: &str, row: &Value) -> Result<(), StoreError> {
        self.append_entry(session_id, &TranscriptEntry::Row { row: row.clone() })?;
        self.mirror_row(session_id, row);
        Ok(())
    }

    pub fn append_upsert(&self, session_id: &str, row: &Value) -> Result<(), StoreError> {
        self.append_entry(session_id, &TranscriptEntry::Upsert { row: row.clone() })?;
        self.mirror_row(session_id, row);
        Ok(())
    }

    /// Dual-write mirror: a mirror failure must be visible, but it must not fail the
    /// authoritative transcript write (the transcript is still the source of truth
    /// until the read path flips).
    fn mirror_row(&self, session_id: &str, row: &Value) {
        let Some(model) = &self.read_model else {
            return;
        };
        match model.lock() {
            Ok(model) => {
                if let Err(error) = model.upsert(session_id, row, now_ms()) {
                    eprintln!("[zcode-host] read-model mirror failed for {session_id}: {error}");
                }
            }
            Err(_) => eprintln!("[zcode-host] read-model mirror lock poisoned"),
        }
    }

    pub fn append_ack(
        &self,
        session_id: &str,
        command_id: &str,
        ack: &Value,
    ) -> Result<(), StoreError> {
        self.append_entry(
            session_id,
            &TranscriptEntry::Ack {
                command_id: command_id.to_string(),
                ack: ack.clone(),
            },
        )
    }

    /// 追加一条模型历史消息（会话记忆的 durable 写路径）。
    pub fn append_history(&self, session_id: &str, message: &Message) -> Result<(), StoreError> {
        self.append_entry(
            session_id,
            &TranscriptEntry::History {
                message: message.clone(),
            },
        )
    }

    /// 记录一次压缩：整份历史被替换为压缩后的列表。
    /// 追加一条文件变更（写工具成功落盘之后调用）。
    pub fn append_file_change(
        &self,
        session_id: &str,
        record: &crate::file_ledger::FileChangeRecord,
    ) -> Result<(), StoreError> {
        self.append_entry(
            session_id,
            &TranscriptEntry::FileChange {
                record: record.clone(),
            },
        )
    }

    /// 分支截断：把 `from_row_id` 及其之后的行从行集里去掉（edit/retry 的 durable 记录）。
    pub fn append_rows_truncated(
        &self,
        session_id: &str,
        from_row_id: u64,
    ) -> Result<(), StoreError> {
        self.append_entry(session_id, &TranscriptEntry::RowsTruncated { from_row_id })?;
        if let Some(model) = &self.read_model {
            if let Ok(model) = model.lock() {
                if let Err(error) = model.truncate_from(session_id, from_row_id) {
                    eprintln!("[zcode-host] read-model truncate failed for {session_id}: {error}");
                }
            }
        }
        Ok(())
    }

    pub fn append_history_compaction(
        &self,
        session_id: &str,
        messages: &[Message],
    ) -> Result<(), StoreError> {
        self.append_entry(
            session_id,
            &TranscriptEntry::HistoryCompacted {
                messages: messages.to_vec(),
            },
        )
    }

    fn append_entry(&self, session_id: &str, entry: &TranscriptEntry) -> Result<(), StoreError> {
        let path = self.transcript_path(session_id);
        let mut line = serde_json::to_vec(entry).map_err(|error| StoreError::Corrupt {
            path: path.clone(),
            message: error.to_string(),
        })?;
        line.push(b'\n');
        let mut file = fs::OpenOptions::new().append(true).open(&path)?;
        file.write_all(&line)?;
        Ok(())
    }

    /// 冷恢复：按文件顺序回放行、命令终态与会话历史。
    ///
    /// **文件级失败（缺失/不可读）也走容错**：返回一份 `skipped_lines` 里带原因的
    /// 空 transcript，而不是 `Err`（review round2 MAJOR-2 的第二扇门）。原先这里
    /// `?` 直接冒泡，调用方 `unwrap_or_default()` 拿到的是 `skipped_lines` 为空的
    /// 默认值——于是「文件不在了」这条最常见的数据丢失场景**完全没有诊断**，
    /// 会话带着空记忆静默恢复。现在它和坏行走同一条可见通路。
    pub fn load_transcript(&self, session_id: &str) -> Result<Transcript, StoreError> {
        let path = self.transcript_path(session_id);
        let raw = match fs::read(&path) {
            Ok(raw) => raw,
            Err(error) => {
                // 文件级失败（缺失/不可读）走容错通路：返回带原因的空 transcript，
                // 让调用方 `resume_sessions` 报出降级诊断，而不是静默失忆。
                let mut transcript = Transcript::default();
                transcript.skipped_lines.push(format!(
                    "transcript file unreadable ({}): {error}",
                    path.display()
                ));
                return Ok(transcript);
            }
        };
        let mut transcript = Transcript::default();
        for (index, raw_line) in raw.split(|byte| *byte == b'\n').enumerate() {
            // 容忍 CRLF：Windows 编辑器写出的 transcript 行末可能带一个 `\r`。
            // 该 `\r` 是行终止符而非 JSON 内容（JSON 字符串里控制字符必须转义），
            // 所以只剥掉末尾单个 `\r`，其余原样保留。
            let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
            if line.is_empty() {
                continue;
            }
            // 容错回放（review MAJOR-2）：**跳过坏行并计数**，不整体失败。
            let entry: TranscriptEntry = match parse_entry(line) {
                Ok(Some(entry)) => entry,
                Ok(None) => {
                    transcript.skipped_lines.push(format!(
                        "line {}: unknown transcript entry tag (forward-compat skip)",
                        index + 1
                    ));
                    continue;
                }
                Err(error) => {
                    transcript
                        .skipped_lines
                        .push(format!("line {}: {error}", index + 1));
                    continue;
                }
            };
            match entry {
                TranscriptEntry::Row { row } => {
                    // 顺带记录「轮 → 起始历史条数」：turn 落 turnHeader 时本轮的
                    // 用户输入还没进历史，所以此刻的 history.len() 正是该轮的历史起点。
                    // 分支截断（editUserQuery / retryTurn）靠它把历史切回轮起点，
                    // 不必额外持久化一个锚点字段。
                    if row.get("kind").and_then(Value::as_str) == Some("turnHeader") {
                        if let Some(turn_id) = row.get("turnId").and_then(Value::as_str) {
                            transcript
                                .turn_history_starts
                                .push((turn_id.to_string(), transcript.history.len()));
                        }
                    }
                    transcript.rows.push(row);
                }
                TranscriptEntry::Upsert { row } => {
                    let row_id = row.get("rowId").and_then(Value::as_u64);
                    if let Some(slot) = transcript.rows.iter_mut().find(|candidate| {
                        row_id.is_some() && candidate.get("rowId").and_then(Value::as_u64) == row_id
                    }) {
                        *slot = row;
                    } else {
                        transcript.rows.push(row);
                    }
                }
                TranscriptEntry::Ack { command_id, ack } => {
                    transcript.acks.insert(command_id, ack);
                }
                TranscriptEntry::History { message } => transcript.history.push(message),
                TranscriptEntry::HistoryCompacted { messages } => {
                    transcript.history = messages;
                }
                TranscriptEntry::FileChange { record } => {
                    transcript.file_changes.push(record);
                }
                TranscriptEntry::RowsTruncated { from_row_id } => {
                    // 与 `row.removed{fromRowId}` 同一语义：该行及其后全部消失。
                    let removed_turns: Vec<String> = transcript
                        .rows
                        .iter()
                        .filter(|row| {
                            row.get("rowId")
                                .and_then(Value::as_u64)
                                .is_some_and(|id| id >= from_row_id)
                                && row.get("kind").and_then(Value::as_str) == Some("turnHeader")
                        })
                        .filter_map(|row| row.get("turnId").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect();
                    transcript.rows.retain(|row| {
                        row.get("rowId")
                            .and_then(Value::as_u64)
                            .is_some_and(|id| id < from_row_id)
                    });
                    // 轮锚点必须跟着消失：留下已截断轮的锚点，下一次 edit/retry 会拿
                    // 到一个不存在的轮起点，历史会被切到错误的位置。
                    transcript.turn_history_starts.retain(|(turn_id, _)| {
                        !removed_turns.iter().any(|removed| removed == turn_id)
                    });
                    // 文件台账跟着裁：留下被截断轮次的记录，重启后 rewind 的门禁
                    // 会认为那一轮还能回滚，然后去恢复一个对话里已经不存在的轮次。
                    transcript.file_changes.retain(|change| {
                        !removed_turns
                            .iter()
                            .any(|removed| removed == &change.turn_id)
                    });
                }
            }
        }
        Ok(transcript)
    }

    pub fn list_sessions(&self) -> Vec<String> {
        let mut ids = Vec::new();
        if let Ok(entries) = fs::read_dir(self.root.join("sessions")) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    if let Some(name) = entry.file_name().to_str() {
                        ids.push(name.to_string());
                    }
                }
            }
        }
        ids.sort();
        ids
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Transcript {
    pub rows: Vec<Value>,
    /// 回放时按落轮顺序记录的「turnId → 该轮起始时的历史条数」。
    /// 由 `Row` 条目里的 turnHeader 派生，不是独立的持久化字段。
    pub turn_history_starts: Vec<(String, usize)>,
    /// 回放出的文件变更台账（rewind 的事实来源）。
    pub file_changes: Vec<crate::file_ledger::FileChangeRecord>,
    pub acks: std::collections::HashMap<String, Value>,
    pub history: Vec<Message>,
    pub skipped_lines: Vec<String>,
}

/// 会话 id 生成：进程内计数器 + 纳秒时间戳 + pid，避免为 id 引入 uuid 依赖。
/// 只要求进程内唯一与可读；持久身份由 meta.json 承担。
pub fn generate_id(prefix: &str, counter: &std::sync::atomic::AtomicU64) -> String {
    let seq = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{}-{nanos}-{seq}", std::process::id())
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// 路径卫生：session id 只允许安全字符，防目录穿越（外部输入边界）。
pub fn is_safe_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 128
        && session_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}
