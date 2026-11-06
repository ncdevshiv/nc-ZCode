//! 文件变更台账：每一轮里被写工具改过的文件，改之前是什么内容。
//!
//! 这是 `applyFileRewind`（以及 `editUserQuery` 的 `workspaceMode:"rewind"`）的事实
//! 来源。TS 侧同样的账本落在 artifact store + session event 两处（
//! `WorkspaceCheckpointArtifact` + `CheckpointCreated`）；本 host 只有一份持久化通道
//! ——`transcript.jsonl`——所以台账作为 `t:"fileChange"` 事件与行投影同轨追加。
//!
//! 设计约束：
//! - **只有真正落盘的写工具进账**（Write / Edit）。Bash 改的文件不记账，rewind 时靠
//!   内容比对判定 `external_modified` 并不强改（对齐 TS 的 shell-ignored 语义：
//!   shell 来源的变更无法证明能安全回滚）。
//! - 台账不直接依赖 `TranscriptStore`：落盘经一个可注入的 sink（`LedgerSink`），
//!   这样台账能被单独测试、也能被别的宿主（远程/测试夹具）替换成别的持久化实现。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// 一次写工具调用留下的最小可回滚事实。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeRecord {
    pub turn_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    /// 相对 workspace root 的路径（rewind 必须用同一个根再解析，不存绝对路径）。
    pub path: String,
    /// 改动前文件是否存在（`existedBefore`：不存在 → rewind 时应删除该文件）。
    pub existed_before: bool,
    /// 改动前内容；`None` = 当时不存在或读不出（读不出时由调用方拒绝记账）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_content: Option<String>,
    /// 改动后内容（用于"当前内容是否仍等于我们写的最后一版"的判定）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_content: Option<String>,
    pub additions: usize,
    pub deletions: usize,
}

/// 一轮的文件变更汇总（`turnHeaderRowSchema.fileChanges` 的形状）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeSummary {
    pub additions: usize,
    pub deletions: usize,
    pub files: usize,
}

/// 台账落盘通道：由宿主接 `TranscriptStore`，失败必须能被调用方观测到。
pub type LedgerSink = Box<dyn Fn(&FileChangeRecord) -> Result<(), String> + Send + Sync>;

/// 会话级文件变更台账（`Session` 独占一个实例，工具经 `ToolContext` 写它）。
#[derive(Default)]
pub struct FileLedger {
    records: Mutex<Vec<FileChangeRecord>>,
    sink: Mutex<Option<Arc<LedgerSink>>>,
    /// sink 写失败的次数：台账不落盘 = rewind 会缺事实，必须可观测而不是静默。
    dropped: AtomicUsize,
}

impl FileLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// 接上持久化通道（host 在创建/恢复会话时注入）。
    pub fn attach_sink(&self, sink: LedgerSink) {
        if let Ok(mut guard) = self.sink.lock() {
            *guard = Some(Arc::new(sink));
        }
    }

    /// 记账一次文件变更。**先落盘再入内存**：与 host 所有写路径一致，
    /// 落盘失败时内存不会领先于磁盘（那样重启后这段变更就"看得见却回滚不了"）。
    pub fn record(&self, record: FileChangeRecord) -> Result<(), String> {
        let sink = self.sink.lock().ok().and_then(|guard| guard.clone());
        if let Some(sink) = sink {
            if let Err(error) = sink(&record) {
                self.dropped.fetch_add(1, Ordering::SeqCst);
                return Err(error);
            }
        }
        if let Ok(mut guard) = self.records.lock() {
            guard.push(record);
        }
        Ok(())
    }

    /// 冷恢复：把已回放的记录放回台账（不重复走 sink，磁盘就是来源）。
    pub fn seed(&self, records: &[FileChangeRecord]) {
        if let Ok(mut guard) = self.records.lock() {
            guard.extend(records.iter().cloned());
        }
    }

    pub fn records(&self) -> Vec<FileChangeRecord> {
        self.records
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// 某一轮的全部变更，按发生顺序。
    pub fn for_turn(&self, turn_id: &str) -> Vec<FileChangeRecord> {
        self.records()
            .into_iter()
            .filter(|record| record.turn_id == turn_id)
            .collect()
    }

    /// 某路径当前"我们写出的最后一版"内容（跨轮，供 `external_modified` 比对）。
    pub fn last_written(&self, path: &str) -> Option<String> {
        self.records()
            .into_iter()
            .rev()
            .find(|record| record.path == path)
            .and_then(|record| record.after_content)
    }

    /// 一轮的文件变更汇总：additions/deletions 逐条累加，files 是去重后的路径数。
    pub fn summary_for_turn(&self, turn_id: &str) -> Option<FileChangeSummary> {
        let records = self.for_turn(turn_id);
        if records.is_empty() {
            return None;
        }
        let mut paths: BTreeMap<&str, ()> = BTreeMap::new();
        let mut additions = 0usize;
        let mut deletions = 0usize;
        for record in &records {
            paths.insert(record.path.as_str(), ());
            additions = additions.saturating_add(record.additions);
            deletions = deletions.saturating_add(record.deletions);
        }
        Some(FileChangeSummary {
            additions,
            deletions,
            files: paths.len(),
        })
    }

    /// 丢掉若干轮次的记录（分支截断后，被截掉的轮不再可回滚）。
    pub fn drop_turns(&self, turn_ids: &[String]) {
        if turn_ids.is_empty() {
            return;
        }
        if let Ok(mut guard) = self.records.lock() {
            guard.retain(|record| !turn_ids.iter().any(|id| id == &record.turn_id));
        }
    }

    /// sink 写失败次数（>0 就说明台账不完整，rewind 的事实来源有洞）。
    pub fn dropped_records(&self) -> usize {
        self.dropped.load(Ordering::SeqCst)
    }
}

