//! zcode-scan 可测核心：镜像 `packages/services/src/storage/adapters/fsWalker.ts` 的遍历算法。
//!
//! 顺序语义 = TS `concurrency = 1` 调度的复刻：LIFO pending 栈、每目录内按枚举序
//! 出文件、子目录延后（files-before-dirs、兄弟子树后进先出）。TS 默认
//! `concurrency = 4` 会把兄弟子树交错——交错顺序在 TS 侧本身就非确定，记录为已接受
//! 差异（见 README）；条目集合、计数、错误集合与并发度无关，仍与 TS 一致。
//!
//! TS 侧不存在的能力（无 ignore 集、无深度上限、恒不跟软链）在 sidecar 里是可选
//! 参数，**默认值全部对齐 TS 行为**：`recursive = true`、`maxDepth = 无`、
//! `followSymlinks = false`。

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// ready 事件中的 sidecar 名。
pub const SIDECAR_NAME: &str = "zcode-scan";

/// 扫描条目 —— 字段镜像 TS `StorageScanEntry`：`relativePath` / `bytes` / `mtimeMs`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanEntry {
    /// 根相对路径，`/` 分隔 —— 镜像 TS `relative().split(sep).join("/")`。
    pub relative_path: String,
    pub bytes: u64,
    /// Unix epoch 毫秒（整数；JS `stats.mtimeMs` 含小数，亚毫秒不可观测，截断见 README）。
    pub mtime_ms: u64,
}

/// 局部错误 —— 镜像 TS `StoragePathError`：根相对 `path` + Node errno 风格 `code`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanError {
    pub path: String,
    pub code: String,
}

/// 遍历汇总 —— 镜像 TS `WalkStorageRootResult` + sidecar 取消标记。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanSummary {
    pub directories_scanned: u64,
    pub files_scanned: u64,
    /// 根目录不存在（TS 语义：视为空根，**不算错误**）。
    pub missing_root: bool,
    /// 遍历期间观察到取消位（sidecar 扩展；对应 TS 侧 AbortError → cancelled 快照）。
    pub cancelled: bool,
}

/// 遍历选项。`Default` 即 TS 行为。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkOptions {
    /// false = 只扫根目录（≡ `maxDepth = 0`）；默认 true（TS 恒递归）。
    pub recursive: bool,
    /// 目录深度上限：根为 0，仅打开深度 ≤ maxDepth 的目录；默认无上限（同 TS）。
    pub max_depth: Option<u32>,
    /// 跟随软链（目录按 canonical 去重防环）；默认 false —— TS 跳过一切软链。
    pub follow_symlinks: bool,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            recursive: true,
            max_depth: None,
            follow_symlinks: false,
        }
    }
}

/// 遍历 `root`：每产出一个文件条目调用 `on_entry`，每个可恢复错误调用 `on_error`
/// 后**继续**（镜像 TS 容错语义）。`cancelled` 每进入一个目录/条目前检查，置位后尽快
/// 停止并以 `cancelled = true` 的部分汇总返回。
pub fn walk_storage_root<F, G>(
    root: &Path,
    options: &WalkOptions,
    cancelled: &AtomicBool,
    mut on_entry: F,
    mut on_error: G,
) -> ScanSummary
where
    F: FnMut(&ScanEntry),
    G: FnMut(&ScanError),
{
    let effective_max_depth = if options.recursive {
        options.max_depth
    } else {
        Some(0)
    };
    let mut summary = ScanSummary::default();
    let mut visited_symlink_targets: HashSet<PathBuf> = HashSet::new();
    if options.follow_symlinks {
        if let Ok(canonical) = fs::canonicalize(root) {
            visited_symlink_targets.insert(canonical);
        }
    }

    let mut pending: Vec<(PathBuf, u32)> = vec![(root.to_path_buf(), 0)];

    'walk: while let Some((directory, depth)) = pending.pop() {
        if cancelled.load(Ordering::SeqCst) {
            break 'walk;
        }
        let read_dir = match fs::read_dir(&directory) {
            Ok(read_dir) => read_dir,
            Err(err) => {
                if directory == root && err.kind() == io::ErrorKind::NotFound {
                    // TS：根不存在 = missingRoot（空根，不是错误）。
                    summary.missing_root = true;
                } else {
                    on_error(&ScanError {
                        path: to_relative(root, &directory),
                        code: node_error_code(&err),
                    });
                }
                continue;
            }
        };
        summary.directories_scanned += 1;

        for entry in read_dir {
            if cancelled.load(Ordering::SeqCst) {
                break 'walk;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    // 枚举中途失败：报目录级错误后继续（TS opendir 迭代同为容错回调）。
                    on_error(&ScanError {
                        path: to_relative(root, &directory),
                        code: node_error_code(&err),
                    });
                    continue;
                }
            };
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(err) => {
                    report_lstat_error(&mut on_error, root, &path, &err);
                    continue;
                }
            };

            if file_type.is_symlink() {
                // TS：isSymbolicLink → 无条件跳过（从不跟随）。
                if !options.follow_symlinks {
                    continue;
                }
                // followSymlinks=true（sidecar 扩展）：按目标类型处理；目录按
                // canonical 去重防环（策略镜像 skillDiscoveryWalk）。
                let metadata = match fs::metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(err) => {
                        report_lstat_error(&mut on_error, root, &path, &err);
                        continue;
                    }
                };
                if metadata.is_dir() {
                    let canonical = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                    if visited_symlink_targets.insert(canonical) {
                        push_dir(&mut pending, path, depth, effective_max_depth);
                    }
                    continue;
                }
                if metadata.is_file() {
                    emit_file(&mut summary, &mut on_entry, root, &path, &metadata);
                }
                continue;
            }

            if file_type.is_dir() {
                push_dir(&mut pending, path, depth, effective_max_depth);
                continue;
            }
            if !file_type.is_file() {
                // TS：既非目录也非文件（管道/套接字/设备）→ 跳过。
                continue;
            }
            match entry.metadata() {
                Ok(metadata) if metadata.is_file() => {
                    emit_file(&mut summary, &mut on_entry, root, &path, &metadata);
                }
                Ok(_) => {}
                Err(err) => report_lstat_error(&mut on_error, root, &path, &err),
            }
        }
    }

    summary.cancelled = cancelled.load(Ordering::SeqCst);
    summary
}

