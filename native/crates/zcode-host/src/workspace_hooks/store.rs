//! 信任台账落盘（`workspace-hook-trust-store-file.ts` +
//! `adapters/storage/workspace-hook-trust-store.ts` 的 Rust 版）。
//!
//! 三个硬要求：
//! 1. **原子替换**：先写同目录临时文件再 rename，进程崩溃不会留下半个 JSON。
//! 2. **损坏 = fail-closed 且状态粘滞**：文件语法/结构坏掉后，本进程在成功写入前
//!    一直判 corrupt（任何记录都不可信）；原文件被移到 `<name>.corrupt-<ms>` 存证
//!    （TS 同）。不能"移开之后下一次读变成 missing"，那会把"损坏"洗成"首次使用"。
//! 3. **不丢别人的记录**：store 是跨 workspace 的共享文件，写入必须是
//!    load-merge-save（按 `workspaceIdentity\u{0}digest` 合并），不能整文件覆盖。
//!
//! 与 TS 的差异（记录在 crate README）：TS 有跨进程文件锁 + 陈旧锁回收（pid 启动时间
//! 探测）。本 host 是数据目录的唯一写者，进程内互斥足够；跨进程并发写同一数据目录
//! 不在支持范围内。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::trust::{trust_key, TrustRecord, TrustStoreFile, TrustStoreStatus};

pub const TRUST_STORE_FILE: &str = "workspace-hook-trust-v1.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustStoreLoad {
    Missing,
    Ok(Vec<TrustRecord>),
    /// 文件存在但不可信；`recovered_path` 是移走的存证文件（移动失败时为 None）。
    Corrupt {
        recovered_path: Option<PathBuf>,
    },
}

#[derive(Debug)]
pub struct TrustStoreError {
    pub message: String,
}

impl TrustStoreError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Default)]
struct Inner {
    loaded: bool,
    status: Option<TrustStoreStatus>,
    records: Vec<TrustRecord>,
    /// 损坏判定一旦成立就粘滞到下一次成功写入：读取失败/被移走都不改变结论。
    corrupt_sticky: bool,
}

/// 一次进程内的台账访问器。`path` 是 `<data-dir>/security/workspace-hook-trust-v1.json`。
pub struct TrustStore {
    path: PathBuf,
    /// 进程内串行化：读-改-写必须在同一把锁里，否则两个会话并发授权会互相丢记录。
    mutation_lock: Mutex<()>,
    inner: Mutex<Inner>,
}