/// 行数上限：超过就退化成"整块删除 + 整块新增"的保守计数。
///
/// WHY：LCS 是 O(n·m) 内存，10k 行的文件能一次吃掉几百 MB。TS 侧用
/// `structuredPatch(before, after, { timeout: 5000 })` 并在超时时退化到已有 patch 行数，
/// 本 host 用同样的"有界近似"策略：小文件精确、大文件保守，且绝不无界。
const DIFF_LINE_CAP: usize = 4096;

/// shell 类工具名：它们的变更**不进台账**，rewind 只能以 `external_modified`
/// 出现，绝不由 host 猜着回滚（对齐 TS `isIgnoredShellTool`）。
pub fn is_shell_tool_name(name: &str) -> bool {
    let normalized = name.to_lowercase();
    normalized == "bash" || normalized == "shell" || normalized.ends_with("shell")
}

/// 建一个已经把落盘接到 transcript 的台账（`Session` 唯一使用的构造入口）。
///
/// WHY 放在台账自己这边：sink 的装配需要知道 store 与 sessionId，而这两件事都属于
/// "持久化细节"；让 `Session::new` 内联一段闭包等于把存储知识漏进构造函数，
/// 换一个存储实现就要改两处。
pub fn new_with_transcript_sink(
    session_id: &str,
    store: &crate::store::TranscriptStore,
    seed: Vec<FileChangeRecord>,
) -> Arc<FileLedger> {
    let ledger = Arc::new(FileLedger::new());
    ledger.seed(&seed);
    let store = store.clone();
    let session_id = session_id.to_string();
    ledger.attach_sink(Box::new(move |record| {
        store
            .append_file_change(&session_id, record)
            .map_err(|error| error.to_string())
    }));
    ledger
}

/// 一次写工具调用在台账里的"改动前"快照。
///
/// WHY 由 tool-loop 采集而不是每个工具自己上报：记账条件是结构性的
/// （非只读 + workspace 副作用 + 输入里有 `file_path`），每个工具自己上报等于把
/// 同一条规则抄 N 遍，漏一个就是一个"改了文件但回滚不了"的洞。
pub struct PendingChange {
    pub path: String,
    pub absolute: std::path::PathBuf,
    pub existed_before: bool,
    pub before_content: Option<String>,
}

