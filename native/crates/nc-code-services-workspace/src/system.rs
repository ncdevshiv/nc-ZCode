//! `system` channel — OS info, integrated terminal shells, intranet probes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::{AppPaths, ServiceArgs, ServiceError};
use serde_json::{json, Value};

pub struct SystemService {
    paths: AppPaths,
}

impl SystemService {
    pub fn new(paths: AppPaths) -> Arc<Self> {
        Arc::new(Self { paths })
    }

    fn info(&self) -> Value {
        json!({
            "homedir": self.paths.home_dir().to_string_lossy(),
            "platform": node_platform(),
        })
    }

    fn shell_options(&self) -> Value {
        if !cfg!(windows) {
            return json!([]);
        }
        let mut options = Vec::new();
        let comspec = env_value("ComSpec")
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "cmd.exe".to_string());
        options.push(json!({
            "dialect": "cmd",
            "id": format!("cmd:{comspec}"),
            "label": "CMD",
            "path": comspec,
            "source": "system",
        }));
        if let Some(git_bash) = resolve_git_bash() {
            options.push(json!({
                "dialect": "git-bash",
                "id": format!("git-bash:{}", git_bash.path),
                "label": "Git Bash",
                "path": git_bash.path,
                "source": git_bash.source,
            }));
        }
        Value::Array(options)
    }

    async fn probe(&self, request: Value) -> Result<Value, ServiceError> {
        let targets = request
            .get("targets")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| ServiceError::invalid_params("targets must be an array"))?;
        let attempts = normalize_attempts(request.get("attempts").and_then(Value::as_u64));
        let total = targets.len();
        let required = normalize_required(
            request.get("requiredSuccessCount").and_then(Value::as_u64),
            total,
        );
        let mut results = Vec::new();
        for target in &targets {
            match target.get("kind").and_then(Value::as_str) {
                Some("service") => results.push(probe_service_target(target, attempts).await),
                _ => results.push(probe_tcp_target(target, attempts).await),
            }
        }
        let reached = results
            .iter()
            .filter(|result| result["reachable"] == json!(true))
            .count();
        let strategy = if results.iter().all(|result| result["kind"] == json!("tcp")) {
            "tcp-connect"
        } else if results
            .iter()
            .all(|result| result["kind"] == json!("service"))
        {
            "service-http"
        } else {
            "mixed"
        };
        Ok(json!({
            "isIntranet": total > 0 && reached as u64 >= required,
            "reachedTargetCount": reached,
            "requiredSuccessCount": required,
            "totalTargets": total,
            "checkedAt": now_ms(),
            "strategy": strategy,
            "results": results,
        }))
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
            "info" => Ok(WireValue::Json(self.info())),
            "listIntegratedTerminalShells" => Ok(WireValue::Json(self.shell_options())),
            "probeIntranet" => {
                let request = args.required_json(0, "request")?;
                Ok(WireValue::Json(self.probe(request).await?))
            }
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

async fn probe_tcp_target(target: &Value, attempts: u32) -> Value {
    let host = target
        .get("host")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let port = target
        .get("port")
        .and_then(Value::as_u64)
        .filter(|port| (1..=65535).contains(port))
        .unwrap_or(22) as u16;
    let target_id = target
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{host}:{port}"));
    let timeout = normalize_timeout(target.get("timeoutMs").and_then(Value::as_u64));
    if host.is_empty() {
        return json!({
            "targetId": target_id, "kind": "tcp", "host": host, "port": port,
            "reachable": false, "attemptCount": attempts, "latencyMs": null,
            "error": "invalid target",
        });
    }
    let mut last_error = String::new();
    for attempt in 1..=attempts {
        let started = Instant::now();
        match tokio::time::timeout(
            timeout,
            tokio::net::TcpStream::connect((host.as_str(), port)),
        )
        .await
        {
            Ok(Ok(_)) => {
                return json!({
                    "targetId": target_id, "kind": "tcp", "host": host, "port": port,
                    "reachable": true, "attemptCount": attempt,
                    "latencyMs": started.elapsed().as_millis() as u64,
                });
            }
            Ok(Err(error)) => last_error = error.to_string(),
            Err(_) => last_error = format!("timeout({}ms)", timeout.as_millis()),
        }
    }
    json!({
        "targetId": target_id, "kind": "tcp", "host": host, "port": port,
        "reachable": false, "attemptCount": attempts, "latencyMs": null,
        "error": if last_error.is_empty() { "probe failed".to_string() } else { last_error },
    })
}

