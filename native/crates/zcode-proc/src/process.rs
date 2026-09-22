//! 进程树枚举与强杀 —— 镜像 TS `taskkill /T /F` + 抓取列表路径依赖的可观察结果
//! （`adapters/src/mcp/process-tree.ts`、`services/src/process/*`）：
//! 根 PID 已死/不存在 = 无操作成功；只有结束时仍存活的进程才算失败。

use std::collections::{HashMap, HashSet, VecDeque};

/// 快照/进程表中的一条进程记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    /// 可执行文件名（Windows: szExeFile；Linux: /proc stat comm；其余 Unix: 空串）。
    pub name: String,
}

/// 杀树结果里的一条失败记录（仅存活幸存者才进入 failed）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillFailure {
    pub pid: u32,
    pub error: String,
}

/// `kill_tree` 结果：killed 为确认已死的 PID（升序），failed 为结束时仍存活的幸存者。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillTreeResult {
    pub killed: Vec<u32>,
    pub failed: Vec<KillFailure>,
}

/// 轮询间隔（镜像 TS POSIX_PROCESS_TREE_POLL_MS=25ms）。
const POLL_INTERVAL_MS: u64 = 25;
/// 强杀后的确认窗口：超时后 force 幸存者，再给这段时间确认退出。
const FORCE_VERIFY_MS: u64 = 500;

/// 后代闭包（含根，根存在时）；顺序为根优先 BFS（深度升序、同深度 pid 升序）。
pub fn list_tree(root: u32) -> Vec<ProcessInfo> {
    // B-09 根因修复：快照成员资格不能当存活依据 —— Windows 先发布退出码、后从
    // 枚举表移除，高负载下该窗口可被拉长到 >50ms，已死根会短暂滞留在快照里。
    // 以 pid_alive（退出码探针，权威且先发布）为唯一存活判据；已死/不存在根按
    // 契约（README「根 PID 已死/不存在 → 空结果」）返回空。
    if !platform::pid_alive(root) {
        return Vec::new();
    }
    let snapshot = platform::snapshot();
    tree_closure(&snapshot, root)
}

