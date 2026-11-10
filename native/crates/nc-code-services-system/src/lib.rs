//! `system` channel — system info, integrated terminal shells, intranet probe.
//!
//! Port of `packages/services/src/system/{system,systemService}.ts` (wired channel).
//! - `info`: real homedir + platform.
//! - `listIntegratedTerminalShells`: real executable detection (Windows + common Unix paths).
//! - `probeIntranet`: TCP-connect and HTTP service probes with retries/normalization
//!   matching TS semantics. HTTP service probes use `tokio` (no extra HTTP client dep):
//!   `http://` targets are probed via real GET + JSON `{ok, marker?}` validation
//!   (`x-zcode-intranet-token`, `expectedMarker`). `https://` targets fail honestly
//!   (TLS not implemented in this build) rather than fabricating reachability.

/// Handoff skeleton (pure data/types): serde persistence structs, port trait, Send/Sync markers.
pub mod handoff;

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::{ServiceArgs, ServiceError};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream as TokioTcpStream;

const DEFAULT_PROBE_TIMEOUT_MS: u64 = 800;
const DEFAULT_PROBE_ATTEMPTS: u64 = 2;
const MAX_PROBE_ATTEMPTS: u64 = 3;
const DEFAULT_PROBE_PORT: u16 = 22;
const MAX_HTTP_BODY_BYTES: usize = 64 * 1024;

pub struct SystemService;