fn push_dir(pending: &mut Vec<(PathBuf, u32)>, path: PathBuf, depth: u32, max_depth: Option<u32>) {
    let child_depth = depth + 1;
    if max_depth.is_none_or(|max| child_depth <= max) {
        pending.push((path, child_depth));
    }
}

fn emit_file<F: FnMut(&ScanEntry)>(
    summary: &mut ScanSummary,
    on_entry: &mut F,
    root: &Path,
    path: &Path,
    metadata: &fs::Metadata,
) {
    summary.files_scanned += 1;
    on_entry(&ScanEntry {
        relative_path: to_relative(root, path),
        bytes: metadata.len(),
        mtime_ms: mtime_ms(metadata),
    });
}

/// TS：lstat 失败时 ENOENT 属正常竞态（扫描期间文件被轮转/删除），静默跳过；
/// 其余错误上报后继续。
fn report_lstat_error<G: FnMut(&ScanError)>(
    on_error: &mut G,
    root: &Path,
    path: &Path,
    err: &io::Error,
) {
    if err.kind() != io::ErrorKind::NotFound {
        on_error(&ScanError {
            path: to_relative(root, path),
            code: node_error_code(err),
        });
    }
}

/// 相对根路径，`/` 分隔 —— 镜像 TS `relative().split(sep).join("/")`。
pub fn to_relative(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

/// `io::Error` → Node errno 字符串（镜像 TS `errorCode()`：取 `error.code`，
/// 缺失兜底 `"UNKNOWN"`）。表只收录两平台都明确的原始码，其余走 `ErrorKind` 兜底。
pub fn node_error_code(err: &io::Error) -> String {
    if let Some(raw) = err.raw_os_error() {
        let mapped = if cfg!(windows) {
            // Windows：ERROR_FILE_NOT_FOUND/PATH_NOT_FOUND、ALREADY_EXISTS、
            // DIRECTORY（read_dir 指向文件）、DISK_FULL、ACCESS_DENIED。
            match raw {
                2 | 3 => Some("ENOENT"),
                5 => Some("EACCES"),
                112 => Some("ENOSPC"),
                183 => Some("EEXIST"),
                267 => Some("ENOTDIR"),
                _ => None,
            }
        } else {
            // POSIX errno：EPERM/ENOENT/EACCES/EEXIST/ENOTDIR/EISDIR/EINVAL/ENOSPC。
            match raw {
                1 => Some("EPERM"),
                2 => Some("ENOENT"),
                13 => Some("EACCES"),
                17 => Some("EEXIST"),
                20 => Some("ENOTDIR"),
                21 => Some("EISDIR"),
                22 => Some("EINVAL"),
                28 => Some("ENOSPC"),
                _ => None,
            }
        };
        if let Some(code) = mapped {
            return code.to_string();
        }
    }
    match err.kind() {
        io::ErrorKind::NotFound => "ENOENT",
        io::ErrorKind::PermissionDenied => "EACCES",
        io::ErrorKind::AlreadyExists => "EEXIST",
        io::ErrorKind::InvalidInput => "EINVAL",
        _ => "UNKNOWN",
    }
    .to_string()
}

fn mtime_ms(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        // 1970 年前时间戳（负值）归 0，保证输出是非负 JSON 整数。
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use tempfile::tempdir;

    use super::*;

    struct Collected {
        entries: Vec<ScanEntry>,
        errors: Vec<ScanError>,
        summary: ScanSummary,
    }

    fn run(root: &Path, options: &WalkOptions) -> Collected {
        run_with_cancel(root, options, &AtomicBool::new(false))
    }

    fn run_with_cancel(root: &Path, options: &WalkOptions, cancelled: &AtomicBool) -> Collected {
        let mut entries = Vec::new();
        let mut errors = Vec::new();
        let summary = walk_storage_root(
            root,
            options,
            cancelled,
            |entry| entries.push(entry.clone()),
            |error| errors.push(error.clone()),
        );
        Collected {
            entries,
            errors,
            summary,
        }
    }

    fn paths_of(result: &Collected) -> Vec<String> {
        result
            .entries
            .iter()
            .map(|entry| entry.relative_path.clone())
            .collect()
    }

    /// 顺序 oracle：TS `concurrency = 1` 的可执行规格 —— 每目录按枚举序先出全部
    /// 文件、子目录按枚举序压栈（LIFO 使兄弟子树逆序），独立于被测实现的内部写法。
    fn reference_order(root: &Path) -> Vec<String> {
        let mut pending = vec![root.to_path_buf()];
        let mut ordered = Vec::new();
        while let Some(directory) = pending.pop() {
            let mut child_dirs = Vec::new();
            let mut child_files = Vec::new();
            for entry in fs::read_dir(&directory).expect("read_dir") {
                let entry = entry.expect("entry");
                let file_type = entry.file_type().expect("file_type");
                if file_type.is_dir() {
                    child_dirs.push(entry.path());
                } else if file_type.is_file() {
                    child_files.push(entry.path());
                }
            }
            for file in child_files {
                ordered.push(to_relative(root, &file));
            }
            for child in child_dirs {
                pending.push(child);
            }
        }
        ordered
    }

    fn build_ordering_tree(root: &Path) {
        fs::create_dir(root.join("d1")).expect("mkdir d1");
        fs::create_dir(root.join("d2")).expect("mkdir d2");
        fs::write(root.join("root.txt"), b"root").expect("write");
        fs::write(root.join("z.txt"), b"z").expect("write");
        fs::write(root.join("d1").join("a.txt"), b"a").expect("write");
        fs::create_dir(root.join("d1").join("nested")).expect("mkdir nested");
        fs::write(root.join("d1").join("nested").join("deep.txt"), b"deep").expect("write");
        fs::write(root.join("d2").join("b.txt"), b"b").expect("write");
    }

    #[test]
    fn emits_files_in_lifo_enum_order_byte_equal_to_reference() {
        let tmp = tempdir().expect("tempdir");
        build_ordering_tree(tmp.path());

        let result = run(tmp.path(), &WalkOptions::default());
        assert!(
            result.errors.is_empty(),
            "unexpected errors: {:?}",
            result.errors
        );

        assert_eq!(
            paths_of(&result),
            reference_order(tmp.path()),
            "输出顺序必须与 TS concurrency=1 的 LIFO 规格逐字节一致"
        );
        // 不依赖顺序的守卫：集合、计数、字段。
        let mut got: Vec<String> = paths_of(&result);
        got.sort();
        let mut want = vec![
            "d1/a.txt".to_string(),
            "d1/nested/deep.txt".to_string(),
            "d2/b.txt".to_string(),
            "root.txt".to_string(),
            "z.txt".to_string(),
        ];
        want.sort();
        assert_eq!(got, want);
        assert_eq!(result.summary.files_scanned, 5);
        assert_eq!(
            result.summary.directories_scanned, 4,
            "root + d1 + nested + d2"
        );
        assert!(!result.summary.missing_root);
        assert!(!result.summary.cancelled);

        let root_entry = result
            .entries
            .iter()
            .find(|entry| entry.relative_path == "root.txt")
            .expect("root.txt");
        assert_eq!(root_entry.bytes, 4, "bytes = 文件实际字节数");
        assert!(
            root_entry.mtime_ms > 1_600_000_000_000,
            "mtimeMs 应是像样的 epoch 毫秒: {}",
            root_entry.mtime_ms
        );
    }

    #[test]
    fn depth_limit_and_non_recursive_stop_descent() {
        let tmp = tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join("a").join("b")).expect("mkdir");
        fs::write(root.join("f.txt"), b"1").expect("write");
        fs::write(root.join("a").join("f2.txt"), b"22").expect("write");
        fs::write(root.join("a").join("b").join("f3.txt"), b"333").expect("write");

        let depth0 = run(
            root,
            &WalkOptions {
                max_depth: Some(0),
                ..WalkOptions::default()
            },
        );
        assert_eq!(paths_of(&depth0), vec!["f.txt"], "maxDepth=0 只扫根目录");

        let depth1 = run(
            root,
            &WalkOptions {
                max_depth: Some(1),
                ..WalkOptions::default()
            },
        );
        assert_eq!(
            paths_of(&depth1),
            vec!["f.txt", "a/f2.txt"],
            "maxDepth=1 扫根 + 一级子目录"
        );
        assert_eq!(depth1.summary.directories_scanned, 2);

        let unlimited = run(root, &WalkOptions::default());
        assert_eq!(unlimited.entries.len(), 3, "默认无深度上限（同 TS）");

        let non_recursive = run(
            root,
            &WalkOptions {
                recursive: false,
                ..WalkOptions::default()
            },
        );
        assert_eq!(
            paths_of(&non_recursive),
            vec!["f.txt"],
            "recursive=false ≡ 只扫根"
        );

        // recursive=false 压过更大的 maxDepth（recursive 是总开关）。
        let both = run(
            root,
            &WalkOptions {
                recursive: false,
                max_depth: Some(9),
                follow_symlinks: false,
            },
        );
        assert_eq!(paths_of(&both), vec!["f.txt"]);
    }

    #[test]
    fn has_no_ignore_set_scans_dot_and_content_directories() {
        // TS fsWalker 没有任何忽略集：数据根下的 .git/node_modules 等一律入账。
        let tmp = tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join(".git")).expect("mkdir");
        fs::create_dir_all(root.join("node_modules").join("pkg")).expect("mkdir");
        fs::write(root.join(".git").join("HEAD"), b"ref").expect("write");
        fs::write(root.join("node_modules").join("pkg").join("index.js"), b"x").expect("write");
        fs::write(root.join(".hidden"), b"h").expect("write");

        let result = run(root, &WalkOptions::default());
        let mut got = paths_of(&result);
        got.sort();
        assert_eq!(
            got,
            vec![
                ".git/HEAD".to_string(),
                ".hidden".to_string(),
                "node_modules/pkg/index.js".to_string(),
            ],
            "无 ignore 集：点目录/内容目录必须被扫描（镜像 TS）"
        );
    }

    #[test]
    fn missing_root_is_empty_root_not_error() {
        let tmp = tempdir().expect("tempdir");
        let missing = tmp.path().join("does-not-exist");
        let result = run(&missing, &WalkOptions::default());
        assert!(
            result.summary.missing_root,
            "根不存在 → missingRoot=true（TS 语义）"
        );
        assert!(result.entries.is_empty());
        assert!(
            result.errors.is_empty(),
            "missingRoot 不是错误，不得产生 scan_error"
        );
        assert_eq!(result.summary.files_scanned, 0);
        assert_eq!(result.summary.directories_scanned, 0);
    }

    #[test]
    fn root_pointing_at_file_reports_enotdir_error_and_continues_empty() {
        let tmp = tempdir().expect("tempdir");
        let file_root = tmp.path().join("im-a-file.txt");
        fs::write(&file_root, b"x").expect("write");

        let result = run(&file_root, &WalkOptions::default());
        assert!(!result.summary.missing_root, "ENOTDIR ≠ ENOENT");
        assert_eq!(
            result.errors,
            vec![ScanError {
                path: String::new(),
                code: "ENOTDIR".to_string(),
            }],
            "根 read_dir 失败 → 容错上报（TS onError({{path:'',code}}））后空完成"
        );
        assert_eq!(result.summary.files_scanned, 0);
        assert_eq!(result.summary.directories_scanned, 0);
    }

    #[test]
    fn cancel_flag_stops_promptly_with_partial_counts() {
        let tmp = tempdir().expect("tempdir");
        let root = tmp.path();
        for dir in ["d1", "d2"] {
            fs::create_dir(root.join(dir)).expect("mkdir");
            for index in 0..5 {
                fs::write(root.join(dir).join(format!("f{index}.txt")), b"x").expect("write");
            }
        }

        let cancelled = AtomicBool::new(false);
        let mut seen = 0usize;
        let summary = walk_storage_root(
            root,
            &WalkOptions::default(),
            &cancelled,
            |entry| {
                seen += 1;
                assert!(
                    entry.relative_path.ends_with(".txt"),
                    "取消前只应产出文件条目"
                );
                if seen == 3 {
                    cancelled.store(true, Ordering::SeqCst);
                }
            },
            |_| panic!("容错路径不应触发错误回调"),
        );

        assert_eq!(seen, 3, "置位后不得再产出第 4 条");
        assert_eq!(summary.files_scanned, 3, "部分计数与实际产出一致");
        assert!(summary.cancelled, "观察到取消位 → cancelled=true");
        assert!(summary.directories_scanned >= 1);
        assert!(!summary.missing_root);
    }

    #[cfg(unix)]
    fn make_symlink(target: &Path, link: &Path, dir: bool) -> io::Result<()> {
        let _ = dir;
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn make_symlink(target: &Path, link: &Path, dir: bool) -> io::Result<()> {
        if dir {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        }
    }

    #[test]
    fn symlink_entries_skipped_by_default_and_followed_when_requested() {
        let tmp = tempdir().expect("tempdir");
        let root = tmp.path();
        fs::write(root.join("real.txt"), b"real").expect("write");
        let link = root.join("link.txt");
        if let Err(err) = make_symlink(&root.join("real.txt"), &link, false) {
            // Windows 无开发者模式/管理员权限时创建软链会失败：记录跳过，不伪造通过。
            eprintln!("skip symlink test, cannot create symlink on this machine: {err}");
            return;
        }

        let skipped = run(root, &WalkOptions::default());
        assert_eq!(
            paths_of(&skipped),
            vec!["real.txt"],
            "默认不跟软链（TS：isSymbolicLink → skip）"
        );

        let followed = run(
            root,
            &WalkOptions {
                follow_symlinks: true,
                ..WalkOptions::default()
            },
        );
        let mut got = paths_of(&followed);
        got.sort();
        assert_eq!(
            got,
            vec!["link.txt".to_string(), "real.txt".to_string()],
            "followSymlinks=true：软链文件按目标入账（sidecar 扩展）"
        );
    }

    #[test]
    fn followed_symlinked_directory_cycle_terminates() {
        let tmp = tempdir().expect("tempdir");
        let root = tmp.path();
        let sub = root.join("sub");
        fs::create_dir(&sub).expect("mkdir");
        fs::write(sub.join("inner.txt"), b"i").expect("write");
        if let Err(err) = make_symlink(root, &sub.join("loop"), true) {
            eprintln!("skip symlink cycle test, cannot create symlink: {err}");
            return;
        }

        let result = run(
            root,
            &WalkOptions {
                follow_symlinks: true,
                ..WalkOptions::default()
            },
        );
        let mut got = paths_of(&result);
        got.sort();
        assert_eq!(
            got,
            vec!["sub/inner.txt".to_string()],
            "目录环必须被 canonical 去重挡住，且真实文件恰好一条"
        );
        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
    }

    #[test]
    fn node_error_code_maps_platform_codes_and_falls_back_like_ts() {
        assert_eq!(
            node_error_code(&io::Error::from(io::ErrorKind::NotFound)),
            "ENOENT"
        );
        assert_eq!(
            node_error_code(&io::Error::from(io::ErrorKind::PermissionDenied)),
            "EACCES"
        );
        assert_eq!(
            node_error_code(&io::Error::from(io::ErrorKind::AlreadyExists)),
            "EEXIST"
        );
        // 原始码路径：Windows ERROR_DIRECTORY(267) / POSIX ENOTDIR(20) 都映射 ENOTDIR。
        let not_a_directory = io::Error::from_raw_os_error(if cfg!(windows) { 267 } else { 20 });
        assert_eq!(node_error_code(&not_a_directory), "ENOTDIR");
        // 未收录的原始码 → ErrorKind 兜底 → 与 TS errorCode() 一致的 UNKNOWN。
        assert_eq!(
            node_error_code(&io::Error::from_raw_os_error(999_999)),
            "UNKNOWN"
        );
    }

    #[test]
    fn to_relative_uses_forward_slashes_at_any_depth() {
        let root = Path::new("/data/root");
        assert_eq!(to_relative(root, &root.join("a").join("b.txt")), "a/b.txt");
        assert_eq!(to_relative(root, &root.join("top.txt")), "top.txt");
        assert_eq!(
            to_relative(root, root),
            "",
            "根自身 → 空串（同 TS relative）"
        );
    }
}