/// 杀整棵树：循环轮询中按深度降序（叶子优先）逐个 kill，首轮杀完后对进程组补一刀
/// （unix killpg，windows 无操作），超时后 force 幸存者一次并确认 500ms；
/// 根已死（pid_alive 权威探针）或不在快照 → 空结果（镜像 TS 存活守卫的 no-op 成功）。
pub fn kill_tree(root: u32, timeout_ms: u64) -> KillTreeResult {
    // B-09 根因修复：入口存活判据原先只看快照成员资格，与「退出码先发布、枚举表
    // 后移除」的平台时序竞争 —— 已死根在高负载下滞留快照时会被二次「杀灭」并计入
    // killed（e2e 二轮 no-op 断言 1/185 复现：一轮 killed 含根、failed=[]，二轮
    // 又杀到同 pid）。改用 pid_alive 退出码探针做入口门（先于快照），快照只负责
    // 为存活根构建后代闭包；根已死/不存在 → 空结果 no-op（README 契约）。
    if !platform::pid_alive(root) {
        return KillTreeResult {
            killed: Vec::new(),
            failed: Vec::new(),
        };
    }
    let snapshot = platform::snapshot();
    let first = tree_closure(&snapshot, root);
    if first.is_empty() {
        // TS 契约：目标已死/不存在时 terminate 直接成功返回，不报错。
        return KillTreeResult {
            killed: Vec::new(),
            failed: Vec::new(),
        };
    }

    // 历轮见过的全部成员：根中途退出后仍要对早先发现的孤儿做 force 判定。
    let mut seen: HashMap<u32, ProcessInfo> =
        first.iter().map(|info| (info.pid, info.clone())).collect();
    let mut last_error: HashMap<u32, String> = HashMap::new();

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let mut leader_pgid: Option<u32> = None;
    let mut group_signalled = false;
    loop {
        let snapshot = platform::snapshot();
        let closure = tree_closure(&snapshot, root);
        if closure.is_empty() {
            break; // 根已退出
        }
        for info in &closure {
            seen.entry(info.pid).or_insert_with(|| info.clone());
        }

        // 首轮杀之前记录组长身份：进程死后 getpgid 可能失效，信号在本轮杀完后补发。
        if !group_signalled {
            leader_pgid = platform::leader_pgid(root);
        }

        // 叶子优先：深度降序；同深度按 pid 升序保证确定性。
        let depths = depth_map(&snapshot, root);
        let mut order = closure;
        order.sort_by(|a, b| {
            let da = depths.get(&a.pid).copied().unwrap_or(0);
            let db = depths.get(&b.pid).copied().unwrap_or(0);
            db.cmp(&da).then(a.pid.cmp(&b.pid))
        });
        for info in &order {
            match platform::terminate(info.pid) {
                Ok(()) => {
                    last_error.remove(&info.pid);
                }
                Err(err) => {
                    last_error.insert(info.pid, err);
                }
            }
        }
        if !group_signalled {
            if let Some(pgid) = leader_pgid {
                platform::kill_group(pgid);
            }
            group_signalled = true;
        }

        if order.iter().all(|info| !platform::pid_alive(info.pid)) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
    }

    // force：仍存活者再杀一次并确认。
    let mut survivors: Vec<ProcessInfo> = seen
        .values()
        .filter(|info| platform::pid_alive(info.pid))
        .cloned()
        .collect();
    survivors.sort_by_key(|info| info.pid);
    for info in &survivors {
        match platform::terminate(info.pid) {
            Ok(()) => {
                last_error.remove(&info.pid);
            }
            Err(err) => {
                last_error.insert(info.pid, err);
            }
        }
    }
    if !survivors.is_empty() {
        wait_until(&survivors, FORCE_VERIFY_MS);
    }

    let mut ordered: Vec<ProcessInfo> = seen.into_values().collect();
    ordered.sort_by_key(|info| info.pid);
    let mut killed: Vec<u32> = Vec::new();
    let mut failed: Vec<KillFailure> = Vec::new();
    for info in &ordered {
        if platform::pid_alive(info.pid) {
            // failed 只收结束时仍存活的幸存者；无系统错误时给出统一诊断文案。
            let error = last_error.remove(&info.pid).unwrap_or_else(|| {
                format!("still alive {FORCE_VERIFY_MS}ms after force terminate")
            });
            failed.push(KillFailure {
                pid: info.pid,
                error,
            });
        } else {
            killed.push(info.pid);
        }
    }
    KillTreeResult { killed, failed }
}

fn wait_until(targets: &[ProcessInfo], budget_ms: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(budget_ms);
    loop {
        if !targets.iter().any(|info| platform::pid_alive(info.pid)) {
            return;
        }
        if std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
    }
}

/// 从全量快照取根的后代闭包（含根）；根缺失 → 空。BFS 保证根优先、环安全。
fn tree_closure(snapshot: &[ProcessInfo], root: u32) -> Vec<ProcessInfo> {
    if !snapshot.iter().any(|info| info.pid == root) {
        return Vec::new();
    }
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for info in snapshot {
        children.entry(info.ppid).or_default().push(info.pid);
    }
    let by_pid: HashMap<u32, &ProcessInfo> = snapshot.iter().map(|info| (info.pid, info)).collect();

    let mut result: Vec<ProcessInfo> = Vec::new();
    let mut seen: HashSet<u32> = HashSet::new();
    let mut queue: VecDeque<u32> = VecDeque::new();
    queue.push_back(root);
    seen.insert(root);
    while let Some(pid) = queue.pop_front() {
        if let Some(info) = by_pid.get(&pid) {
            result.push((*info).clone());
        }
        if let Some(kids) = children.get(&pid) {
            let mut kids = kids.clone();
            kids.sort_unstable();
            for kid in kids {
                if seen.insert(kid) {
                    queue.push_back(kid);
                }
            }
        }
    }
    result
}

/// 每个 PID 相对根的深度（根=0）；环/越界父链兜底为 0。
fn depth_map(snapshot: &[ProcessInfo], root: u32) -> HashMap<u32, usize> {
    let by_pid: HashMap<u32, &ProcessInfo> = snapshot.iter().map(|info| (info.pid, info)).collect();
    let mut depths: HashMap<u32, usize> = HashMap::new();
    for info in snapshot {
        let mut depth = 0usize;
        let mut cursor = info.pid;
        let mut guard = 0usize;
        while cursor != root {
            let Some(parent) = by_pid.get(&cursor).map(|entry| entry.ppid) else {
                break;
            };
            if parent == cursor {
                break; // 自环保护
            }
            cursor = parent;
            depth += 1;
            guard += 1;
            if guard > snapshot.len() {
                depth = 0; // 环：放弃深度，仅保序
                break;
            }
        }
        depths.insert(info.pid, depth);
    }
    depths
}

