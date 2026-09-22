// Tauri 构建期钩子：校验 tauri.conf.json 并嵌入 Windows 资源（图标/版本元数据）。
fn main() {
    tauri_build::build()
}
