//! `--window-selftest`: headless interactive-window backend verification.
//!
//! The Tauri window itself needs a GUI session, so the headless gate verifies
//! the window's data source instead: resolve the static dir with the same
//! precedence as `backend.rs`, build the real channel registry via
//! `nc_code_app::build`, serve on an ephemeral loopback port, and fetch
//! `/` + `/api/server-info` over real HTTP. PASS proves the window can load
//! and boot against the embedded Rust backend without any TS runtime.

use std::path::PathBuf;
use std::sync::Arc;

use nc_code_service_core::AppPaths;

fn resolve_static_dir_for_selftest() -> PathBuf {
    if let Ok(value) = std::env::var("ZCODE_WEB_DIST") {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }
    if let Some(staged) = option_env!("ZCODE_WEB_DIST_STAGED") {
        let path = PathBuf::from(staged);
        if path.join("index.html").is_file() {
            return path;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../packages/web/dist")
}

pub fn run() -> i32 {
    let static_dir = resolve_static_dir_for_selftest();
    if !static_dir.join("index.html").is_file() {
        eprintln!(
            "[window-selftest] FAIL: web build not found at {} (run `vite build` in packages/web or set ZCODE_WEB_DIST)",
            static_dir.display()
        );
        return 1;
    }
    // Build the real composition root (same entry the window uses).
    let backend = match nc_code_app::build(AppPaths::from_env()) {
        Ok(backend) => backend,
        Err(error) => {
            eprintln!("[window-selftest] FAIL: channel registration failed: {error}");
            return 1;
        }
    };
    let channels = backend.registry.len();
    if channels == 0 {
        eprintln!("[window-selftest] FAIL: registry has no channels");
        return 1;
    }
    let registry = Arc::clone(&backend.registry);
    let options = nc_code_server::ServerOptions {
        static_dir: Some(static_dir.clone()),
        spa_fallback: true,
        server_id: "zcode-shell-selftest".to_string(),
        name: None,
        auth_token: None,
        workspaces: vec![nc_code_server::WorkspaceInfo::new(
            backend.paths.home_dir().to_string_lossy().to_string(),
        )],
        remote: Some(std::sync::Arc::clone(&backend.client.remotes)),
        ncag: None,
    };

    // Serve on an ephemeral port and probe over real HTTP (blocking client:
    // selftest is a one-shot binary, the server is on the tokio runtime).
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("[window-selftest] FAIL: runtime: {error}");
            return 1;
        }
    };
    let (origin, _handle) = runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind loopback");
        let address = listener.local_addr().expect("local addr");
        let handle = tokio::spawn(nc_code_server::serve(listener, registry, options));
        (format!("http://{address}/"), handle)
    });

    // Probe with reqwest (blocking is unnecessary: use a raw std TcpStream
    // GET so the selftest has no extra async dependency).
    let index_ok = http_get_contains(&origin, "<");
    let info_ok = http_get_contains(&origin, "workspaces");
    if index_ok && info_ok {
        eprintln!(
            "[window-selftest] PASS: origin={origin} channels={channels} static_dir={}",
            static_dir.display()
        );
        0
    } else {
        eprintln!("[window-selftest] FAIL: index_ok={index_ok} info_ok={info_ok} origin={origin}");
        1
    }
}

fn http_get_contains(origin: &str, needle: &str) -> bool {
    // origin is http://127.0.0.1:PORT/ â€” request "/" or "/api/server-info".
    let base = origin.trim_end_matches('/');
    let url = if needle == "workspaces" {
        format!("{base}/api/server-info")
    } else {
        format!("{base}/")
    };
    let Some(addr) = url.strip_prefix("http://") else {
        return false;
    };
    let (host_port, path) = match addr.find('/') {
        Some(idx) => (&addr[..idx], &addr[idx..]),
        None => (addr, "/"),
    };
    let mut stream = match std::net::TcpStream::connect(host_port) {
        Ok(stream) => stream,
        Err(_) => return false,
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
    let request = format!("GET {path} HTTP/1.0\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
    use std::io::{Read, Write};
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut body = String::new();
    if stream.read_to_string(&mut body).is_err() {
        return false;
    }
    body.contains(needle) && body.contains("200")
}

#[cfg(test)]
mod tests {
    #[test]
    fn selftest_resolves_a_static_dir_shape() {
        // The resolver mirrors backend.rs precedence; the real file check
        // runs in the --window-selftest binary.
        let dir = super::resolve_static_dir_for_selftest();
        assert!(!dir.as_os_str().is_empty());
    }
}
