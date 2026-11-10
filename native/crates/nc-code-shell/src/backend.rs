//! Embedded Rust backend for the Tauri window.
//!
//! Binds `127.0.0.1:0` (ephemeral), serves the current `packages/web` build plus the
//! channel RPC endpoints, and navigates the main window to that origin. The Tauri
//! `frontendDist` scaffold is only the initial page; it is replaced by the real UI as
//! soon as the backend listener is up.

use std::path::PathBuf;
use std::sync::Arc;

use nc_code_server::{ServerOptions, WorkspaceInfo};
use nc_code_service_core::logger::ServiceLogger;
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
    // NC-AG runs in-process with the desktop shell (specs/nc-ag-orchestration.md §8).
    let ncag_runtime = match nc_code_orchestrator::NcagConfig::from_env() {
        Ok(config) => {
            nc_code_orchestrator::main_support::runtime_from_config(config, &backend.paths)
                .map(std::sync::Arc::new)
                .inspect_err(|error| {
                    ServiceLogger::new("nc-code-shell")
                        .warn(None, format!("NC-AG disabled: {}", error.message));
                })
                .ok()
        }
        Err(error) => {
            ServiceLogger::new("nc-code-shell")
                .warn(None, format!("NC-AG disabled: {}", error.message));
            None
        }
    };
    if let Some(runtime) = &ncag_runtime {
        // The Tauri setup hook runs on a plain thread, not inside a Tokio reactor, so a bare
        // tokio::spawn (runtime.spawn_background) panics with "there is no reactor running"
        // and aborted the whole desktop process at startup. Hop onto the shell's own runtime
        // through the explicit handle and await the loop body there.
        let runtime = std::sync::Arc::clone(runtime);
        let shutdown = nc_code_orchestrator::new_shutdown_flag();
        let stop_flag = std::sync::Arc::clone(&shutdown);
        tauri::async_runtime::spawn(async move {
            runtime.run_background(stop_flag).await;
        });
        // The loop runs for the whole app lifetime; the flag must outlive this scope.
        std::mem::forget(shutdown);
    }
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
        ncag: ncag_runtime,
    };
    let listener = bind_listener()?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("read listener address failed: {error}"))?;
    tauri::async_runtime::spawn(async move {
        if let Err(error) = nc_code_server::serve(listener, registry, options).await {
            ServiceLogger::new("nc-code-shell")
                .error(None, format!("backend server stopped: {error}"));
        }
    });
    let origin = format!("http://{address}/");
    if let Some(window) = app.get_webview_window("main") {
        let url = Url::parse(&origin).map_err(|error| format!("invalid backend url: {error}"))?;
        window
            .navigate(url)
            .map_err(|error| format!("navigate to backend failed: {error}"))?;
    }
    ServiceLogger::new("nc-code-shell").info(
        None,
        format!("backend listening at {origin} (channels={channels})"),
    );
    Ok(BackendHandle {
        origin,
        channels,
        static_dir,
    })
}

/// Default loopback port. A stable origin is required for WebView2 localStorage
/// (theme, provider-setup dismissal, UI preferences) to survive an app restart; the
/// previous ephemeral `:0` gave every launch a new origin and silently wiped it.
const DEFAULT_SHELL_PORT: u16 = 43117;

fn bind_listener() -> Result<TcpListener, String> {
    let preferred = std::env::var("ZCODE_SHELL_PORT")
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .unwrap_or(DEFAULT_SHELL_PORT);
    match tauri::async_runtime::block_on(TcpListener::bind(("127.0.0.1", preferred))) {
        Ok(listener) => Ok(listener),
        Err(error) => {
            // Another NC-Code instance may own the default port. Falling back keeps the
            // app usable, at the cost of a fresh storage origin for this run.
            ServiceLogger::new("nc-code-shell").warn(
                None,
                format!("bind {preferred} failed ({error}); falling back to an ephemeral port"),
            );
            tauri::async_runtime::block_on(TcpListener::bind(("127.0.0.1", 0)))
                .map_err(|fallback| format!("bind loopback listener failed: {fallback}"))
        }
    }
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
