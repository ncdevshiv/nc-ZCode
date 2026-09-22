//! `zcode-server` 可执行入口：解析 CLI、绑定 TCP、打印 `LISTENING <addr>` 供 e2e 解析。

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use tokio::net::TcpListener;
use zcode_server::build_router;

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
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args = Args::parse();
    // S3/S5 集成开放：真实 host 尚未接线，默认 router 接进程级存活端口，
    // /healthz 如实报告「进程可服务、后端未接线」，不使用任何假 host。
    let router = build_router(args.static_dir);
    let listener = TcpListener::bind((args.host.as_str(), args.port)).await?;
    let addr: SocketAddr = listener.local_addr()?;
    // e2e 依赖这一行精确格式解析实际绑定地址（--port 0 时端口是动态的）。
    println!("LISTENING {addr}");
    axum::serve(listener, router.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Ctrl-C 时优雅退出；信号注册失败则忽略（不影响服务）。
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