impl TrustStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            mutation_lock: Mutex::new(()),
            inner: Mutex::new(Inner::default()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 首次读盘（之后走缓存）；返回本次读到的三态。损坏文件在这里被移开存证。
    pub fn load(&self) -> TrustStoreLoad {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.load_locked(&mut inner)
    }

    /// 强制重新读盘（配置热更新/外部 CLI 改过文件时用）。
    pub fn reload(&self) -> TrustStoreLoad {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.loaded = false;
        self.load_locked(&mut inner)
    }

    fn load_locked(&self, inner: &mut Inner) -> TrustStoreLoad {
        if inner.loaded {
            return match inner.status {
                Some(TrustStoreStatus::Corrupt) => TrustStoreLoad::Corrupt {
                    recovered_path: None,
                },
                _ if inner.records.is_empty() => TrustStoreLoad::Missing,
                _ => TrustStoreLoad::Ok(inner.records.clone()),
            };
        }
        let result = match fs::read_to_string(&self.path) {
            Ok(content) => match TrustStoreFile::parse(&content) {
                Ok(file) => {
                    inner.status = Some(TrustStoreStatus::Ok);
                    inner.records = file.records;
                    TrustStoreLoad::Ok(inner.records.clone())
                }
                Err(error) => {
                    eprintln!(
                        "[zcode-host] workspace hook trust store is corrupt ({}): {error}",
                        self.path.display()
                    );
                    inner.corrupt_sticky = true;
                    TrustStoreLoad::Corrupt {
                        recovered_path: self.recover_corrupt_file(),
                    }
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if inner.corrupt_sticky {
                    TrustStoreLoad::Corrupt {
                        recovered_path: None,
                    }
                } else {
                    inner.records = Vec::new();
                    TrustStoreLoad::Missing
                }
            }
            Err(error) => {
                eprintln!(
                    "[zcode-host] workspace hook trust store unreadable ({}): {error}",
                    self.path.display()
                );
                // 读不出来 ≠ 没有记录：按损坏处理（fail-closed）。
                inner.corrupt_sticky = true;
                TrustStoreLoad::Corrupt {
                    recovered_path: None,
                }
            }
        };
        if matches!(result, TrustStoreLoad::Corrupt { .. }) {
            inner.status = Some(TrustStoreStatus::Corrupt);
            inner.records = Vec::new();
        }
        inner.loaded = true;
        result
    }

    /// 按 key 合并写入（后者覆盖前者），返回合并后的全量记录。
    pub fn upsert(&self, records: Vec<TrustRecord>) -> Result<Vec<TrustRecord>, TrustStoreError> {
        let _guard = self
            .mutation_lock
            .lock()
            .map_err(|_| TrustStoreError::new("trust store lock poisoned"))?;
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !inner.loaded {
            let _ = self.load_locked(&mut inner);
        }
        let mut existing = inner.records.clone();
        for record in records {
            record.validate().map_err(TrustStoreError::new)?;
            let key = trust_key(&record.workspace_identity, &record.hook_declaration_digest);
            existing.retain(|current| {
                trust_key(
                    &current.workspace_identity,
                    &current.hook_declaration_digest,
                ) != key
            });
            existing.push(record);
        }
        self.write(&existing)?;
        inner.records = existing.clone();
        inner.status = Some(TrustStoreStatus::Ok);
        inner.corrupt_sticky = false;
        Ok(existing)
    }

    /// 撤销：删除指定摘要的记录（撤销本身只存在于运行时，持久效果 = 不再有授权）。
    pub fn remove(
        &self,
        workspace_identity: &str,
        hook_declaration_digests: &[String],
    ) -> Result<Vec<TrustRecord>, TrustStoreError> {
        let _guard = self
            .mutation_lock
            .lock()
            .map_err(|_| TrustStoreError::new("trust store lock poisoned"))?;
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !inner.loaded {
            let _ = self.load_locked(&mut inner);
        }
        let mut existing = inner.records.clone();
        existing.retain(|record| {
            !(record.workspace_identity == workspace_identity
                && hook_declaration_digests.contains(&record.hook_declaration_digest))
        });
        self.write(&existing)?;
        inner.records = existing.clone();
        inner.status = Some(TrustStoreStatus::Ok);
        inner.corrupt_sticky = false;
        Ok(existing)
    }

    /// 读取 + 状态判定（评估入口用这一份，避免 callsite 自己解释三态）。
    pub fn records(&self) -> (TrustStoreStatus, Vec<TrustRecord>) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !inner.loaded {
            let _ = self.load_locked(&mut inner);
        }
        (
            inner.status.unwrap_or(TrustStoreStatus::Ok),
            inner.records.clone(),
        )
    }

    fn write(&self, records: &[TrustRecord]) -> Result<(), TrustStoreError> {
        let file = TrustStoreFile {
            schema_version: super::trust::WORKSPACE_HOOK_TRUST_STORE_SCHEMA_VERSION,
            records: records.to_vec(),
        };
        write_atomic(&self.path, file.to_json_string().as_bytes())
    }

    fn recover_corrupt_file(&self) -> Option<PathBuf> {
        let stamp = crate::store::now_ms();
        let recovered = self.path.with_file_name(format!(
            "{}.corrupt-{stamp}",
            self.path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| TRUST_STORE_FILE.to_string())
        ));
        match fs::rename(&self.path, &recovered) {
            Ok(()) => {
                eprintln!(
                    "[zcode-host] corrupt workspace hook trust store moved aside to {}",
                    recovered.display()
                );
                Some(recovered)
            }
            Err(error) => {
                eprintln!("[zcode-host] could not move corrupt trust store aside: {error}");
                None
            }
        }
    }
}

