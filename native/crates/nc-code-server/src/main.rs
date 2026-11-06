//! `nc-code-server` 可执行入口：解析 CLI、绑定 TCP、打印 `LISTENING <addr>` 供 e2e 解析。

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use nc_code_server::{serve, ServerOptions, WorkspaceInfo};
use nc_code_service_core::AppPaths;
use tokio::net::TcpListener;

/// ZCode axum server（S5）：静态 UI + 健康探测；host 接线为开放集成项。
#[derive(Parser, Debug)]
#[command(name = "zcode-server", about = "ZCode axum server (S5)")]
struct Args {
    /// 监听地址
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// 监听端口（0 = 由系统分配，实际地址打印到 stdout）
    #[arg(long, default_value_t = 0)]
    port: u16,

    /// 静态资源目录（未命中文件时 SPA fallback 到 index.html）
    #[arg(long)]
    static_dir: Option<PathBuf>,

    /// 初始工作区（广告给 /api/server-info；默认当前目录）
    #[arg(long)]
    workspace: Option<PathBuf>,

    /// 数据根目录（默认 AppPaths 的 home）
    #[arg(long)]
    data_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args = Args::parse();
    let workspace = args
        .workspace
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let mut paths = AppPaths::from_env();
    if let Some(data_dir) = args.data_dir.clone() {
        paths = paths.with_data_base_dir(data_dir);
    }
    let backend = nc_code_app::build(paths)
        .map_err(|error| std::io::Error::other(format!("channel registration failed: {error}")))?;
    let options = ServerOptions {
        static_dir: args.static_dir,
        spa_fallback: true,
        server_id: "zcode-server".to_string(),
        name: None,
        auth_token: None,
        workspaces: vec![WorkspaceInfo::new(workspace.to_string_lossy().to_string())],
        remote: Some(std::sync::Arc::clone(&backend.client.remotes)),
    };
    let listener = TcpListener::bind((args.host.as_str(), args.port)).await?;
    let addr: SocketAddr = listener.local_addr()?;
    // e2e 依赖这一行精确格式解析实际绑定地址（--port 0 时端口是动态的）。
    println!("LISTENING {addr}");
    serve(listener, backend.registry, options).await?;
    Ok(())
}

/// Ctrl-C 时优雅退出；信号注册失败则忽略（不影响服务）。
#[cfg(test)]
#[allow(dead_code)]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