/// 执行前采集；返回 `None` = 这次调用不需要记账（或既有文件读不出来，宁可不记）。
pub fn capture_before(
    ctx: &crate::tools::ToolContext,
    read_only: bool,
    side_effect_scope: &str,
    tool_name: &str,
    input: &serde_json::Value,
) -> Option<PendingChange> {
    if read_only
        || side_effect_scope != crate::tools::SCOPE_WORKSPACE
        || is_shell_tool_name(tool_name)
    {
        return None;
    }
    let raw = input.get("file_path").and_then(serde_json::Value::as_str)?;
    // 复用工具层的 workspace 解析（越界路径必须在这里就被拒掉，不能自己写一份
    // 宽松版：那等于给台账开后门）。
    let absolute = crate::tools::fs_tools::resolve_workspace_path(ctx, raw).ok()?;
    let existed_before = absolute.is_file();
    if !existed_before {
        return Some(PendingChange {
            path: raw.to_string(),
            absolute,
            existed_before: false,
            before_content: None,
        });
    }
    // 已存在但读不出来（二进制/权限）：不记账。半条记录比没有记录更危险——
    // 它会诱使 rewind 用 `None` 当成"原本不存在"而删掉一个 host 从没读过的文件。
    let before_content = std::fs::read_to_string(&absolute).ok()?;
    Some(PendingChange {
        path: raw.to_string(),
        absolute,
        existed_before: true,
        before_content: Some(before_content),
    })
}

/// 执行成功后落账。当前内容读不出来（二进制/被工具删掉）时记 `afterContent: null`，
/// 由 rewind 的 preview 判成 `unsupported_checkpoint`——**拒绝回滚**而不是猜。
pub fn record_after(
    ledger: &FileLedger,
    turn_id: &str,
    tool_call_id: &str,
    tool_name: &str,
    pending: PendingChange,
) -> Result<(), String> {
    let after_content = std::fs::read_to_string(&pending.absolute).ok();
    let (additions, deletions) =
        line_change_counts(pending.before_content.as_deref(), after_content.as_deref());
    ledger.record(FileChangeRecord {
        turn_id: turn_id.to_string(),
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        path: pending.path,
        existed_before: pending.existed_before,
        before_content: pending.before_content,
        after_content,
        additions,
        deletions,
    })
}

/// 行级增删计数（`turnHeader.fileChanges` 的数字来源）。
/// `None` = 文件当时不存在（全算新增）/ 文件被删（全算删除）。
pub fn line_change_counts(before: Option<&str>, after: Option<&str>) -> (usize, usize) {
    let before_lines: Vec<&str> = before.map(split_lines).unwrap_or_default();
    let after_lines: Vec<&str> = after.map(split_lines).unwrap_or_default();
    if before_lines.is_empty() {
        return (after_lines.len(), 0);
    }
    if after_lines.is_empty() {
        return (0, before_lines.len());
    }
    if before_lines == after_lines {
        return (0, 0);
    }
    let common = common_edges(&before_lines, &after_lines);
    let (b_mid, a_mid) = remaining(&before_lines, &after_lines, common);
    if before_lines.len() > DIFF_LINE_CAP || after_lines.len() > DIFF_LINE_CAP {
        // 超界：保守退化。共同前缀/后缀仍然削掉，剩下的整块算删+增。
        return (a_mid, b_mid);
    }
    let kept = lcs_len(
        &before_lines[common.0..before_lines.len() - common.1],
        &after_lines[common.0..after_lines.len() - common.1],
    );
    (a_mid - kept, b_mid - kept)
}

/// 按行切分，但不把**行尾换行符**当成一行。
///
/// WHY：`"a\nb\n"` 直接 split 会多出一个尾随空串，把 2 行的文件数成 3 行，
/// `turnHeader.fileChanges` 的增删数就与 TS 的 `structuredPatch` 对不上（多算一行）。
pub(crate) fn split_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if lines.last().copied() == Some("") {
        lines.pop();
    }
    lines
}

