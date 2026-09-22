//! 会话持久化：JSONL transcript。
//!
//! 本 increment 的存储后端（specs/rust-native-crates.md S3 验收明示 OPEN）：TS 侧
//! 的 sqlite 单写者存储尚未移植，host 用追加式 JSONL 落盘行与命令终态，保证
//! 「重启 → resume 行不丢、幂等重提可回放」这两个验收点可测。布局：
//!
//! ```text
//! <data-dir>/sessions/<sessionId>/meta.json          # 会话元信息（title/epoch/revision）
//! <data-dir>/sessions/<sessionId>/transcript.jsonl   # {"t":"row",...} / {"t":"ack",...}
//! ```
//!
//! 写路径只有 append（行与 ack 都是追加事实），meta.json 是唯一可重写文件，
//! 用「写临时文件 + rename」保证原子性。

use std::fs;
use std::io::Write;
use std::path::PathBuf;

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
}

#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Corrupt { path: PathBuf, message: String },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "store io error: {error}"),
            Self::Corrupt { path, message } => {
                write!(f, "store corrupt at {}: {message}", path.display())
            }
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
}

/// transcript 行：行投影或命令终态 ack。
/// 追加日志语义：Row = 新行；Upsert = 同行替换（重放时按 rowId 覆盖）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum TranscriptEntry {
    Row { row: Value },
    Upsert { row: Value },
    Ack { command_id: String, ack: Value },
}

impl TranscriptStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
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

    pub fn append_row(&self, session_id: &str, row: &Value) -> Result<(), StoreError> {
        self.append_entry(session_id, &TranscriptEntry::Row { row: row.clone() })
    }

    pub fn append_upsert(&self, session_id: &str, row: &Value) -> Result<(), StoreError> {
        self.append_entry(session_id, &TranscriptEntry::Upsert { row: row.clone() })
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

    /// 冷恢复：按文件顺序回放行与命令终态。
    pub fn load_transcript(&self, session_id: &str) -> Result<Transcript, StoreError> {
        let path = self.transcript_path(session_id);
        let raw = fs::read(&path)?;
        let mut transcript = Transcript::default();
        for (index, line) in raw.split(|byte| *byte == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let entry: TranscriptEntry =
                serde_json::from_slice(line).map_err(|error| StoreError::Corrupt {
                    path: path.clone(),
                    message: format!("line {}: {error}", index + 1),
                })?;
            match entry {
                TranscriptEntry::Row { row } => transcript.rows.push(row),
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

    pub fn delete_session(&self, session_id: &str) -> Result<(), StoreError> {
        let dir = self.session_dir(session_id);
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct Transcript {
    pub rows: Vec<Value>,
    pub acks: std::collections::HashMap<String, Value>,
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