#[cfg(windows)]
mod platform {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_TERMINATE,
    };

    use super::ProcessInfo;

    /// ERROR_INVALID_PARAMETER：OpenProcess 对不存在的 PID 返回此码。
    const ERROR_INVALID_PARAMETER: u32 = 87;
    /// STILL_ACTIVE：GetExitCodeProcess 报告进程仍在运行。
    const STILL_ACTIVE: u32 = 259;

    fn win32_code(err: &windows::core::Error) -> u32 {
        err.code().0 as u32 & 0xFFFF
    }

    /// Toolhelp 快照：失败按空表返回（list_tree 退化为空 → kill_tree no-op，不 panic）。
    pub fn snapshot() -> Vec<ProcessInfo> {
        let mut processes = Vec::new();
        let Ok(handle) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
            return processes;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        unsafe {
            if Process32FirstW(handle, &mut entry).is_ok() {
                loop {
                    processes.push(ProcessInfo {
                        pid: entry.th32ProcessID,
                        ppid: entry.th32ParentProcessID,
                        name: decode_wide(&entry.szExeFile),
                    });
                    if Process32NextW(handle, &mut entry).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(handle);
        }
        processes
    }

    fn decode_wide(raw: &[u16]) -> String {
        let end = raw.iter().position(|&unit| unit == 0).unwrap_or(raw.len());
        String::from_utf16_lossy(&raw[..end])
    }

    /// 存活判定：OpenProcess + GetExitCodeProcess != STILL_ACTIVE。
    /// 打不开时 ERROR_INVALID_PARAMETER（87，对象已释放）= 已死，其余（如拒绝访问）按存活保守处理。
    pub fn pid_alive(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
            Ok(handle) => handle,
            Err(err) => return win32_code(&err) != ERROR_INVALID_PARAMETER,
        };
        let mut code = 0u32;
        let alive = match unsafe { GetExitCodeProcess(handle, &mut code) } {
            Ok(()) => code == STILL_ACTIVE,
            // 查询失败按存活保守处理（进入 failed 而不是误报 killed）。
            Err(_) => true,
        };
        let _ = unsafe { CloseHandle(handle) };
        alive
    }

    /// TerminateProcess（PROCESS_TERMINATE）；“进程不存在”视为已死成功。
    pub fn terminate(pid: u32) -> Result<(), String> {
        if pid == 0 {
            return Err("invalid pid 0".to_string());
        }
        let handle = match unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) } {
            Ok(handle) => handle,
            Err(err) => {
                if win32_code(&err) == ERROR_INVALID_PARAMETER {
                    // 快照到发起之间已退出 —— 目标结果是死，不算失败。
                    return Ok(());
                }
                return Err(format!("open process failed: {err}"));
            }
        };
        let result = unsafe { TerminateProcess(handle, 1) };
        let _ = unsafe { CloseHandle(handle) };
        result.map_err(|err| format!("terminate failed: {err}"))
    }

    /// Windows 无进程组信号：Job Object 场景走 kill_job，树杀只按 pid 逐个 Terminate。
    pub fn leader_pgid(_pid: u32) -> Option<u32> {
        None
    }

    /// Windows 无操作（见 `leader_pgid`）。
    pub fn kill_group(_pgid: u32) {}
}

#[cfg(unix)]
mod platform {
    use super::ProcessInfo;