impl SystemService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }

    fn homedir() -> String {
        std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".to_string())
    }

    fn info(&self) -> Result<WireValue, ServiceError> {
        Ok(WireValue::Json(json!({
            "homedir": Self::homedir(),
            "platform": std::env::consts::OS,
        })))
    }

    fn is_executable(path: &Path) -> bool {
        if !path.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(path) {
                return meta.permissions().mode() & 0o111 != 0;
            }
            false
        }
        #[cfg(not(unix))]
        {
            true
        }
    }

    fn find_in_path(names: &[&str]) -> Option<PathBuf> {
        let path_var = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path_var) {
            for name in names {
                let p = dir.join(name);
                if Self::is_executable(&p) {
                    return Some(p);
                }
            }
        }
        None
    }

    fn list_integrated_terminal_shells(&self) -> Result<WireValue, ServiceError> {
        let mut shells: Vec<Value> = Vec::new();
        let is_windows = cfg!(windows) || std::env::consts::OS == "windows";

        if is_windows {
            if let Some(p) = Self::find_in_path(&["pwsh.exe"]) {
                shells.push(json!({
                    "id": "pwsh",
                    "label": "PowerShell 7",
                    "path": p.to_string_lossy(),
                    "args": [],
                    "source": "detected",
                }));
            }
            let win_ps = std::env::var("SystemRoot")
                .map(|r| PathBuf::from(r).join("System32\\WindowsPowerShell\\v1.0\\powershell.exe"))
                .ok();
            if let Some(p) = win_ps.filter(|p| p.is_file()) {
                shells.push(json!({
                    "id": "powershell",
                    "label": "Windows PowerShell",
                    "path": p.to_string_lossy(),
                    "args": [],
                    "source": "detected",
                }));
            } else if let Some(p) = Self::find_in_path(&["powershell.exe"]) {
                shells.push(json!({
                    "id": "powershell",
                    "label": "Windows PowerShell",
                    "path": p.to_string_lossy(),
                    "args": [],
                    "source": "detected",
                }));
            }
            let cmd = std::env::var("ComSpec")
                .map(PathBuf::from)
                .ok()
                .filter(|p| p.is_file())
                .or_else(|| {
                    std::env::var("SystemRoot")
                        .ok()
                        .map(|r| PathBuf::from(r).join("System32\\cmd.exe"))
                        .filter(|p| p.is_file())
                });
            if let Some(p) = cmd {
                shells.push(json!({
                    "id": "cmd",
                    "label": "Command Prompt",
                    "path": p.to_string_lossy(),
                    "args": [],
                    "source": "detected",
                }));
            }
            for (id, label, candidates) in [
                (
                    "git-bash",
                    "Git Bash",
                    vec![
                        PathBuf::from(r"C:\Program Files\Git\bin\bash.exe"),
                        PathBuf::from(r"C:\Program Files (x86)\Git\bin\bash.exe"),
                    ],
                ),
                (
                    "wsl",
                    "WSL",
                    vec![std::env::var("SystemRoot")
                        .map(|r| PathBuf::from(r).join("System32\\wsl.exe"))
                        .unwrap_or_else(|_| PathBuf::from(r"C:\Windows\System32\wsl.exe"))],
                ),
            ] {
                if let Some(p) = candidates.into_iter().find(|p| p.is_file()) {
                    shells.push(json!({
                        "id": id,
                        "label": label,
                        "path": p.to_string_lossy(),
                        "args": [],
                        "source": "detected",
                    }));
                }
            }
        } else {
            for (id, label, names) in [
                ("bash", "Bash", &["bash"][..]),
                ("zsh", "Zsh", &["zsh"][..]),
                ("fish", "Fish", &["fish"][..]),
                ("sh", "sh", &["sh"][..]),
            ] {
                if let Some(p) = Self::find_in_path(names) {
                    shells.push(json!({
                        "id": id,
                        "label": label,
                        "path": p.to_string_lossy(),
                        "args": [],
                        "source": "detected",
                    }));
                }
            }
        }

        Ok(WireValue::Json(json!(shells)))
    }

    fn normalize_attempts(attempts: Option<u64>) -> u64 {
        match attempts {
            Some(n) => n.clamp(1, MAX_PROBE_ATTEMPTS),
            None => DEFAULT_PROBE_ATTEMPTS,
        }
    }

    fn normalize_required_success_count(required: Option<u64>, total: usize) -> u64 {
        if total == 0 {
            return 1;
        }
        match required {
            Some(n) if n >= 1 => n.min(total as u64),
            _ => 1,
        }
    }

    fn normalize_timeout(timeout_ms: Option<u64>) -> u64 {
        match timeout_ms {
            Some(n) => n.clamp(100, 10_000),
            None => DEFAULT_PROBE_TIMEOUT_MS,
        }
    }

    fn normalize_tcp_target(target: &Value) -> Option<Value> {
        let host = target
            .get("host")
            .and_then(|v| v.as_str())?
            .trim()
            .to_string();
        if host.is_empty() {
            return None;
        }
        let port = target
            .get("port")
            .and_then(|v| v.as_u64())
            .filter(|p| *p >= 1 && *p <= 65535)
            .map(|p| p as u16)
            .unwrap_or(DEFAULT_PROBE_PORT);
        let timeout_ms = Self::normalize_timeout(target.get("timeoutMs").and_then(|v| v.as_u64()));
        let target_id = target
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("{host}:{port}"));
        Some(json!({
            "kind": "tcp",
            "targetId": target_id,
            "host": host,
            "port": port,
            "timeoutMs": timeout_ms,
        }))
    }

    fn normalize_service_target(target: &Value) -> Option<Value> {
        let url = target
            .get("url")
            .and_then(|v| v.as_str())?
            .trim()
            .to_string();
        if url.is_empty() {
            return None;
        }
        let lower = url.to_ascii_lowercase();
        if !lower.starts_with("http://") && !lower.starts_with("https://") {
            return None;
        }
        let timeout_ms = Self::normalize_timeout(target.get("timeoutMs").and_then(|v| v.as_u64()));
        let target_id = target
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| url.clone());
        let expected_marker = target
            .get("expectedMarker")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let token = target
            .get("token")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Some(json!({
            "kind": "service",
            "targetId": target_id,
            "url": url,
            "expectedMarker": expected_marker,
            "token": token,
            "timeoutMs": timeout_ms,
        }))
    }

    fn normalize_target(target: &Value) -> Option<Value> {
        match target.get("kind").and_then(|v| v.as_str()) {
            Some("service") => Self::normalize_service_target(target),
            Some("tcp") | None => Self::normalize_tcp_target(target),
            _ => Self::normalize_tcp_target(target),
        }
    }

    fn tcp_probe_once(host: &str, port: u16, timeout: Duration) -> Result<u64, String> {
        let started = Instant::now();
        let addr = format!("{host}:{port}");
        use std::net::ToSocketAddrs;
        let mut last_err = String::from("probe failed");
        let addrs = addr.to_socket_addrs().map_err(|e| e.to_string())?;
        let mut any = false;
        for sa in addrs {
            any = true;
            match TcpStream::connect_timeout(&sa, timeout) {
                Ok(_s) => return Ok(started.elapsed().as_millis() as u64),
                Err(e) => last_err = e.to_string(),
            }
        }
        if !any {
            return Err(format!("could not resolve {addr}"));
        }
        Err(last_err)
    }

    fn run_tcp_probe_with_retry(target: &Value, attempts: u64) -> Value {
        let host = target.get("host").and_then(|v| v.as_str()).unwrap_or("");
        let port = target
            .get("port")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_PROBE_PORT as u64) as u16;
        let timeout_ms = target
            .get("timeoutMs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_PROBE_TIMEOUT_MS);
        let timeout = Duration::from_millis(timeout_ms);
        let target_id = target.get("targetId").cloned().unwrap_or(Value::Null);
        let mut last_error = String::new();

        for attempt in 1..=attempts {
            match Self::tcp_probe_once(host, port, timeout) {
                Ok(latency_ms) => {
                    return json!({
                        "targetId": target_id,
                        "kind": "tcp",
                        "host": host,
                        "port": port,
                        "reachable": true,
                        "attemptCount": attempt,
                        "latencyMs": latency_ms,
                    });
                }
                Err(e) => last_error = e,
            }
        }

        json!({
            "targetId": target_id,
            "kind": "tcp",
            "host": host,
            "port": port,
            "reachable": false,
            "attemptCount": attempts,
            "latencyMs": Value::Null,
            "error": if last_error.is_empty() { "probe failed".to_string() } else { last_error },
        })
    }

    fn parse_http_url(url: &str) -> Result<(String, u16, String, bool), String> {
        let lower = url.to_ascii_lowercase();
        let (rest, default_port, is_https) = if lower.starts_with("https://") {
            (&url[8..], 443u16, true)
        } else if lower.starts_with("http://") {
            (&url[7..], 80u16, false)
        } else {
            return Err("invalid service url scheme".to_string());
        };
        let (hostport, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = if hostport.starts_with('[') {
            // IPv6 literal [::1]:port
            if let Some(end) = hostport.find(']') {
                let h = hostport[1..end].to_string();
                let p = hostport[end + 1..]
                    .strip_prefix(':')
                    .and_then(|s| s.parse::<u16>().ok())
                    .unwrap_or(default_port);
                (h, p)
            } else {
                return Err("invalid IPv6 host".to_string());
            }
        } else if let Some((h, p)) = hostport.rsplit_once(':') {
            if p.chars().all(|c| c.is_ascii_digit()) {
                (h.to_string(), p.parse::<u16>().unwrap_or(default_port))
            } else {
                (hostport.to_string(), default_port)
            }
        } else {
            (hostport.to_string(), default_port)
        };
        if host.is_empty() {
            return Err("invalid service url host".to_string());
        }
        Ok((host, port, path.to_string(), is_https))
    }

    async fn http_service_probe_once(
        url: &str,
        timeout: Duration,
        token: Option<&str>,
    ) -> Result<(u64, Option<String>), String> {
        let started = Instant::now();
        let (host, port, path, is_https) = Self::parse_http_url(url)?;
        if is_https {
            return Err("https service probe not supported (no TLS in this build); use http:// for intranet service probes".to_string());
        }
        let addr = format!("{host}:{port}");
        let connect = tokio::time::timeout(timeout, TokioTcpStream::connect(&addr))
            .await
            .map_err(|_| format!("timeout({}ms)", timeout.as_millis()))?
            .map_err(|e| e.to_string())?;
        let mut stream = connect;
        let mut req = format!(
            "GET {path} HTTP/1.1\r\nHost: {hostport}\r\nAccept: application/json\r\nConnection: close\r\nUser-Agent: nc-code-system-probe/1\r\n",
            hostport = if port == 80 { host.clone() } else { format!("{host}:{port}") }
        );
        if let Some(t) = token {
            // Header value: keep simple; intranet tokens are opaque.
            let safe = t.replace(['\r', '\n'], "");
            req.push_str(&format!("x-zcode-intranet-token: {safe}\r\n"));
        }
        req.push_str("\r\n");
        tokio::time::timeout(timeout, stream.write_all(req.as_bytes()))
            .await
            .map_err(|_| format!("timeout({}ms)", timeout.as_millis()))?
            .map_err(|e| e.to_string())?;

        let mut buf = Vec::with_capacity(4096);
        let mut tmp = [0u8; 4096];
        loop {
            let n = tokio::time::timeout(timeout, stream.read(&mut tmp))
                .await
                .map_err(|_| format!("timeout({}ms)", timeout.as_millis()))?
                .map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            let remaining = MAX_HTTP_BODY_BYTES
                .saturating_add(8192)
                .saturating_sub(buf.len());
            if remaining == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n.min(remaining)]);
            // Stop early if we have headers + full content-length body.
            if let Some(header_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
                if let Some(cl) = headers.lines().find_map(|l| {
                    l.strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                }) {
                    let body_start = header_end + 4;
                    if buf.len() >= body_start + cl {
                        buf.truncate(body_start + cl);
                        break;
                    }
                }
            }
        }

        let latency_ms = started.elapsed().as_millis() as u64;
        let text = String::from_utf8_lossy(&buf);
        let (head, body) = text
            .split_once("\r\n\r\n")
            .ok_or_else(|| "invalid HTTP response".to_string())?;
        let mut lines = head.lines();
        let status_line = lines
            .next()
            .ok_or_else(|| "invalid HTTP response".to_string())?;
        // HTTP/1.1 200 OK
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(|| "invalid HTTP status".to_string())?;
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}"));
        }
        let body_trim = body.trim();
        let parsed: Value =
            serde_json::from_str(body_trim).map_err(|_| "invalid service response".to_string())?;
        let obj = parsed
            .as_object()
            .ok_or_else(|| "invalid service response".to_string())?;
        let ok = obj
            .get("ok")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| "invalid service response: missing ok".to_string())?;
        if let Some(m) = obj.get("marker") {
            if !m.is_string() && !m.is_null() {
                return Err("invalid service response: marker must be string".to_string());
            }
        }
        if !ok {
            return Err("service returned ok=false".to_string());
        }
        let marker = obj
            .get("marker")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Ok((latency_ms, marker))
    }

    async fn run_service_probe_with_retry(target: &Value, attempts: u64) -> Value {
        let url = target.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let target_id = target.get("targetId").cloned().unwrap_or(Value::Null);
        let timeout_ms = target
            .get("timeoutMs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_PROBE_TIMEOUT_MS);
        let timeout = Duration::from_millis(timeout_ms);
        let token = target.get("token").and_then(|v| v.as_str());
        let expected_marker = target.get("expectedMarker").and_then(|v| v.as_str());
        let mut last_error = String::new();

        for attempt in 1..=attempts {
            match Self::http_service_probe_once(url, timeout, token).await {
                Ok((latency_ms, marker)) => {
                    if let Some(expected) = expected_marker {
                        let actual = marker.as_deref().unwrap_or("<empty>");
                        if actual != expected {
                            last_error =
                                format!("marker mismatch(expected={expected}, actual={actual})");
                            continue;
                        }
                    }
                    let mut result = json!({
                        "targetId": target_id,
                        "kind": "service",
                        "url": url,
                        "reachable": true,
                        "attemptCount": attempt,
                        "latencyMs": latency_ms,
                    });
                    if let Some(m) = marker {
                        result["marker"] = Value::String(m);
                    }
                    return result;
                }
                Err(e) => last_error = e,
            }
        }

        json!({
            "targetId": target_id,
            "kind": "service",
            "url": url,
            "reachable": false,
            "attemptCount": attempts,
            "latencyMs": Value::Null,
            "error": if last_error.is_empty() { "probe failed".to_string() } else { last_error },
        })
    }

    fn resolve_probe_strategy(targets: &[Value]) -> &'static str {
        if targets.is_empty() {
            return "tcp-connect";
        }
        let mut has_tcp = false;
        let mut has_service = false;
        for t in targets {
            match t.get("kind").and_then(|v| v.as_str()) {
                Some("service") => has_service = true,
                _ => has_tcp = true,
            }
        }
        match (has_tcp, has_service) {
            (true, false) => "tcp-connect",
            (false, true) => "service-http",
            _ => "mixed",
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    async fn probe_intranet(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "request")?;
        let attempts = Self::normalize_attempts(params.get("attempts").and_then(|v| v.as_u64()));
        let raw_targets = params
            .get("targets")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let normalized: Vec<Value> = raw_targets
            .iter()
            .filter_map(Self::normalize_target)
            .collect();
        let required_success_count = Self::normalize_required_success_count(
            params.get("requiredSuccessCount").and_then(|v| v.as_u64()),
            normalized.len(),
        );

        let mut handles = Vec::with_capacity(normalized.len());
        for target in &normalized {
            let t = target.clone();
            if t.get("kind").and_then(|v| v.as_str()) == Some("service") {
                handles.push(tokio::spawn(async move {
                    Self::run_service_probe_with_retry(&t, attempts).await
                }));
            } else {
                handles.push(tokio::task::spawn_blocking(move || {
                    Self::run_tcp_probe_with_retry(&t, attempts)
                }));
            }
        }
        let mut results = Vec::with_capacity(handles.len());
        for h in handles {
            let v = match h.await {
                Ok(v) => v,
                Err(e) => {
                    json!({"reachable": false, "error": e.to_string(), "attemptCount": attempts})
                }
            };
            results.push(v);
        }

        let reached_target_count = results
            .iter()
            .filter(|r| r.get("reachable").and_then(|v| v.as_bool()) == Some(true))
            .count() as u64;
        let is_intranet = !normalized.is_empty() && reached_target_count >= required_success_count;
        let strategy = Self::resolve_probe_strategy(&normalized);

        Ok(WireValue::Json(json!({
            "isIntranet": is_intranet,
            "reachedTargetCount": reached_target_count,
            "requiredSuccessCount": required_success_count,
            "totalTargets": normalized.len(),
            "checkedAt": Self::now_ms(),
            "strategy": strategy,
            "results": results,
        })))
    }
}

