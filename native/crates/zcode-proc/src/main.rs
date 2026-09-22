//! zcode-proc 可执行入口：`ready` → serve 循环 → 按关闭原因返回退出码。
//!
//! `run()` 在返回前已完成 Job 句柄清理，因此这里以正常返回路径退出
//! （`ExitCode`），不走 `process::exit`（会跳过 TLS 析构）。

use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(zcode_proc::run() as u8)
}