/// (共同前缀行数, 共同后缀行数)。
fn common_edges(before: &[&str], after: &[&str]) -> (usize, usize) {
    let prefix = before
        .iter()
        .zip(after.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = before[prefix..]
        .iter()
        .rev()
        .zip(after[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    (prefix, suffix)
}

fn remaining(before: &[&str], after: &[&str], edges: (usize, usize)) -> (usize, usize) {
    (
        before.len().saturating_sub(edges.0 + edges.1),
        after.len().saturating_sub(edges.0 + edges.1),
    )
}

fn lcs_len(a: &[&str], b: &[&str]) -> usize {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    let mut prev = vec![0usize; b.len() + 1];
    let mut cur = vec![0usize; b.len() + 1];
    for ai in a {
        for j in 1..=b.len() {
            cur[j] = if b[j - 1] == *ai {
                prev[j - 1] + 1
            } else {
                prev[j].max(cur[j - 1])
            };
        }
        std::mem::swap(&mut prev, &mut cur);
        cur.iter_mut().for_each(|cell| *cell = 0);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(
        turn: &str,
        path: &str,
        before: Option<&str>,
        after: Option<&str>,
    ) -> FileChangeRecord {
        let (additions, deletions) = line_change_counts(before, after);
        FileChangeRecord {
            turn_id: turn.to_string(),
            tool_call_id: format!("call-{path}"),
            tool_name: "Write".to_string(),
            path: path.to_string(),
            existed_before: before.is_some(),
            before_content: before.map(str::to_string),
            after_content: after.map(str::to_string),
            additions,
            deletions,
        }
    }

    #[test]
    fn summary_aggregates_lines_and_dedupes_paths() {
        let ledger = FileLedger::new();
        ledger
            .record(record("t1", "a.txt", None, Some("one\ntwo\n")))
            .expect("record");
        ledger
            .record(record("t1", "b.txt", Some("x\n"), Some("x\ny\n")))
            .expect("record");
        // 同一路径第二次改动只算一个文件。
        ledger
            .record(record("t1", "b.txt", Some("x\ny\n"), Some("x\ny\nz\n")))
            .expect("record");
        let summary = ledger.summary_for_turn("t1").expect("summary");
        assert_eq!(summary.files, 2, "two distinct paths");
        assert_eq!(summary.additions, 4, "2 new + 1 new + 1 new");
        assert_eq!(summary.deletions, 0);
        assert!(ledger.summary_for_turn("t9").is_none());
    }

    #[test]
    fn last_written_is_the_latest_after_content_for_that_path() {
        let ledger = FileLedger::new();
        ledger
            .record(record("t1", "a.txt", None, Some("v1")))
            .expect("record");
        ledger
            .record(record("t2", "a.txt", Some("v1"), Some("v2")))
            .expect("record");
        assert_eq!(ledger.last_written("a.txt").as_deref(), Some("v2"));
        assert_eq!(ledger.for_turn("t2").len(), 1);
        assert_eq!(
            ledger.for_turn("t2")[0].before_content.as_deref(),
            Some("v1")
        );
    }

    #[test]
    fn record_failure_keeps_memory_behind_disk() {
        let ledger = FileLedger::new();
        ledger.attach_sink(Box::new(|_| Err("disk full".to_string())));
        let error = ledger
            .record(record("t1", "a.txt", None, Some("x")))
            .expect_err("sink failure must surface");
        assert_eq!(error, "disk full");
        assert!(
            ledger.records().is_empty(),
            "内存不得领先于磁盘：落盘失败的记录不能假装已经在账本里"
        );
        assert_eq!(ledger.dropped_records(), 1);
    }

    #[test]
    fn line_counts_handle_create_delete_and_noop() {
        // 尾随换行不算一行：`"a\nb\n"` 是 2 行的新文件（TS 的 structuredPatch 同样数 2）。
        assert_eq!(line_change_counts(None, Some("a\nb\n")), (2, 0));
        assert_eq!(line_change_counts(Some("a\nb\n"), None), (0, 2));
        assert_eq!(line_change_counts(Some("same\n"), Some("same\n")), (0, 0));
        assert_eq!(
            line_change_counts(Some("a\nb\nc\n"), Some("a\nx\nc\n")),
            (1, 1)
        );
    }

    /// 大文件必须走有界退化，而不是把 LCS 的 O(n·m) 内存打满。
    #[test]
    fn oversized_inputs_degrade_without_hanging() {
        let big_before: Vec<String> = (0..DIFF_LINE_CAP + 500)
            .map(|i| format!("line {i}"))
            .collect();
        let mut big_after = big_before.clone();
        big_after[DIFF_LINE_CAP + 499] = "changed".to_string();
        let before = big_before.join("\n");
        let after = big_after.join("\n");
        let (additions, deletions) = line_change_counts(Some(&before), Some(&after));
        assert!(
            additions >= 1 && deletions >= 1,
            "conservative counts, never zero"
        );
    }
}