async fn probe_service_target(target: &Value, attempts: u32) -> Value {
    let url_text = target
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let parsed = url::Url::parse(&url_text).ok();
    let target_id = target
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| parsed.as_ref().map(|url| url.to_string()))
        .unwrap_or_else(|| url_text.clone());
    let Some(parsed) = parsed.filter(|url| matches!(url.scheme(), "http" | "https")) else {
        return json!({
            "targetId": target_id, "kind": "service", "url": url_text,
            "reachable": false, "attemptCount": attempts, "latencyMs": null,
            "error": "invalid target",
        });
    };
    let expected_marker = target
        .get("expectedMarker")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let token = target
        .get("token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let timeout = normalize_timeout(target.get("timeoutMs").and_then(Value::as_u64));
    let client = reqwest::Client::new();
    let mut last_error = String::new();
    for attempt in 1..=attempts {
        let started = Instant::now();
        let mut request = client.get(parsed.clone()).timeout(timeout);
        if let Some(token) = token {
            request = request.header("x-nc-code-intranet-token", token);
        }
        match request.send().await {
            Ok(response) if response.status().is_success() => {
                match response.json::<Value>().await {
                    Ok(payload) => {
                        let ok = payload.get("ok").and_then(Value::as_bool).unwrap_or(false);
                        let marker = payload.get("marker").and_then(Value::as_str);
                        if !ok {
                            last_error = "service returned ok=false".to_string();
                        } else if let Some(expected) = expected_marker {
                            if marker != Some(expected) {
                                last_error = format!(
                                    "marker mismatch(expected={expected}, actual={})",
                                    marker.unwrap_or("<empty>")
                                );
                                continue;
                            }
                        } else {
                            let mut result = json!({
                                "targetId": target_id, "kind": "service", "url": parsed.to_string(),
                                "reachable": true, "attemptCount": attempt,
                                "latencyMs": started.elapsed().as_millis() as u64,
                            });
                            if let Some(marker) = marker {
                                result["marker"] = Value::String(marker.to_string());
                            }
                            return result;
                        }
                    }
                    Err(error) => last_error = format!("invalid service response: {error}"),
                }
            }
            Ok(response) => last_error = format!("HTTP {}", response.status().as_u16()),
            Err(error) => last_error = error.to_string(),
        }
    }
    json!({
        "targetId": target_id, "kind": "service", "url": parsed.to_string(),
        "reachable": false, "attemptCount": attempts, "latencyMs": null,
        "error": if last_error.is_empty() { "probe failed".to_string() } else { last_error },
    })
}

struct GitBash {
    path: String,
    source: &'static str,
}

fn resolve_git_bash() -> Option<GitBash> {
    for candidate in [
        "C:\\Program Files\\Git\\bin\\bash.exe",
        "C:\\Program Files (x86)\\Git\\bin\\bash.exe",
    ] {
        if Path::new(candidate).is_file() {
            return Some(GitBash {
                path: candidate.to_string(),
                source: "system",
            });
        }
    }
    let path_value = env_value("PATH")?;
    let extensions = windows_executable_extensions();
    for entry in path_value
        .split(';')
        .filter(|entry| !entry.trim().is_empty())
    {
        for extension in &extensions {
            let candidate = PathBuf::from(entry).join(format!("git{extension}"));
            if !candidate.is_file() {
                continue;
            }
            let git_dir = candidate.parent()?;
            for inferred in [
                git_dir.join("..").join("bin").join("bash.exe"),
                git_dir.join("..").join("..").join("bin").join("bash.exe"),
            ] {
                if inferred.is_file() {
                    return Some(GitBash {
                        path: inferred.to_string_lossy().into_owned(),
                        source: "path",
                    });
                }
            }
        }
    }
    None
}

fn windows_executable_extensions() -> Vec<String> {
    let raw = env_value("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string());
    let mut values: Vec<String> = raw
        .split(';')
        .map(|value| value.trim().to_lowercase())
        .filter(|value| !value.is_empty())
        .collect();
    if !values.contains(&".exe".to_string()) {
        values.insert(0, ".exe".to_string());
    }
    values
}

fn env_value(key: &str) -> Option<String> {
    std::env::vars()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, value)| value)
}

fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    }
}

fn normalize_attempts(value: Option<u64>) -> u32 {
    value.unwrap_or(2).clamp(1, 3) as u32
}

fn normalize_required(value: Option<u64>, total: usize) -> u64 {
    match value {
        Some(value) => value.clamp(1, total.max(1) as u64),
        None => 1,
    }
}

fn normalize_timeout(value: Option<u64>) -> Duration {
    Duration::from_millis(value.unwrap_or(800).clamp(100, 10_000))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_matches_node_names() {
        assert!(!node_platform().is_empty());
    }

    #[test]
    fn probe_normalization_clamps_values() {
        assert_eq!(normalize_attempts(None), 2);
        assert_eq!(normalize_attempts(Some(9)), 3);
        assert_eq!(normalize_required(None, 3), 1);
        assert_eq!(normalize_required(Some(9), 3), 3);
        assert_eq!(normalize_timeout(None), Duration::from_millis(800));
        assert_eq!(normalize_timeout(Some(50)), Duration::from_millis(100));
    }

    #[tokio::test]
    async fn tcp_probe_reports_latency_for_a_listening_port() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let result = probe_tcp_target(
            &json!({"host": "127.0.0.1", "port": port, "id": "local"}),
            1,
        )
        .await;
        assert_eq!(result["reachable"], json!(true));
        assert_eq!(result["targetId"], json!("local"));
        assert!(result["latencyMs"].is_u64());
    }

    #[tokio::test]
    async fn tcp_probe_fails_for_a_closed_port() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        let result = probe_tcp_target(
            &json!({"host": "127.0.0.1", "port": port, "timeoutMs": 150}),
            2,
        )
        .await;
        assert_eq!(result["reachable"], json!(false));
        assert_eq!(result["attemptCount"], json!(2));
    }
}
