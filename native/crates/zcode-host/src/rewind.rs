//! 工作区文件回滚（`applyFileRewind`）的计划与执行。
//!
//! 语义逐条对齐 `apps/zcode-cli/packages/core/src/runtime/methods/file-rewind.ts`：
//! - **preview 先分桶**：`safeFiles` / `unsafeFiles` / `ignoredFiles`，
//!   `canApply = safeFiles.length > 0 && unsafeFiles.length === 0`；
//! - **按路径归并**（同一文件在一轮里被写多次只算一个回滚目标：恢复"本轮第一次改动前"
//!   的内容，检查"本轮最后一次改动后"的内容是否仍是我们写出的那一版）；
//! - **逆序应用**（后改的先恢复），任一步失败就把已改动的文件按 journal 反向补偿回去；
//! - shell（Bash）来源的变更**不记账**，因此只可能以 `external_modified` 出现，
//!   永远不会被 host 猜着回滚。
//!
//! 文件系统通过 `FsAdapter` 注入：产品路径用真实磁盘，测试用内存替身——被测的分桶
//! 与补偿逻辑因此可以不碰盘就被逐条钉住。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::content_hash::content_hash;
use crate::file_ledger::FileChangeRecord;

/// `WorkspaceFileRewindUnsafeReason` 闭集里**本 host 有生产者**的那几个
/// （TS types.ts:566 还有 `checkpoint_missing` / `checkpoint_unreadable`：两者来自
/// artifact store 的读取与解析失败，而本 host 的台账就在 transcript 里、随会话一起
/// 加载，不存在"有 checkpoint 但读不出"的第三种状态。没有生产者的枚举值不写进来，
/// 免得变成永远命中不到的死分支）。
pub const UNSAFE_EXTERNAL_MODIFIED: &str = "external_modified";
pub const UNSAFE_FILE_READ_FAILED: &str = "file_read_failed";
pub const UNSAFE_UNSUPPORTED_CHECKPOINT: &str = "unsupported_checkpoint";

/// `WorkspaceFileRewindIgnoredFile.reason` 的闭集。
pub const IGNORED_BASH: &str = "bash_ignored";

/// 单个文件要做的动作：恢复内容，或删除（本轮之前它不存在）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewindAction {
    Restore,
    Delete,
}

impl RewindAction {
    pub fn as_str(self) -> &'static str {
        match self {
            RewindAction::Restore => "restore",
            RewindAction::Delete => "delete",
        }
    }
}

