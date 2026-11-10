//! `zcode-ncag` - standalone NC-AG service (axum API + SSE + background supervisor).
//!
//! ```sh
//! NCAG_DATA_DIR=.ncag zcode-ncag --host 127.0.0.1 --port 3040
//! ```
//!
//! The same router is mounted inside the ZCode server/shell; this binary exists so the
//! platform can be operated and verified without the desktop UI.

use std::path::PathBuf;

use clap::Parser;
use nc_code_orchestrator::config::NcagConfig;
use nc_code_orchestrator::main_support;
use nc_code_service_core::AppPaths;

#[derive(Parser, Debug)]
#[command(
    name = "zcode-ncag",
    about = "NC-AG autonomous multi-agent orchestration service"
)]
struct Args {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 3040)]
    port: u16,
    /// Print the effective configuration (including missing provider/model) and exit.
    #[arg(long)]
    check_config: bool,
    /// Workspace root override (git repository used for worktrees).
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Data directory for the SQLite store.
    #[arg(long)]
    data_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args = Args::parse();
    let mut config = NcagConfig::from_env().map_err(|error| {
        std::io::Error::other(format!("configuration error: {}", error.message))
    })?;
    if let Some(workspace) = args.workspace {
        config.workspace_root = workspace;
    }
    if let Some(dir) = args.data_dir {
        config.database_path = dir.join("ncag.sqlite");
        config.data_dir = dir;
    }
    if args.check_config {
        println!(
            "{}",
            serde_json::to_string_pretty(&main_support::describe(&config)).unwrap_or_default()
        );
        return Ok(());
    }
    let paths = AppPaths::from_env();
    let runtime = main_support::runtime_from_config(config, &paths)
        .map_err(|error| std::io::Error::other(format!("startup failed: {}", error.message)))?;
    let state = runtime.api_state();
    let router = nc_code_orchestrator::api::router(state);
    let shutdown = runtime.spawn_background();
    let listener = tokio::net::TcpListener::bind((args.host.as_str(), args.port)).await?;
    let address = listener.local_addr()?;
    // The e2e venue parses exactly this line (same contract as zcode-server).
    println!("NCAG LISTENING {address}");
    let result = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .await;
    result
}