/// 原子写：同目录临时文件 → fsync → rename。失败时清理临时文件。
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), TrustStoreError> {
    let directory = path
        .parent()
        .ok_or_else(|| TrustStoreError::new("trust store path has no parent directory"))?;
    fs::create_dir_all(directory)
        .map_err(|error| TrustStoreError::new(format!("create security dir: {error}")))?;
    let temp = path.with_extension("json.tmp");
    {
        let mut file = fs::File::create(&temp)
            .map_err(|error| TrustStoreError::new(format!("create temp store: {error}")))?;
        file.write_all(bytes)
            .map_err(|error| TrustStoreError::new(format!("write temp store: {error}")))?;
        file.sync_all()
            .map_err(|error| TrustStoreError::new(format!("fsync temp store: {error}")))?;
    }
    fs::rename(&temp, path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        TrustStoreError::new(format!("rename trust store into place: {error}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_hooks::config::HookEventName;

    fn base_dir(tag: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("zcode-hook-store-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn record(workspace: &str, digest_char: char, at: &str) -> TrustRecord {
        TrustRecord {
            workspace_identity: workspace.into(),
            hook_declaration_digest: digest_char.to_string().repeat(64),
            digest_algorithm: "sha256".into(),
            decision: "trusted".into(),
            granted_at: at.into(),
            last_used_at: None,
            bundle_digest_at_grant: Some("f".repeat(64)),
            event_at_grant: HookEventName::PreToolUse,
            display_command_at_grant: "node check.js".into(),
            source_path_at_grant: ".zcode/config.json".into(),
            source_discovery_order_at_grant: Some(0),
            matcher_at_grant: Some(None),
            matcher_index_at_grant: Some(0),
            hook_index_at_grant: Some(0),
            app_version_at_grant: None,
        }
    }

    #[test]
    fn missing_store_is_an_empty_ok_store_not_an_error() {
        let store = TrustStore::new(base_dir("missing").join(TRUST_STORE_FILE));
        assert_eq!(store.load(), TrustStoreLoad::Missing);
        assert_eq!(store.records(), (TrustStoreStatus::Ok, Vec::new()));
    }

    #[test]
    fn upsert_merges_by_key_and_keeps_other_workspaces() {
        let base = base_dir("merge");
        let store = TrustStore::new(base.join(TRUST_STORE_FILE));
        store
            .upsert(vec![record("ws-a", 'a', "2026-01-01T00:00:00.000Z")])
            .expect("first write");
        store
            .upsert(vec![
                record("ws-b", 'b', "2026-01-01T00:00:01.000Z"),
                // 同 key 再授权 = 覆盖（新的 grantedAt）。
                record("ws-a", 'a', "2026-01-02T00:00:00.000Z"),
            ])
            .expect("second write");
        let (status, records) = store.records();
        assert_eq!(status, TrustStoreStatus::Ok);
        assert_eq!(records.len(), 2, "同 key 覆盖而不是堆叠");
        let ws_a = records
            .iter()
            .find(|record| record.workspace_identity == "ws-a")
            .expect("ws-a");
        assert_eq!(ws_a.granted_at, "2026-01-02T00:00:00.000Z");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn remove_deletes_only_the_requested_keys() {
        let base = base_dir("remove");
        let store = TrustStore::new(base.join(TRUST_STORE_FILE));
        store
            .upsert(vec![
                record("ws-a", 'a', "2026-01-01T00:00:00.000Z"),
                record("ws-a", 'b', "2026-01-01T00:00:00.000Z"),
                record("ws-b", 'c', "2026-01-01T00:00:00.000Z"),
            ])
            .unwrap();
        store.remove("ws-a", &["a".repeat(64)]).expect("revoke one");
        let (_, records) = store.records();
        let keys: Vec<(String, String)> = records
            .into_iter()
            .map(|record| (record.workspace_identity, record.hook_declaration_digest))
            .collect();
        assert_eq!(
            keys,
            vec![
                ("ws-a".to_string(), "b".repeat(64)),
                ("ws-b".to_string(), "c".repeat(64)),
            ]
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_corrupt_store_stays_corrupt_until_a_successful_write() {
        let base = base_dir("corrupt");
        let path = base.join(TRUST_STORE_FILE);
        fs::write(&path, "{ this is not json").unwrap();
        let store = TrustStore::new(path.clone());
        let load = store.load();
        let TrustStoreLoad::Corrupt { recovered_path } = load else {
            panic!("must be corrupt");
        };
        let recovered = recovered_path.expect("file moved aside");
        assert!(recovered.exists(), "存证文件必须保留");
        assert!(recovered
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("workspace-hook-trust-v1.json.corrupt-"));
        assert!(!path.exists(), "损坏文件已被移走");
        assert_eq!(
            store.records(),
            (TrustStoreStatus::Corrupt, Vec::new()),
            "损坏后任何记录都不可信"
        );
        // 再读一次也不能把 corrupt 洗成 missing。
        assert!(matches!(store.reload(), TrustStoreLoad::Corrupt { .. }));
        assert_eq!(store.records().0, TrustStoreStatus::Corrupt);

        // 成功写入后恢复为 Ok（存证文件仍在，用户可人工恢复）。
        store
            .upsert(vec![record("ws-a", 'd', "2026-01-01T00:00:00.000Z")])
            .expect("write after corruption");
        assert_eq!(store.records().0, TrustStoreStatus::Ok);
        assert_eq!(store.records().1.len(), 1);
        assert!(recovered.exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn writes_are_atomic_and_leave_no_temp_file() {
        let base = base_dir("atomic");
        let path = base.join(TRUST_STORE_FILE);
        let store = TrustStore::new(path.clone());
        store
            .upsert(vec![record("ws-a", 'e', "2026-01-01T00:00:00.000Z")])
            .unwrap();
        assert!(path.exists());
        assert!(
            !path.with_extension("json.tmp").exists(),
            "临时文件必须已经被 rename 走"
        );
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"schemaVersion\": 1"));
        assert!(content.contains("ws-a"));
        let _ = fs::remove_dir_all(&base);
    }
}
