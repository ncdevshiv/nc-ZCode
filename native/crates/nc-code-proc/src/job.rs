//! Job 作用域 spawn —— Windows Job Object（KILL_ON_JOB_CLOSE）/ unix setsid + PDEATHSIG，
//! 镜像 TS `adapters/src/mcp/windows-job-object.ts` 的可观察语义：
//! 子树随 sidecar 退出而终止；入 Job 失败时子进程仍在运行，交由调用方 `kill_tree` 回退。

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// spawn 阶段错误：
/// - `Setup`：创建/参数阶段失败，**未产生子进程** → 400；
/// - `Assign`：子进程已创建但无法纳入作用域，pid 仍存活 → 409 + `kill_tree` 回退。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnError {
    /// 创建/参数阶段失败（未产生子进程）。
    Setup(String),
    /// 子进程已创建但入 Job 失败；`pid` 仍在运行。
    /// 仅 Windows 可产生（unix 无“入 Job”阶段），unix 构建允许未构造。
    #[cfg_attr(not(windows), allow(dead_code))]
    Assign { pid: u32, error: String },
}

/// jobKey 序号：`job-{seq}-{pid}`，进程内单调递增。
static SEQ: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// 当前 sidecar 持有的 Job 句柄（serve 循环在主线程运行，句柄归主线程 TLS 所有）。
    static JOBS: RefCell<HashMap<String, JobHandle>> = RefCell::new(HashMap::new());
}

/// Job 作用域句柄：Windows 为 Job Object 句柄（关闭即杀子树）；
/// unix 为 setsid 组长 pid（terminate 走 killpg，sidecar 死亡由 PDEATHSIG 兜底）。
pub struct JobHandle {
    #[cfg(windows)]
    job: windows::Win32::Foundation::HANDLE,
    #[cfg(unix)]
    pid: u32,
}

