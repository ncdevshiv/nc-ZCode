//! nc-code-image 可执行入口：`ready` → serve 循环 → 按关闭原因返回退出码。

use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(nc_code_image::run() as u8)
}
