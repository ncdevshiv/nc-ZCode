//! zcode-host 二进制：stdio NDJSON RPC 服务进程。
//!
//! 与 TS host 的对接方式（specs/rust-native-crates.md S3 切缝）：外层 host 按
//! workspace flag 决定 spawn 哪个二进制；stdout 只写协议帧，stderr 只写诊断
//! （日志走 stderr，绝不污染协议管道）。

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio::sync::mpsc::UnboundedReceiver;
use zcode_host::provider_config::ProviderConfig;
use zcode_host::HostServer;

#[derive(Debug, Parser)]
#[command(name = "zcode-host", about = "ZCode native agent host (S3 increment)")]
struct Args {
    /// 工作区路径（身份隔离的 file-operation 根）。
    #[arg(long)]
    workspace: PathBuf,
    /// 会话数据目录（transcript 根）。
    #[arg(long)]
    data_dir: PathBuf,
    /// Provider 配置 JSON；缺省 = 模型层不可用（fail-closed）。
    #[arg(long)]
    provider_config: Option<PathBuf>,
    /// 允许 destructive 工具调用（write/edit/非只读 bash/外部 MCP 工具）；缺省 fail-closed。
    #[arg(long)]
    full_access: bool,
    /// MCP server（stdio）：`Name=command[:arg,arg]`，可重复。
    #[arg(long = "mcp-server", value_name = "SPEC")]
    mcp_servers: Vec<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let provider = match args.provider_config.as_deref() {
        Some(path) => match ProviderConfig::load(path) {
            Ok(config) => Some(config),
            Err(error) => {
                eprintln!("[zcode-host] provider config rejected (fail-closed): {error}");
                None
            }
        },
        None => None,
    };

    let _ = std::fs::create_dir_all(args.data_dir.join("sessions"));

    let (server, mut outbox): (Arc<HostServer>, UnboundedReceiver<String>) = HostServer::new(
        args.workspace.clone(),
        args.data_dir.clone(),
        provider,
        args.full_access,
        args.mcp_servers.clone(),
    )
    .unwrap_or_else(|error| {
        eprintln!("[zcode-host] startup failed: {error}");
        std::process::exit(1);
    });
    let resumed = server.resume_sessions();
    eprintln!(
        "[zcode-host] ready workspace={} data-dir={} resumed-sessions={} provider={}",
        args.workspace.display(),
        args.data_dir.display(),
        resumed,
        if server.has_provider() {
            "configured"
        } else {
            "unconfigured (fail-closed)"
        },
    );

    // stdout 写任务：所有响应与帧共用一个有序管道。stdout 锁不跨 await 持有
    // （平台实现非 Sync；每次写独立加锁，语义不变）。
    let writer = tokio::spawn(async move {
        while let Some(line) = outbox.recv().await {
            let mut stdout = std::io::stdout();
            if stdout.write_all(line.as_bytes()).is_err() || stdout.write_all(b"\n").is_err() {
                break;
            }
            let _ = stdout.flush();
        }
    });

    // stdin 读任务：阻塞读取放 blocking 池，行经 channel 进异步分发。
    let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::task::spawn_blocking(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(line) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    if line_tx.send(line).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    eprintln!("[zcode-host] stdin read error: {error}");
                    break;
                }
            }
        }
    });

    while let Some(line) = line_rx.recv().await {
        let (response, post_frames) = server.handle_line(&line).await;
        if let Some(response) = response {
            if server.send_line(response).is_err() {
                break;
            }
        }
        for frame in post_frames {
            if server.send_line(frame).is_err() {
                break;
            }
        }
    }

    // stdin EOF → 读完即退：释放 server（outbox 最后一个发送端），writer 才能收尾。
    drop(server);
    let _ = writer.await;
}