    /// Linux：遍历 /proc/<pid>/stat 取 (pid, ppid, comm)；快照失败按空表。
    #[cfg(target_os = "linux")]
    pub fn snapshot() -> Vec<ProcessInfo> {
        let mut processes = Vec::new();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return processes;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(pid_text) = name.to_str() else {
                continue;
            };
            let Ok(pid) = pid_text.parse::<u32>() else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            if let Some((ppid, comm)) = parse_stat(&stat) {
                processes.push(ProcessInfo {
                    pid,
                    ppid,
                    name: comm,
                });
            }
        }
        processes
    }

    /// 解析 `/proc/<pid>/stat`：`pid (comm) state ppid ...`；comm 可含空格与括号，
    /// 因此定位最后一个 `)` 之后的字段。
    #[cfg(target_os = "linux")]
    fn parse_stat(stat: &str) -> Option<(u32, String)> {
        let open = stat.find('(')?;
        let close = stat.rfind(')')?;
        let comm = stat[open + 1..close].to_string();
        let mut fields = stat[close + 1..].split_whitespace();
        fields.next()?; // state
        let ppid = fields.next()?.parse::<u32>().ok()?;
        Some((ppid, comm))
    }

    /// 非 Linux Unix：优雅降级为 `ps` 抓取 pid/ppid/comm。
    #[cfg(not(target_os = "linux"))]
    pub fn snapshot() -> Vec<ProcessInfo> {
        let args: [&str; 3] = if cfg!(target_os = "darwin") {
            ["-axo", "pid=,ppid=,comm=", ""]
        } else {
            ["-eo", "pid=,ppid=,comm=", ""]
        };
        let output = std::process::Command::new("ps")
            .args(args.iter().filter(|arg| !arg.is_empty()))
            .output();
        let Ok(output) = output else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&output.stdout);
        text.lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                let pid = parts.next()?.parse::<u32>().ok()?;
                let ppid = parts.next()?.parse::<u32>().ok()?;
                let name = parts.next().unwrap_or("").to_string();
                Some(ProcessInfo { pid, ppid, name })
            })
            .collect()
    }

    /// 存活判定镜像 TS：ESRCH = 已死；EPERM（存在但无权限）= 存活。
    pub fn pid_alive(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if rc == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    /// SIGKILL 单进程；ESRCH（已死）= 成功。
    pub fn terminate(pid: u32) -> Result<(), String> {
        if pid == 0 {
            return Err("invalid pid 0".to_string());
        }
        let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        if rc == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::ESRCH) => Ok(()),
            _ => Err(format!("kill failed: {err}")),
        }
    }

    /// 若 pid 是进程组组长则返回其 pgid（否则 None）；须在 pid 存活时采样。
    pub fn leader_pgid(pid: u32) -> Option<u32> {
        if pid == 0 {
            return None;
        }
        let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
        if pgid > 0 && pgid == pid as libc::pid_t {
            Some(pgid as u32)
        } else {
            None
        }
    }

    /// 对已知组发 SIGKILL：跳过自身所在组，避免树根恰为 sidecar 所在组组长时误杀自己。
    pub fn kill_group(pgid: u32) {
        if pgid == 0 {
            return;
        }
        let own = unsafe { libc::getpgid(0) };
        if own > 0 && own == pgid as libc::pid_t {
            return;
        }
        unsafe {
            libc::killpg(pgid as libc::pid_t, libc::SIGKILL);
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn list_tree_includes_self_and_excludes_self_parent() {
        let me = std::process::id();
        let tree = list_tree(me);
        assert!(tree.iter().any(|info| info.pid == me), "自身必须在闭包内");
        assert!(
            tree.iter().all(|info| info.ppid != me || info.pid != me),
            "环保护"
        );
    }

    #[test]
    fn missing_root_yields_empty_closure_and_noop_kill() {
        // 超出 Windows 22 位 PID 空间的值必然不存在。
        let ghost = 2_147_483_647;
        assert!(list_tree(ghost).is_empty());
        let result = kill_tree(ghost, 100);
        assert_eq!(result.killed, Vec::<u32>::new());
        assert_eq!(result.failed, Vec::<KillFailure>::new());
    }

    #[test]
    fn dead_root_kill_is_noop_success() {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit 0"])
            .spawn()
            .expect("spawn");
        let pid = child.id();
        child.wait().expect("wait");
        drop(child);
        let result = kill_tree(pid, 200);
        assert_eq!(result.killed, Vec::<u32>::new(), "已死根 = 空结果成功");
        assert!(result.failed.is_empty());
    }

    /// B-09 回归锁：根已退出（pid_alive 读到退出码）但进程对象仍被句柄持有
    /// （e2e 场景：测试 Child 未 wait）——即使条目仍可被枚举，kill_tree/list_tree
    /// 也必须按「已死根 = 空结果」契约返回 no-op，不得把已死根再杀一遍并计入
    /// killed。门走 pid_alive 权威探针，故本测试在修复后为确定性（不依赖
    /// 枚举滞留窗口是否恰好打开）；缺陷期的行为证据见 e2e 负载复现（日志 ENTRY 004）。
    #[test]
    fn dead_root_with_live_handle_is_noop_even_if_still_enumerable() {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "ping -n 300 127.0.0.1 >nul"])
            .spawn()
            .expect("spawn");
        let pid = child.id();
        child.kill().expect("terminate");
        // 退出码先于枚举移除发布：等权威探针确认已死（上界 2s，防环境异常挂死）。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while platform::pid_alive(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            !platform::pid_alive(pid),
            "退出码必须在 2s 内发布（探针失效则本测试无意义）"
        );
        // 句柄仍持有（未 wait/drop）：对象未销毁，最接近 e2e 二轮的缺陷条件。
        let result = kill_tree(pid, 200);
        assert_eq!(result.killed, Vec::<u32>::new(), "已死根 = 空结果 no-op");
        assert!(result.failed.is_empty(), "已死根不得进入 failed");
        assert!(
            list_tree(pid).is_empty(),
            "已死根 list_tree 必须空（不看枚举滞留）"
        );
        let _ = child.wait();
    }

    #[test]
    fn depth_order_kills_leaves_first() {
        let snapshot = vec![
            ProcessInfo {
                pid: 10,
                ppid: 1,
                name: "root.exe".into(),
            },
            ProcessInfo {
                pid: 20,
                ppid: 10,
                name: "mid.exe".into(),
            },
            ProcessInfo {
                pid: 30,
                ppid: 20,
                name: "leaf.exe".into(),
            },
            ProcessInfo {
                pid: 11,
                ppid: 1,
                name: "other.exe".into(),
            },
        ];
        let closure = tree_closure(&snapshot, 10);
        assert_eq!(
            closure.iter().map(|info| info.pid).collect::<Vec<_>>(),
            vec![10, 20, 30],
            "闭包必须排除无关进程且根优先"
        );
        let depths = depth_map(&snapshot, 10);
        assert_eq!(depths[&10], 0);
        assert_eq!(depths[&20], 1);
        assert_eq!(depths[&30], 2);
        // 树外节点沿父链走到缺失祖先即停（有条目即可，仅用于排序不参与闭包）。
        assert!(depths.contains_key(&11));
    }

    #[test]
    fn closure_survives_ppid_cycle() {
        let snapshot = vec![
            ProcessInfo {
                pid: 1,
                ppid: 2,
                name: "a".into(),
            },
            ProcessInfo {
                pid: 2,
                ppid: 1,
                name: "b".into(),
            },
        ];
        let closure = tree_closure(&snapshot, 1);
        assert_eq!(closure.len(), 2, "环内全部可达且不死循环");
        let _ = depth_map(&snapshot, 1);
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;

    #[test]
    fn list_tree_finds_self_on_this_platform() {
        let me = std::process::id();
        let tree = list_tree(me);
        assert!(tree.iter().any(|info| info.pid == me), "自身必须在闭包内");
    }

    #[test]
    fn missing_root_yields_empty_on_unix() {
        // 超出 Linux 默认 pid_max（4194304）的值必然不存在。
        assert!(list_tree(0).is_empty());
        let result = kill_tree(2_147_483_647, 50);
        assert!(result.killed.is_empty() && result.failed.is_empty());
    }

    #[test]
    fn kills_real_child_tree_root() {
        let mut child = std::process::Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        assert!(
            list_tree(pid).iter().any(|info| info.pid == pid),
            "新进程必须出现在快照"
        );
        let result = kill_tree(pid, 2_000);
        assert!(
            result.failed.is_empty(),
            "SIGKILL 后不应有幸存者: {:?}",
            result.failed
        );
        assert!(result.killed.contains(&pid), "killed 必须包含根 pid");
        assert!(list_tree(pid).is_empty(), "根必须已从进程表消失");
        let _ = child.wait(); // 回收僵尸
    }
}