#[cfg(windows)]
impl JobHandle {
    /// 终止整棵子树（TerminateJobObject，退出码 1）。
    fn terminate(&self) {
        unsafe {
            let _ = windows::Win32::System::JobObjects::TerminateJobObject(self.job, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for JobHandle {
    fn drop(&mut self) {
        // 关句柄触发 JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE：sidecar 退出即清理子树。
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.job);
        }
    }
}

#[cfg(unix)]
impl JobHandle {
    /// 终止整棵子树（对 setsid 组长发 SIGKILL → killpg 覆盖全组）。
    fn terminate(&self) {
        unsafe {
            // ESRCH（组已消失）忽略：目标结果就是“已死”。
            libc::killpg(self.pid as libc::pid_t, libc::SIGKILL);
        }
    }
}

// unix 无 Drop：sidecar 退出时 pre_exec 设置的 PDEATHSIG(SIGKILL) 终止子进程，
// 与 Windows 关句柄杀子树的可观察语义一致。

/// 生成下一个 jobKey。
pub fn next_key(pid: u32) -> String {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("job-{seq}-{pid}")
}

/// 登记句柄（覆盖同名 key 不会发生：key 含自增 seq 与 pid）。
pub fn store(key: String, handle: JobHandle) {
    JOBS.with(|jobs| jobs.borrow_mut().insert(key, handle));
}

/// 移除并终止该 job；返回是否曾持有（首次 true，重复/未知 key false）。
pub fn terminate(key: &str) -> bool {
    JOBS.with(|jobs| match jobs.borrow_mut().remove(key) {
        Some(handle) => {
            handle.terminate();
            drop(handle); // 再关一次句柄兜底（Windows 下幂等）
            true
        }
        None => false,
    })
}

/// 关闭全部句柄：退出路径上确定性触发 KILL_ON_JOB_CLOSE（与进程退出收尾一致）。
pub fn clear_all() {
    JOBS.with(|jobs| jobs.borrow_mut().clear());
}

/// 在 Job 作用域内启动进程：Windows = Job Object，unix = 新会话 + 父亡信号。
pub fn spawn(
    cmd: &str,
    args: &[String],
    cwd: Option<&str>,
    env: Option<&HashMap<String, String>>,
) -> Result<(u32, JobHandle), SpawnError> {
    platform::spawn(cmd, args, cwd, env)
}

#[cfg(windows)]
mod platform {
    use std::collections::HashMap;
    use std::ffi::c_void;

    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Threading::{
        CreateProcessW, OpenProcess, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
        PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        STARTUPINFOW,
    };

    use super::{JobHandle, SpawnError};

    /// 按 Windows 命令行解析规则给单个 token 加引号（反斜杠-引号算法）：
    /// 引号前的反斜杠翻倍、内容引号反斜杠转义、闭引号前的尾部反斜杠翻倍。
    fn quote_arg(arg: &str) -> String {
        if !arg.is_empty() && !arg.chars().any(|c| matches!(c, ' ' | '\t' | '"')) {
            return arg.to_string();
        }
        let mut out = String::from('"');
        let mut slashes = 0usize;
        for ch in arg.chars() {
            match ch {
                '\\' => slashes += 1,
                '"' => {
                    out.push_str(&"\\".repeat(slashes * 2 + 1));
                    slashes = 0;
                    out.push('"');
                }
                _ => {
                    out.push_str(&"\\".repeat(slashes));
                    slashes = 0;
                    out.push(ch);
                }
            }
        }
        out.push_str(&"\\".repeat(slashes * 2)); // 尾部反斜杠翻倍，避免吞掉闭引号
        out.push('"');
        out
    }

    /// 拼接完整命令行（lpApplicationName 置空 → CreateProcess 按首个 token 搜 PATH）。
    fn quote_command(cmd: &str, args: &[String]) -> String {
        let mut line = quote_arg(cmd);
        for arg in args {
            line.push(' ');
            line.push_str(&quote_arg(arg));
        }
        line
    }

    pub fn spawn(
        cmd: &str,
        args: &[String],
        cwd: Option<&str>,
        env: Option<&HashMap<String, String>>,
    ) -> Result<(u32, JobHandle), SpawnError> {
        // 1) 匿名 Job + 关句柄即杀子树（镜像 TS JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE）。
        let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
            .map_err(|err| SpawnError::Setup(format!("create job object failed: {err}")))?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if let Err(err) = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } {
            let _ = unsafe { CloseHandle(job) };
            return Err(SpawnError::Setup(format!("set job limits failed: {err}")));
        }

        // 2) 命令行需要可变 NUL 结尾缓冲；应用名置空走 PATH 搜索（镜像 TS 传法）。
        let mut cmdline: Vec<u16> = quote_command(cmd, args).encode_utf16().collect();
        cmdline.push(0);
        let cwd_wide: Option<Vec<u16>> = cwd.map(|text| {
            let mut wide: Vec<u16> = text.encode_utf16().collect();
            wide.push(0);
            wide
        });
        // 提供 env 时整块替换进程环境（CreateProcess 语义，镜像 TS env 直传）。
        let env_block: Option<Vec<u16>> = env.map(build_env_block);

        let mut flags = CREATE_NO_WINDOW;
        if env_block.is_some() {
            flags = PROCESS_CREATION_FLAGS(flags.0 | CREATE_UNICODE_ENVIRONMENT.0);
        }
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut info = PROCESS_INFORMATION::default();
        let created = unsafe {
            CreateProcessW(
                PCWSTR::null(),
                Some(PWSTR(cmdline.as_mut_ptr())),
                None,
                None,
                false,
                flags,
                env_block
                    .as_ref()
                    .map(|block| block.as_ptr() as *const c_void),
                cwd_wide
                    .as_ref()
                    .map_or(PCWSTR::null(), |wide| PCWSTR(wide.as_ptr())),
                &startup,
                &mut info,
            )
        };
        if let Err(err) = created {
            // 尚未产生子进程 → 400；Job 句柄关闭即可（空 Job 无子树）。
            let _ = unsafe { CloseHandle(job) };
            return Err(SpawnError::Setup(format!("create process failed: {err}")));
        }

        // 3) 纳入 Job：失败时子进程已启动 —— 不杀，交上层 409 + kill_tree 回退。
        let pid = info.dwProcessId;
        let assigned =
            match unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid) } {
                Ok(handle) => {
                    let result = unsafe { AssignProcessToJobObject(job, handle) };
                    let _ = unsafe { CloseHandle(handle) };
                    result
                }
                Err(err) => Err(err),
            };
        unsafe {
            let _ = CloseHandle(info.hThread);
            let _ = CloseHandle(info.hProcess);
        }
        match assigned {
            Ok(()) => Ok((pid, JobHandle { job })),
            Err(err) => {
                let _ = unsafe { CloseHandle(job) };
                Err(SpawnError::Assign {
                    pid,
                    error: format!("assign process to job failed: {err}"),
                })
            }
        }
    }

    /// Windows 环境块：每项 `KEY=VALUE\0`，结尾额外 NUL；空块也必须双 NUL（UTF-16）。
    fn build_env_block(env: &HashMap<String, String>) -> Vec<u16> {
        let mut block: Vec<u16> = Vec::new();
        for (key, value) in env {
            block.extend(format!("{key}={value}").encode_utf16());
            block.push(0);
        }
        block.push(0);
        if block.len() == 1 {
            block.push(0); // 空 map：[0] → [0, 0]
        }
        block
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn env_block_is_double_nul_terminated_including_empty() {
            let mut env = HashMap::new();
            env.insert("A".to_string(), "1".to_string());
            let block = build_env_block(&env);
            assert_eq!(&block[..3], &[b'A' as u16, b'=' as u16, b'1' as u16]);
            assert_eq!(&block[3..], &[0, 0], "条目结束 + 块结束双 NUL");

            let empty = build_env_block(&HashMap::new());
            assert_eq!(empty, vec![0, 0], "空环境块同样双 NUL");
        }

        #[test]
        fn quote_arg_follows_windows_parsing_rules() {
            assert_eq!(quote_arg("plain"), "plain");
            assert_eq!(quote_arg(""), r#""""#);
            assert_eq!(quote_arg("has space"), r#""has space""#);
            assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
            // 无空白/引号 → 不加引号，尾部反斜杠原样（无闭引号可吞）。
            assert_eq!(quote_arg(r"C:\dir\"), r"C:\dir\");
            // 需要引号时尾部反斜杠翻倍，避免吞掉闭引号。
            assert_eq!(quote_arg(r"C:\dir x\"), r#""C:\dir x\\""#);
            assert_eq!(quote_arg(r#"a\"b"#), r#""a\\\"b""#);
        }

        #[test]
        fn quote_command_joins_with_spaces() {
            let args = vec!["/C".to_string(), "echo hi".to_string()];
            assert_eq!(quote_command("cmd", &args), r#"cmd /C "echo hi""#);
            assert_eq!(quote_command("ping", &[]), "ping");
        }
    }
}

#[cfg(unix)]
mod platform {
    use std::collections::HashMap;
    use std::io;
    use std::os::unix::process::CommandExt;

    use super::{JobHandle, SpawnError};

    pub fn spawn(
        cmd: &str,
        args: &[String],
        cwd: Option<&str>,
        env: Option<&HashMap<String, String>>,
    ) -> Result<(u32, JobHandle), SpawnError> {
        let mut command = std::process::Command::new(cmd);
        command.args(args);
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        if let Some(vars) = env {
            command.env_clear();
            for (key, value) in vars {
                command.env(key, value);
            }
        }
        unsafe {
            // 外层 unsafe 已覆盖闭包体（词法作用域继承），libc 调用无需再包一层。
            command.pre_exec(|| {
                // 新会话新组长：kill_job 走 killpg 覆盖整棵后代组。
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                #[cfg(target_os = "linux")]
                {
                    // 父进程（sidecar）退出即 SIGKILL 子进程，对齐 KILL_ON_JOB_CLOSE。
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::getppid() == 1 {
                        // 竞态兜底：fork 与 prctl 之间 sidecar 已退出。
                        libc::raise(libc::SIGKILL);
                    }
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .map_err(|err| SpawnError::Setup(format!("spawn failed: {err}")))?;
        let pid = child.id();
        // 独立线程回收子进程，避免被杀后留僵尸；终止语义仍归 JobHandle（killpg）。
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok((pid, JobHandle { pid }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_keys_are_unique_across_spawns() {
        let first = next_key(123);
        let second = next_key(123);
        assert_ne!(first, second, "同 pid 不同 seq 必须区分");
        assert!(first.starts_with("job-") && first.ends_with("-123"));
    }

    #[test]
    fn terminate_unknown_key_is_false_and_idempotent() {
        assert!(!terminate("job-0-0"), "未知 key → false");
    }
}