impl Default for SystemService {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl Channel for SystemService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        match command {
            "info" => self.info().map_err(ChannelFailure::from),
            "listIntegratedTerminalShells" => self
                .list_integrated_terminal_shells()
                .map_err(ChannelFailure::from),
            "probeIntranet" => self
                .probe_intranet(&args)
                .await
                .map_err(ChannelFailure::from),
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;
    use std::net::TcpListener;
    use std::thread;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener as TokioTcpListener;

    fn ctx() -> ChannelContext {
        ChannelContext::new("system", ClientMode::DesktopContinuous)
    }

    async fn serve_http_once(status: u16, body: &str) -> (u16, tokio::task::JoinHandle<()>) {
        serve_http_n(1, status, body).await
    }

    async fn serve_http_n(n: usize, status: u16, body: &str) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = body.to_string();
        let handle = tokio::spawn(async move {
            for _ in 0..n {
                if let Ok((mut s, _)) = listener.accept().await {
                    let mut buf = vec![0u8; 4096];
                    let _ = s.read(&mut buf).await;
                    let reason = if status == 200 { "OK" } else { "ERR" };
                    let resp = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                }
            }
        });
        (port, handle)
    }

    #[tokio::test]
    async fn info_returns_homedir_and_platform() {
        let service = SystemService::new();
        let result = service
            .call(&ctx(), "info", vec![], CancelToken::new())
            .await
            .unwrap();
        let WireValue::Json(v) = result else {
            panic!("json")
        };
        assert!(v["homedir"].is_string());
        assert_eq!(v["platform"], std::env::consts::OS);
    }