/// 一个回滚操作（按路径归并后的最小单元）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindOperation {
    pub path: String,
    /// 本轮第一次改动前的内容；`None` = 当时不存在 → 动作是删除。
    pub restore_content: Option<String>,
    /// 本轮最后一次改动后我们写出的内容（当前内容必须与它一致才安全）。
    pub expected_after: Option<String>,
    pub action: RewindAction,
    pub operation_count: usize,
    pub tool_names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeFile {
    pub action: RewindAction,
    pub operation_count: usize,
    pub path: String,
    pub tool_names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsafeFile {
    pub operation_count: usize,
    pub path: String,
    pub reason: &'static str,
    pub tool_names: Vec<String>,
    pub message: Option<String>,
    pub expected_hash: Option<String>,
    pub current_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredFile {
    pub operation_count: usize,
    pub path: String,
    pub reason: &'static str,
    pub tool_names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindPreview {
    pub can_apply: bool,
    pub ignored_files: Vec<IgnoredFile>,
    pub safe_files: Vec<SafeFile>,
    pub unsafe_files: Vec<UnsafeFile>,
}

impl RewindPreview {
    /// 客户端可见形状（`WorkspaceFileRewindPreview` 的 camelCase 投影）。
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::json;
        json!({
            "canApply": self.can_apply,
            "safeFiles": self.safe_files.iter().map(|file| json!({
                "action": file.action.as_str(),
                "operationCount": file.operation_count,
                "path": file.path,
                "toolNames": file.tool_names,
            })).collect::<Vec<_>>(),
            "unsafeFiles": self.unsafe_files.iter().map(|file| {
                let mut value = json!({
                    "operationCount": file.operation_count,
                    "path": file.path,
                    "reason": file.reason,
                    "toolNames": file.tool_names,
                });
                if let Some(object) = value.as_object_mut() {
                    if let Some(message) = &file.message {
                        object.insert("message".to_string(), json!(message));
                    }
                    if let Some(hash) = &file.expected_hash {
                        object.insert("expectedHash".to_string(), json!(hash));
                    }
                    if let Some(hash) = &file.current_hash {
                        object.insert("currentHash".to_string(), json!(hash));
                    }
                }
                value
            }).collect::<Vec<_>>(),
            "ignoredFiles": self.ignored_files.iter().map(|file| json!({
                "operationCount": file.operation_count,
                "path": file.path,
                "reason": file.reason,
                "toolNames": file.tool_names,
            })).collect::<Vec<_>>(),
        })
    }
}

/// 预览要读的"当前内容"来源。
///
/// WHY 泛型而不是 `&dyn Fn(..)`：`&dyn Fn` 的省略生命周期是 `'static`，那会强迫调用方
/// 把文件系统句柄塞进 `'static 闭包里（`FsAdapter` 是局部值，塞不进去）。泛型参数没有
/// 这个约束，预览因此能直接借用调用栈上的适配器，而不是逼产品代码 Arc::clone 一份。
pub trait ReadCurrent {
    fn read(&self, relative: &str) -> Result<Option<String>, String>;
}

impl<F> ReadCurrent for F
where
    F: Fn(&str) -> Result<Option<String>, String> + Send + Sync,
{
    fn read(&self, relative: &str) -> Result<Option<String>, String> {
        self(relative)
    }
}

/// 把台账记录按路径归并成回滚操作，保持**首次出现顺序**（应用时再逆序）。
pub fn operations(records: &[FileChangeRecord]) -> Vec<RewindOperation> {
    let mut order: Vec<String> = Vec::new();
    let mut by_path: std::collections::HashMap<String, Vec<FileChangeRecord>> =
        std::collections::HashMap::new();
    for record in records {
        if !by_path.contains_key(&record.path) {
            order.push(record.path.clone());
        }
        by_path
            .entry(record.path.clone())
            .or_default()
            .push(record.clone());
    }
    order
        .into_iter()
        .map(|path| {
            let group = by_path.remove(&path).unwrap_or_default();
            let first = group.first().expect("non-empty group");
            let last = group.last().expect("non-empty group");
            let mut tool_names: BTreeSet<String> = BTreeSet::new();
            for record in &group {
                tool_names.insert(record.tool_name.clone());
            }
            RewindOperation {
                restore_content: first.before_content.clone(),
                action: if first.existed_before {
                    RewindAction::Restore
                } else {
                    RewindAction::Delete
                },
                expected_after: last.after_content.clone(),
                operation_count: group.len(),
                path,
                tool_names: tool_names.into_iter().collect(),
            }
        })
        .collect()
}

/// shell 来源的变更：无法证明能安全回滚，只能进 ignored 桶。
fn is_shell_tool(tool_names: &[String]) -> bool {
    tool_names.iter().any(|name| {
        let normalized = name.to_lowercase();
        normalized == "bash" || normalized == "shell" || normalized.ends_with("shell")
    })
}

/// 分桶预览。`read_current` 以相对路径读文件（host 用 workspace root 解析）。
pub fn preview(ops: &[RewindOperation], read_current: &dyn ReadCurrent) -> RewindPreview {
    let mut safe: Vec<SafeFile> = Vec::new();
    let mut unsafe_files: Vec<UnsafeFile> = Vec::new();
    let mut ignored: Vec<IgnoredFile> = Vec::new();

    for op in ops {
        if is_shell_tool(&op.tool_names) {
            ignored.push(IgnoredFile {
                operation_count: op.operation_count,
                path: op.path.clone(),
                reason: IGNORED_BASH,
                tool_names: op.tool_names.clone(),
            });
            continue;
        }
        // 无法确定"我们写出的那一版"是什么，就没法判断当前内容是否是外部改的。
        let Some(expected) = &op.expected_after else {
            unsafe_files.push(UnsafeFile {
                operation_count: op.operation_count,
                path: op.path.clone(),
                reason: UNSAFE_UNSUPPORTED_CHECKPOINT,
                tool_names: op.tool_names.clone(),
                message: Some("checkpoint cannot resolve the content this host wrote".to_string()),
                expected_hash: None,
                current_hash: None,
            });
            continue;
        };
        match read_current.read(&op.path) {
            Err(message) => unsafe_files.push(UnsafeFile {
                operation_count: op.operation_count,
                path: op.path.clone(),
                reason: UNSAFE_FILE_READ_FAILED,
                tool_names: op.tool_names.clone(),
                message: Some(message),
                expected_hash: Some(content_hash(Some(expected))),
                current_hash: None,
            }),
            Ok(current) => {
                let expected_hash = content_hash(Some(expected));
                let current_hash = content_hash(current.as_deref());
                if current_hash != expected_hash {
                    unsafe_files.push(UnsafeFile {
                        operation_count: op.operation_count,
                        path: op.path.clone(),
                        // 含"文件已被用户删掉"这一档：missing 的 hash 是哨兵值
                        // `"missing"`，与 expected 不等 → 同样是 external_modified
                        // （TS 用同一个 reason，不另开枚举）。
                        reason: UNSAFE_EXTERNAL_MODIFIED,
                        tool_names: op.tool_names.clone(),
                        message: None,
                        expected_hash: Some(expected_hash),
                        current_hash: Some(current_hash),
                    });
                } else {
                    safe.push(SafeFile {
                        action: op.action,
                        operation_count: op.operation_count,
                        path: op.path.clone(),
                        tool_names: op.tool_names.clone(),
                    });
                }
            }
        }
    }

    RewindPreview {
        can_apply: !safe.is_empty() && unsafe_files.is_empty(),
        ignored_files: ignored,
        safe_files: safe,
        unsafe_files,
    }
}

pub type ReadFile = dyn Fn(&Path) -> Result<Option<String>, String> + Send + Sync;
pub type WriteFile = dyn Fn(&Path, &str) -> Result<(), String> + Send + Sync;
pub type RemoveFile = dyn Fn(&Path) -> Result<(), String> + Send + Sync;

/// 真实/替身文件系统三件套。产品路径由 `commands/file_rewind.rs` 用 std::fs 装配。
pub struct FsAdapter {
    pub read: Box<ReadFile>,
    pub write: Box<WriteFile>,
    pub remove: Box<RemoveFile>,
}

impl FsAdapter {
    /// 真实磁盘：读不存在的文件 = `Ok(None)`（与 preview 的 missing 语义一致），
    /// 其它读错误向上抛；写自动建父目录。
    pub fn local(root: PathBuf) -> Self {
        // 三个闭包各自持有一份同一个根的句柄：read/write/remove 必须解析到同一个
        // workspace root，否则补偿会写到别的目录里去。
        let read_root = Arc::new(root.clone());
        let write_root = Arc::new(root.clone());
        let remove_root = Arc::new(root);
        Self {
            read: Box::new(move |relative: &Path| {
                let path = read_root.join(relative);
                match std::fs::read_to_string(&path) {
                    Ok(content) => Ok(Some(content)),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(format!("read {}: {error}", path.display())),
                }
            }),
            write: Box::new(move |relative: &Path, content: &str| {
                let path = write_root.join(relative);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|error| format!("mkdir {}: {error}", parent.display()))?;
                }
                std::fs::write(&path, content).map_err(|error| format!("write {path:?}: {error}"))
            }),
            remove: Box::new(move |relative: &Path| {
                let path = remove_root.join(relative);
                match std::fs::remove_file(&path) {
                    // 已经不在了就算删除成功（幂等），其它错误上报。
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(error) => Err(format!("remove {path:?}: {error}")),
                }
            }),
        }
    }
}

