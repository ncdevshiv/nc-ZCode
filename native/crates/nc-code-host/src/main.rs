//! zcode-host 二进制：stdio NDJSON RPC 服务进程。
//!
//! 与 TS host 的对接方式（specs/rust-native-crates.md S3 切缝）：外层 host 按
//! workspace flag 决定 spawn 哪个二进制；stdout 只写协议帧，stderr 只写诊断
//! （日志走 stderr，绝不污染协议管道）。

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use nc_code_host::headless::{HeadlessRenderer, Rendered};
use nc_code_host::provider_config::ProviderConfig;
use nc_code_host::HostServer;
use serde_json::json;
use tokio::sync::mpsc::UnboundedReceiver;

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
    /// Headless one-shot: run this prompt and print only the assistant answer to stdout.
    #[arg(long, value_name = "PROMPT")]
    prompt: Option<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let provider = match args.provider_config.as_deref() {
        Some(path) => match ProviderConfig::load(path) {
            Ok(config) => {
                // Secure handover: --provider-config is a one-shot secret file.
                // On Unix it must be owner-only (0600) and is unlinked after
                // consumption so credentials do not linger on disk. Never log
                // the secret or its path contents.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(meta) = std::fs::metadata(path) {
                        let mode = meta.permissions().mode() & 0o777;
                        if mode & 0o077 != 0 {
                            eprintln!(
                                "[zcode-host] provider config {} has group/other permissions ({:03o}); expected 0600",
                                path.display(),
                                mode
                            );
                        }
                    }
                }
                let _ = std::fs::remove_file(path);
                Some(config)
            }
            Err(error) => {
                eprintln!("[zcode-host] provider config rejected (fail-closed): {error}");
                None
            }
        },
        // Desktop handoff: the composition root passes the provider store files in the
        // spawn environment; without this fallback a default-flipped host starts
        // model-less even though the desktop already handed it a provider.
        None => match nc_code_host::provider_handoff::resolve_from_env() {
            Some(nc_code_host::provider_handoff::ProviderHandoff::Resolved(config)) => Some(config),
            Some(nc_code_host::provider_handoff::ProviderHandoff::Unavailable(reason)) => {
                eprintln!("[zcode-host] provider handoff unavailable (fail-closed): {reason}");
                None
            }
            None => None,
        },
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

    if let Some(prompt) = args.prompt.clone() {
        let code = run_headless(Arc::clone(&server), outbox, prompt).await;
        drop(server);
        std::process::exit(code);
    }

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
        // One task per protocol line: a long model request must not queue `stop` /
        // `cancelGenerateText` behind it. Per-session ordering is still the session
        // FIFO; responses and frames share the same outbox.
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            let (response, post_frames) = server.handle_line(&line).await;
            if let Some(response) = response {
                let _ = server.send_line(response);
            }
            for frame in post_frames {
                let _ = server.send_line(frame);
            }
        });
    }

    // stdin EOF → 读完即退：释放 server（outbox 最后一个发送端），writer 才能收尾。
    drop(server);
    let _ = writer.await;
}

/// Headless one-shot: create a session, submit `sendText`, render frames until the
/// turn reaches a terminal state. stdout carries assistant text only; diagnostics go
/// to stderr. Exit code is 0 only for `completedSuccess`.
async fn run_headless(
    server: Arc<HostServer>,
    mut outbox: UnboundedReceiver<String>,
    prompt: String,
) -> i32 {
    use std::io::Write;
    if !server.has_provider() {
        eprintln!(
            "[zcode-host] headless needs a model; pass --provider-config with a provider/model/apiKey"
        );
        return 1;
    }
    server.set_inline_responses(true);
    // Create the session through the same v4 command path the desktop uses; a
    // directly-constructed `Session` skips the inbox admission handshake and would
    // make `sendText` wait on a gate that never opens.
    let create = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "v4/command",
        "params": {
            "type": "createSession",
            "commandId": "headless-create",
            "clientId": "headless",
            "sessionId": null,
            "payload": {},
            "issuedAt": 1_700_000_000_000.0,
        },
    })
    .to_string();
    let _ = server.handle_line(&create).await;
    let Some(session_id) = server.session_ids().into_iter().next_back() else {
        eprintln!("[zcode-host] headless session creation returned no session");
        return 1;
    };
    if let Some(session) = server.load_session(&session_id) {
        // The frame projection only writes to subscribers; headless is a subscriber
        // without a socket so the same ordered stdout stream is produced.
        session.set_headless(true);
        session.add_subscriber(
            "headless".to_string(),
            nc_code_host::profile::DeliveryProfileName::Continuous,
        );
    }
    let envelope = json!({
        "type": "sendText",
        "commandId": "headless-1",
        "clientId": "headless",
        "sessionId": session_id,
        "payload": {"text": prompt},
        "issuedAt": 1_700_000_000_000.0,
    });
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "v4/command",
        "params": envelope,
    })
    .to_string();

    let mut renderer = HeadlessRenderer::new();
    let mut printed = String::new();
    let emit = |rendered: Vec<Rendered>, printed: &mut String| {
        for item in rendered {
            match item {
                Rendered::Text(text) => {
                    printed.push_str(&text);
                    print!("{text}");
                    let _ = std::io::stdout().flush();
                }
                Rendered::Notice(message) => eprintln!("[zcode-host] {message}"),
                Rendered::Done { .. } => {}
            }
        }
    };

    let (response, frames) = server.handle_line(&request).await;
    if let Some(response) = response {
        emit(renderer.consume_line(&response), &mut printed);
    }
    for frame in frames {
        emit(renderer.consume_line(&frame), &mut printed);
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    while renderer.terminal().is_none() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            eprintln!("[zcode-host] headless turn timed out");
            return 1;
        }
        match tokio::time::timeout(remaining, outbox.recv()).await {
            Ok(Some(line)) => emit(renderer.consume_line(&line), &mut printed),
            Ok(None) => break,
            Err(_) => {
                eprintln!("[zcode-host] headless turn timed out");
                return 1;
            }
        }
    }

    let (status, text) = renderer.terminal().cloned().unwrap_or_default();
    if printed.trim().is_empty() && !text.trim().is_empty() {
        print!("{text}");
        let _ = std::io::stdout().flush();
    }
    if status == "completedSuccess" {
        0
    } else {
        eprintln!("[zcode-host] headless turn ended with status {status}");
        1
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn basic_test() {
        assert!(true);
    }
}