    #[tokio::test]
    async fn list_integrated_terminal_shells_returns_array() {
        let service = SystemService::new();
        let result = service
            .call(
                &ctx(),
                "listIntegratedTerminalShells",
                vec![],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = result else {
            panic!("json")
        };
        assert!(v.is_array(), "expected array: {v}");
        for item in v.as_array().unwrap() {
            assert!(item["id"].is_string());
            assert!(item["label"].is_string());
            assert!(item["path"].is_string());
        }
    }

    #[tokio::test]
    async fn probe_intranet_tcp_reachable_and_unreachable() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _handle = thread::spawn(move || {
            for _ in 0..4 {
                let _ = listener.accept();
            }
        });

        let service = SystemService::new();
        let request = json!({
            "targets": [
                {"kind": "tcp", "host": "127.0.0.1", "port": port, "id": "ok"},
                {"kind": "tcp", "host": "127.0.0.1", "port": 1, "timeoutMs": 150, "id": "bad"}
            ],
            "attempts": 1,
            "requiredSuccessCount": 1
        });
        let result = service
            .call(
                &ctx(),
                "probeIntranet",
                vec![WireValue::Json(request)],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = result else {
            panic!("json")
        };
        assert_eq!(v["totalTargets"], 2);
        assert_eq!(v["requiredSuccessCount"], 1);
        assert_eq!(v["isIntranet"], true);
        assert!(v["reachedTargetCount"].as_u64().unwrap() >= 1);
        let results = v["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        let ok = results.iter().find(|r| r["targetId"] == "ok").unwrap();
        assert_eq!(ok["reachable"], true);
        assert_eq!(ok["kind"], "tcp");
        assert!(ok["latencyMs"].is_number());
        let bad = results.iter().find(|r| r["targetId"] == "bad").unwrap();
        assert_eq!(bad["reachable"], false);
        assert!(bad["error"].is_string());
    }

    #[tokio::test]
    async fn probe_intranet_normalizes_defaults_and_strategy() {
        let service = SystemService::new();
        let request = json!({"targets": []});
        let result = service
            .call(
                &ctx(),
                "probeIntranet",
                vec![WireValue::Json(request)],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = result else {
            panic!("json")
        };
        assert_eq!(v["totalTargets"], 0);
        assert_eq!(v["requiredSuccessCount"], 1);
        assert_eq!(v["isIntranet"], false);
        assert_eq!(v["strategy"], "tcp-connect");
    }

    #[tokio::test]
    async fn probe_intranet_service_http_ok_and_marker() {
        let (port, _h) = serve_http_once(200, r#"{"ok":true,"marker":"intranet-1"}"#).await;
        let service = SystemService::new();
        let request = json!({
            "targets": [{
                "kind": "service",
                "url": format!("http://127.0.0.1:{port}/probe"),
                "id": "svc",
                "expectedMarker": "intranet-1",
                "token": "secret"
            }],
            "attempts": 1
        });
        let result = service
            .call(
                &ctx(),
                "probeIntranet",
                vec![WireValue::Json(request)],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = result else {
            panic!("json")
        };
        assert_eq!(v["strategy"], "service-http");
        assert_eq!(v["isIntranet"], true);
        let r = &v["results"][0];
        assert_eq!(r["reachable"], true);
        assert_eq!(r["kind"], "service");
        assert_eq!(r["marker"], "intranet-1");
        assert!(r["latencyMs"].is_number());
    }

    #[tokio::test]
    async fn probe_intranet_service_http_marker_mismatch_fails() {
        let (port, _h) = serve_http_n(2, 200, r#"{"ok":true,"marker":"wrong"}"#).await;
        let service = SystemService::new();
        let request = json!({
            "targets": [{
                "kind": "service",
                "url": format!("http://127.0.0.1:{port}/probe"),
                "expectedMarker": "expected"
            }],
            "attempts": 1
        });
        let result = service
            .call(
                &ctx(),
                "probeIntranet",
                vec![WireValue::Json(request)],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = result else {
            panic!("json")
        };
        let r = &v["results"][0];
        assert_eq!(r["reachable"], false);
        assert!(r["error"]
            .as_str()
            .unwrap_or("")
            .contains("marker mismatch"));
    }

    #[tokio::test]
    async fn probe_intranet_service_http_ok_false_and_http_error() {
        let (port, _h) = serve_http_once(200, r#"{"ok":false}"#).await;
        let service = SystemService::new();
        let request = json!({
            "targets": [{"kind":"service","url": format!("http://127.0.0.1:{port}/probe")}],
            "attempts": 1
        });
        let result = service
            .call(
                &ctx(),
                "probeIntranet",
                vec![WireValue::Json(request)],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = result else {
            panic!("json")
        };
        assert_eq!(v["results"][0]["reachable"], false);
        assert!(v["results"][0]["error"]
            .as_str()
            .unwrap_or("")
            .contains("ok=false"));

        let (port2, _h2) = serve_http_once(500, r#"{"ok":true}"#).await;
        let request2 = json!({
            "targets": [{"kind":"service","url": format!("http://127.0.0.1:{port2}/probe")}],
            "attempts": 1
        });
        let result2 = service
            .call(
                &ctx(),
                "probeIntranet",
                vec![WireValue::Json(request2)],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v2) = result2 else {
            panic!("json")
        };
        assert_eq!(v2["results"][0]["reachable"], false);
        assert!(v2["results"][0]["error"]
            .as_str()
            .unwrap_or("")
            .contains("HTTP 500"));
    }

    #[tokio::test]
    async fn probe_intranet_https_fails_honestly() {
        let service = SystemService::new();
        let request = json!({
            "targets": [{"kind": "service", "url": "https://example.invalid/probe", "id": "svc"}],
            "attempts": 1
        });
        let result = service
            .call(
                &ctx(),
                "probeIntranet",
                vec![WireValue::Json(request)],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = result else {
            panic!("json")
        };
        assert_eq!(v["strategy"], "service-http");
        let r = &v["results"][0];
        assert_eq!(r["reachable"], false);
        let err = r["error"].as_str().unwrap_or("");
        assert!(
            err.contains("https") && (err.contains("TLS") || err.contains("not supported")),
            "honest https error: {err}"
        );
    }

    #[tokio::test]
    async fn probe_intranet_mixed_strategy_with_real_http() {
        let (port, _h) = serve_http_once(200, r#"{"ok":true}"#).await;
        let service = SystemService::new();
        let request = json!({
            "targets": [
                {"kind": "tcp", "host": "127.0.0.1", "port": 1, "timeoutMs": 100},
                {"kind": "service", "url": format!("http://127.0.0.1:{port}/probe")}
            ],
            "attempts": 1
        });
        let result = service
            .call(
                &ctx(),
                "probeIntranet",
                vec![WireValue::Json(request)],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = result else {
            panic!("json")
        };
        assert_eq!(v["strategy"], "mixed");
        assert_eq!(v["totalTargets"], 2);
        assert_eq!(v["reachedTargetCount"], 1);
        let results = v["results"].as_array().unwrap();
        let svc = results.iter().find(|r| r["kind"] == "service").unwrap();
        assert_eq!(svc["reachable"], true);
    }

    #[tokio::test]
    async fn unknown_method_not_found() {
        let service = SystemService::new();
        let err = service
            .call(&ctx(), "nope", vec![], CancelToken::new())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Method not found"));
    }
}
