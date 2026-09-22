//! zcode-diff 可执行入口：`ready` → serve 循环 → 按关闭原因返回退出码。

use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(zcode_diff::run() as u8)
}
