//! Embedded Rust backend for the Tauri window.
//!
//! Binds `127.0.0.1:0` (ephemeral), serves the current `packages/web` build plus the
//! channel RPC endpoints, and navigates the main window to that origin. The Tauri
//! `frontendDist` scaffold is only the initial page; it is replaced by the real UI as
//! soon as the backend listener is up.

use std::path::PathBuf;
use std::sync::Arc;

use nc_code_server::{ServerOptions, WorkspaceInfo};
use nc_code_service_core::AppPaths;
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Manager, Url};
use tokio::net::TcpListener;

pub struct BackendHandle {
    pub origin: String,
    pub channels: usize,
    pub static_dir: PathBuf,
}

#[allow(dead_code)]
impl BackendHandle {
    pub fn origin(&self) -> &str {
        &self.origin
    }
    pub fn channels(&self) -> usize {
        self.channels
    }
    pub fn static_dir(&self) -> &std::path::Path {
        &self.static_dir
    }
}

/// Start the embedded server and navigate the main window to it.
///
/// The channel registry is composed by `nc-code-app::build`; this function only binds the
/// loopback listener and points the window at it.
pub fn start(app: &tauri::App) -> Result<BackendHandle, String> {
    let static_dir = resolve_static_dir(app.handle());
    if !static_dir.join("index.html").is_file() {
        return Err(format!(
            "web frontend build not found at {} (run `vite build` in packages/web, set ZCODE_WEB_DIST, or rebuild the shell so build.rs stages the dist)",
            static_dir.display()
        ));
    }
    let backend = {
        // The agent's `Browser` tool drives the same child webview the renderer
        // shows; without a port the tool fails closed (`browser_unavailable`).
        let registry = app
            .state::<crate::browser_view::BrowserViewRegistry>()
            .inner()
            .clone();
        let bridge = Arc::new(crate::browser_use_bridge::BrowserUseBridgeState::default());
        app.manage(Arc::clone(&bridge));
        let browser_port: Option<Arc<dyn nc_code_host::tools::browser::BrowserControlPort>> = Some(
            Arc::new(crate::browser_control::ShellBrowserPort::with_lifecycle(
                registry,
                app.handle().clone(),
                bridge,
            )),
        );
        let computer_port: Option<Arc<dyn nc_code_host::tools::computer::ComputerControlPort>> =
            Some(Arc::new(crate::computer_control::ShellComputerControl));
        nc_code_app::build_with_ports(AppPaths::from_env(), browser_port, computer_port)
            .map_err(|error| format!("channel registration failed: {error}"))?
    };
    let registry = Arc::clone(&backend.registry);
    // The automation scheduling loop shares the same implementation as the server entry point (20s polling, claim -> dispatch -> settle).
    let automation_agent = Arc::clone(&backend.agent.service);
    let _automation_scheduler = tauri::async_runtime::spawn(async move {
        nc_code_app::run_automation_scheduler(automation_agent).await
    });
    let channels = registry.len();
    let options = ServerOptions {
        static_dir: Some(static_dir.clone()),
        spa_fallback: true,
        server_id: "zcode-shell".to_string(),
        name: None,
        auth_token: None,
        workspaces: vec![WorkspaceInfo::new(
            backend.paths.home_dir().to_string_lossy().to_string(),
        )],
        remote: Some(Arc::clone(&backend.client.remotes)),
    };
    let listener = tauri::async_runtime::block_on(TcpListener::bind(("127.0.0.1", 0)))
        .map_err(|error| format!("bind loopback listener failed: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("read listener address failed: {error}"))?;
    tauri::async_runtime::spawn(async move {
        if let Err(error) = nc_code_server::serve(listener, registry, options).await {
            eprintln!("[nc-code-shell] backend server stopped: {error}");
        }
    });
    let origin = format!("http://{address}/");
    if let Some(window) = app.get_webview_window("main") {
        let url = Url::parse(&origin).map_err(|error| format!("invalid backend url: {error}"))?;
        window
            .navigate(url)
            .map_err(|error| format!("navigate to backend failed: {error}"))?;
    }
    eprintln!("[nc-code-shell] backend listening at {origin} (channels={channels})");
    Ok(BackendHandle {
        origin,
        channels,
        static_dir,
    })
}

fn resolve_static_dir(app: &AppHandle) -> PathBuf {
    if let Ok(value) = std::env::var("ZCODE_WEB_DIST") {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }
    // Build-staged dist (dev `cargo run`): build.rs copies packages/web/dist
    // into $OUT_DIR/web and bakes the path at compile time. The packaged app
    // takes the resource branch below instead (this path is a build-machine
    // absolute path and does not travel with the installer).
    if let Some(staged) = option_env!("ZCODE_WEB_DIST_STAGED") {
        let path = PathBuf::from(staged);
        if path.join("index.html").is_file() {
            return path;
        }
    }
    if let Ok(resource) = app.path().resolve("web", BaseDirectory::Resource) {
        if resource.join("index.html").is_file() {
            return resource;
        }
    }
    // String-form resources mirror the source path (`packages/web/dist`):
    // check the mirrored tree as well so both shapes resolve.
    if let Ok(resource) = app
        .path()
        .resolve("packages/web/dist", BaseDirectory::Resource)
    {
        if resource.join("index.html").is_file() {
            return resource;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../packages/web/dist")
}