/// 补偿用的"改动前状态"日志（缺文件记 `None`，读失败直接判定为不可应用）。
struct JournalEntry {
    path: String,
    content_before: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyReport {
    pub applied: bool,
    pub restored_paths: Vec<String>,
    /// 拒绝/失败时对客户端说的原话（TS 逐字对齐）。
    pub response: Option<String>,
    pub unsafe_files: Vec<UnsafeFile>,
}

const REFUSE_UNSAFE: &str = "File rewind was not applied because at least one file is unsafe.";
const REFUSE_NO_SAFE: &str =
    "File rewind was not applied because there is no file left that can be safely restored.";
const FAILED_WITH_COMPENSATION: &str =
    "File rewind was not applied because the write failed; prior files were restored.";

/// 逆序应用安全文件；失败则按 journal 反向补偿。
///
/// WHY 逆序：同一文件被跨轮/同轮改过时，只有从最后一次改动倒着退，才能每一步都
/// 落在"上一次的 beforeContent"上；正序会把中间版本当成终点。
pub fn apply(ops: &[RewindOperation], preview: &RewindPreview, fs: &FsAdapter) -> ApplyReport {
    if !preview.unsafe_files.is_empty() {
        return ApplyReport {
            applied: false,
            restored_paths: Vec::new(),
            response: Some(REFUSE_UNSAFE.to_string()),
            unsafe_files: preview.unsafe_files.clone(),
        };
    }
    if preview.safe_files.is_empty() {
        return ApplyReport {
            applied: false,
            restored_paths: Vec::new(),
            response: Some(REFUSE_NO_SAFE.to_string()),
            unsafe_files: Vec::new(),
        };
    }
    let safe_paths: BTreeSet<&str> = preview
        .safe_files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    let ordered: Vec<&RewindOperation> = ops
        .iter()
        .filter(|op| safe_paths.contains(op.path.as_str()))
        .rev()
        .collect();

    let mut journal: Vec<JournalEntry> = Vec::new();
    let mut restored: Vec<String> = Vec::new();
    for op in ordered {
        let relative = Path::new(&op.path);
        // 先把"当前状态"记进 journal：补偿要回到的是这一步之前，而不是本轮之前。
        let current = match (fs.read)(relative) {
            Ok(current) => current,
            Err(message) => {
                compensate(&journal, fs);
                return ApplyReport {
                    applied: false,
                    restored_paths: Vec::new(),
                    response: Some(format!(
                        "{FAILED_WITH_COMPENSATION} read failed for {}: {message}",
                        op.path
                    )),
                    unsafe_files: vec![UnsafeFile {
                        operation_count: op.operation_count,
                        path: op.path.clone(),
                        reason: UNSAFE_FILE_READ_FAILED,
                        tool_names: op.tool_names.clone(),
                        message: Some(message),
                        expected_hash: None,
                        current_hash: None,
                    }],
                };
            }
        };
        journal.push(JournalEntry {
            path: op.path.clone(),
            content_before: current,
        });
        let result = match (&op.restore_content, op.action) {
            (None, _) | (_, RewindAction::Delete) => (fs.remove)(relative),
            (Some(content), RewindAction::Restore) => (fs.write)(relative, content),
        };
        if let Err(message) = result {
            compensate(&journal, fs);
            return ApplyReport {
                applied: false,
                restored_paths: Vec::new(),
                response: Some(format!(
                    "{FAILED_WITH_COMPENSATION} write failed for {}: {message}",
                    op.path
                )),
                unsafe_files: vec![UnsafeFile {
                    operation_count: op.operation_count,
                    path: op.path.clone(),
                    reason: UNSAFE_FILE_READ_FAILED,
                    tool_names: op.tool_names.clone(),
                    message: Some(message),
                    expected_hash: None,
                    current_hash: None,
                }],
            };
        }
        restored.push(op.path.clone());
    }
    restored.reverse();
    ApplyReport {
        applied: true,
        restored_paths: restored,
        response: None,
        unsafe_files: Vec::new(),
    }
}

/// journal 逆序补偿：本来就缺的文件删回去，有内容的写回去。
fn compensate(journal: &[JournalEntry], fs: &FsAdapter) {
    for entry in journal.iter().rev() {
        let relative = Path::new(&entry.path);
        match &entry.content_before {
            None => {
                let _ = (fs.remove)(relative);
            }
            Some(content) => {
                let _ = (fs.write)(relative, content);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    fn record(
        path: &str,
        tool: &str,
        before: Option<&str>,
        after: Option<&str>,
    ) -> FileChangeRecord {
        FileChangeRecord {
            turn_id: "t1".to_string(),
            tool_call_id: format!("call-{path}-{tool}"),
            tool_name: tool.to_string(),
            path: path.to_string(),
            existed_before: before.is_some(),
            before_content: before.map(str::to_string),
            after_content: after.map(str::to_string),
            additions: 1,
            deletions: if before.is_some() { 1 } else { 0 },
        }
    }

    /// 内存文件系统替身：只服务被测的分桶/补偿逻辑，不碰磁盘。
    struct FakeFs {
        files: Arc<Mutex<HashMap<String, Option<String>>>>,
        fail_write_for: Arc<Mutex<Option<String>>>,
        fail_read_for: Arc<Mutex<Option<String>>>,
    }

    impl FakeFs {
        fn new(files: &[(&str, &str)]) -> Self {
            let map: HashMap<String, Option<String>> = files
                .iter()
                .map(|(path, content)| (path.to_string(), Some(content.to_string())))
                .collect();
            Self {
                files: Arc::new(Mutex::new(map)),
                fail_write_for: Arc::new(Mutex::new(None)),
                fail_read_for: Arc::new(Mutex::new(None)),
            }
        }

        fn adapter(&self) -> FsAdapter {
            let read_files = Arc::clone(&self.files);
            let write_files = Arc::clone(&self.files);
            let remove_files = Arc::clone(&self.files);
            let fail_write = Arc::clone(&self.fail_write_for);
            let fail_read = Arc::clone(&self.fail_read_for);
            FsAdapter {
                read: Box::new(move |path: &Path| {
                    let key = path.to_string_lossy().to_string();
                    if fail_read.lock().unwrap().as_deref() == Some(key.as_str()) {
                        return Err(format!("io error on {key}"));
                    }
                    let guard = read_files.lock().unwrap();
                    match guard.get(&key) {
                        Some(Some(content)) => Ok(Some(content.clone())),
                        Some(None) => Ok(None),
                        None => Ok(None),
                    }
                }),
                write: Box::new(move |path: &Path, content: &str| {
                    let key = path.to_string_lossy().to_string();
                    if fail_write.lock().unwrap().as_deref() == Some(key.as_str()) {
                        return Err(format!("disk full on {key}"));
                    }
                    write_files
                        .lock()
                        .unwrap()
                        .insert(key, Some(content.to_string()));
                    Ok(())
                }),
                remove: Box::new(move |path: &Path| {
                    let key = path.to_string_lossy().to_string();
                    remove_files.lock().unwrap().insert(key, None);
                    Ok(())
                }),
            }
        }

        /// 预览用的只读视图（自带 Arc 句柄，不借用 self 的生命周期）。
        fn reader(&self) -> impl Fn(&str) -> Result<Option<String>, String> + Send + Sync {
            let files = Arc::clone(&self.files);
            move |path: &str| Ok(files.lock().unwrap().get(path).cloned().flatten())
        }

        fn content(&self, path: &str) -> Option<String> {
            self.files.lock().unwrap().get(path).cloned().flatten()
        }
    }

    fn reader(files: &[(&str, &str)]) -> (FakeFs, impl Fn(&str) -> Result<Option<String>, String>) {
        let fs = FakeFs::new(files);
        let handle = Arc::clone(&fs.files);
        let read = move |path: &str| -> Result<Option<String>, String> {
            Ok(handle.lock().unwrap().get(path).cloned().flatten())
        };
        (fs, read)
    }

    #[test]
    fn created_file_is_deleted_and_modified_file_restored() {
        let records = vec![
            record("new.txt", "Write", None, Some("brand new")),
            record("old.txt", "Edit", Some("original"), Some("changed")),
        ];
        let ops = operations(&records);
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].action, RewindAction::Delete);
        assert_eq!(ops[1].action, RewindAction::Restore);
        assert_eq!(ops[1].restore_content.as_deref(), Some("original"));

        let (_fs, read) = reader(&[("new.txt", "brand new"), ("old.txt", "changed")]);
        let preview = preview(&ops, &read);
        assert!(preview.can_apply, "{preview:?}");
        assert_eq!(preview.safe_files.len(), 2);
        assert!(preview.unsafe_files.is_empty());
    }

    #[test]
    fn external_modification_blocks_the_whole_rewind() {
        let records = vec![record("a.txt", "Write", None, Some("v1"))];
        let ops = operations(&records);
        // 用户自己又改过：当前内容 != 我们写出的那一版。
        let (_fs, read) = reader(&[("a.txt", "edited by hand")]);
        let preview = preview(&ops, &read);
        assert!(!preview.can_apply);
        assert_eq!(preview.unsafe_files[0].reason, UNSAFE_EXTERNAL_MODIFIED);
        assert!(preview.unsafe_files[0].expected_hash.is_some());
        assert!(preview.unsafe_files[0].current_hash.is_some());
    }

    #[test]
    fn user_deleted_file_counts_as_external_modification() {
        let records = vec![record("a.txt", "Write", Some("before"), Some("after"))];
        let ops = operations(&records);
        // 文件已经不在了：hash 用哨兵 "missing"，与 expected 不等 → 拒绝整轮回滚。
        let read = |_path: &str| Ok(None);
        let preview = preview(&ops, &read);
        assert!(!preview.can_apply);
        assert_eq!(preview.unsafe_files[0].reason, UNSAFE_EXTERNAL_MODIFIED);
        assert_eq!(
            preview.unsafe_files[0].current_hash.as_deref(),
            Some("missing"),
            "缺失文件的身份必须是协议里那个哨兵值，不是空串"
        );
    }

    #[test]
    fn read_failure_goes_to_file_read_failed_bucket() {
        let records = vec![record("a.txt", "Write", None, Some("v1"))];
        let ops = operations(&records);
        let read = |_path: &str| Err("EACCES".to_string());
        let preview = preview(&ops, &read);
        assert!(!preview.can_apply);
        assert_eq!(preview.unsafe_files[0].reason, UNSAFE_FILE_READ_FAILED);
        assert_eq!(preview.unsafe_files[0].message.as_deref(), Some("EACCES"));
    }

    #[test]
    fn shell_touched_paths_are_ignored_not_guessed() {
        let records = vec![record(
            "script.sh",
            "Bash",
            Some("#!/sh"),
            Some("#!/sh changed"),
        )];
        let ops = operations(&records);
        let (_fs, read) = reader(&[("script.sh", "#!/sh changed")]);
        let preview = preview(&ops, &read);
        assert_eq!(preview.ignored_files.len(), 1);
        assert_eq!(preview.ignored_files[0].reason, IGNORED_BASH);
        assert!(preview.safe_files.is_empty());
        assert!(!preview.can_apply, "no safe file = nothing to apply");
    }

    #[test]
    fn missing_after_content_is_an_unsupported_checkpoint() {
        let mut record = record("a.txt", "Write", None, Some("v1"));
        record.after_content = None;
        let ops = operations(&[record]);
        let (_fs, read) = reader(&[("a.txt", "whatever")]);
        let preview = preview(&ops, &read);
        assert_eq!(
            preview.unsafe_files[0].reason,
            UNSAFE_UNSUPPORTED_CHECKPOINT
        );
    }

    #[test]
    fn repeated_writes_in_one_turn_collapse_to_first_before_and_last_after() {
        let records = vec![
            record("a.txt", "Write", None, Some("v1")),
            record("a.txt", "Edit", Some("v1"), Some("v2")),
        ];
        let ops = operations(&records);
        assert_eq!(ops.len(), 1, "same path is one rewind operation");
        assert_eq!(ops[0].operation_count, 2);
        assert_eq!(
            ops[0].action,
            RewindAction::Delete,
            "before the turn it did not exist"
        );
        assert_eq!(ops[0].expected_after.as_deref(), Some("v2"));
    }

    #[test]
    fn apply_restores_in_reverse_order_and_reports_paths() {
        let records = vec![
            record("a.txt", "Write", Some("a before"), Some("a after")),
            record("b.txt", "Write", None, Some("b new")),
        ];
        let ops = operations(&records);
        let fs = FakeFs::new(&[("a.txt", "a after"), ("b.txt", "b new")]);
        let handle = Arc::clone(&fs.files);
        let read = move |path: &str| -> Result<Option<String>, String> {
            Ok(handle.lock().unwrap().get(path).cloned().flatten())
        };
        let preview = preview(&ops, &read);
        assert!(preview.can_apply, "{preview:?}");
        let report = apply(&ops, &preview, &fs.adapter());
        assert!(report.applied, "{report:?}");
        assert_eq!(
            report.restored_paths,
            vec!["a.txt".to_string(), "b.txt".to_string()]
        );
        assert_eq!(fs.content("a.txt").as_deref(), Some("a before"));
        assert_eq!(fs.content("b.txt"), None, "created file is removed");
    }

    #[test]
    fn apply_refuses_when_any_file_is_unsafe() {
        let records = vec![
            record("a.txt", "Write", Some("a before"), Some("a after")),
            record("b.txt", "Write", Some("b before"), Some("b after")),
        ];
        let ops = operations(&records);
        let fs = FakeFs::new(&[("a.txt", "a after"), ("b.txt", "tampered by hand")]);
        let preview = preview(&ops, &fs.reader());
        let report = apply(&ops, &preview, &fs.adapter());
        assert!(!report.applied);
        assert_eq!(
            report.response.as_deref(),
            Some("File rewind was not applied because at least one file is unsafe.")
        );
        // 拒绝必须是零副作用：安全文件也不能被提前改。
        assert_eq!(fs.content("a.txt").as_deref(), Some("a after"));
    }

    #[test]
    fn mid_apply_failure_compensates_every_already_touched_file() {
        let records = vec![
            record("a.txt", "Write", Some("a before"), Some("a after")),
            record("b.txt", "Write", Some("b before"), Some("b after")),
        ];
        let ops = operations(&records);
        let fs = FakeFs::new(&[("a.txt", "a after"), ("b.txt", "b after")]);
        // 逆序应用：先 a.txt（成功），再 b.txt（写失败）→ a.txt 必须被补偿回去。
        *fs.fail_write_for.lock().unwrap() = Some("b.txt".to_string());
        let preview = preview(&ops, &fs.reader());
        let report = apply(&ops, &preview, &fs.adapter());
        assert!(!report.applied);
        assert!(report
            .response
            .as_deref()
            .is_some_and(|text| text.starts_with(
                "File rewind was not applied because the write failed; prior files were restored."
            )));
        assert_eq!(
            fs.content("a.txt").as_deref(),
            Some("a after"),
            "补偿把已经改过的文件恢复到本次 rewind 之前的状态"
        );
        assert_eq!(fs.content("b.txt").as_deref(), Some("b after"));
    }

    #[test]
    fn preview_json_uses_the_protocol_field_names() {
        let records = vec![record("a.txt", "Write", None, Some("v1"))];
        let ops = operations(&records);
        let (_fs, read) = reader(&[("a.txt", "v1")]);
        let value = preview(&ops, &read).to_json();
        assert_eq!(value["canApply"], json!(true));
        assert_eq!(value["safeFiles"][0]["action"], json!("delete"));
        assert_eq!(value["safeFiles"][0]["operationCount"], json!(1));
        assert!(value["safeFiles"][0]["toolNames"].is_array());
    }
}
