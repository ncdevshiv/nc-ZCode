//! Rust window-connection registry (spec §2).
//!
//! Re-invents TS `windowRemoteConnectionRegistry.ts` semantics over an
//! in-process facade set: `connect` mints a `remoteSessionId` for a
//! workspace (SSH host key / WSL distro+user / Docker dedicated), later
//! resolves that id back to the owning `TaskService` facade. The registry
//! is the single owner of remote-session routing for the window
//! controller; the controller never guesses a facade by workspace path.
//!
//! Transports: the facade supplied at `connect` time is the already-
//! forwarded channel for that target (WSL bind / Docker volume share /
//! SSH tunnel to a remote `zcode-server`). This increment wires the
//! routing contract with in-process facades; a live SSH forward plugs in
//! behind `connect` without changing the routing surface.

use std::collections::HashMap;
use std::sync::Arc;

use nc_code_channels::Channel;
use nc_code_service_core::ServiceError;
use nc_code_services_agent::TaskService;
use serde_json::json;

use crate::remote_ws_facade::WsTaskService;

#[derive(Debug, Clone)]
pub struct RemoteSessionSnapshot {
    pub remote_session_id: String,
    pub workspace_path: String,
    pub workspace_identity: String,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteConnectionState {
    Connecting,
    Online,
    Closing,
    Failed,
    Disconnected,
}

struct Entry {
    workspace_path: String,
    workspace_identity: String,
    generation: u64,
    state: RemoteConnectionState,
    /// Task-channel facade: in-process `TaskService` (local/host-remote) or
    /// `crate::remote_ws_facade::WsTaskService` (WS-forwarded over a tunnel).
    tasks: Arc<dyn Channel>,
    tunnel: Option<std::process::Child>,
}

/// Window-scoped remote-connection registry. One instance per window host.
pub struct RemoteConnectionRegistry {
    sessions: std::sync::Mutex<HashMap<String, Entry>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl RemoteConnectionRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            sessions: std::sync::Mutex::new(HashMap::new()),
            next_id: std::sync::atomic::AtomicU64::new(1),
        })
    }

    fn next_id(&self, prefix: &str) -> String {
        let seq = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!(
            "{prefix}-{}-{seq}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.sessions.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Connect a remote workspace: register the forwarded facade and mint a
    /// logical session id (mirrors TS `connect` → descriptor with
    /// `remoteSessionId` + `generation`).
    pub fn connect(
        &self,
        workspace_path: impl Into<String>,
        workspace_identity: impl Into<String>,
        tasks: Arc<TaskService>,
    ) -> RemoteSessionSnapshot {
        let tasks: Arc<dyn Channel> = tasks;
        self.connect_with_tunnel(workspace_path, workspace_identity, tasks, None)
    }

    /// Register a pre-built WS facade (e.g. [`WsTaskService`] connected to a
    /// tunneled `ws://…/ws` endpoint) without spawning a tunnel child. The
    /// caller owns the tunnel lifetime; `dispose` closes the session.
    pub fn connect_ws_facade(
        &self,
        workspace_path: impl Into<String>,
        workspace_identity: impl Into<String>,
        tasks: Arc<WsTaskService>,
    ) -> RemoteSessionSnapshot {
        let tasks: Arc<dyn Channel> = tasks;
        self.connect_with_tunnel(workspace_path, workspace_identity, tasks, None)
    }

    /// SSH tunnel + WS facade: `ssh -N -L` to the remote `zcode-server`, then
    /// a [`WsTaskService`] over the tunneled WS endpoint (real wire proxy —
    /// invokes go over WS, never an in-process `TaskService`). The registry
    /// owns the tunnel child and kills it on dispose (no process leak).
    pub async fn connect_ssh_tunnel_ws(
        &self,
        host: &str,
        remote_port: u16,
        workspace_path: impl Into<String>,
        workspace_identity: impl Into<String>,
    ) -> Result<RemoteSessionSnapshot, ServiceError> {
        let host = host.trim();
        if host.is_empty() {
            return Err(ServiceError::invalid_params("ssh host is required"));
        }
        let ssh_bin = std::env::var("ZCODE_SSH_BIN").unwrap_or_else(|_| "ssh".to_string());
        let (tunnel, local_port) = self
            .ssh_tunnel_with_port(&ssh_bin, host, remote_port)
            .map_err(|reason| {
                ServiceError::named(
                    "RemoteSourceUnavailable",
                    format!("ssh tunnel failed ({host}:{remote_port}): {reason}"),
                )
            })?;
        let facade = WsTaskService::connect(&format!("ws://127.0.0.1:{local_port}/ws"))
            .await
            .map_err(|error| {
                ServiceError::named(
                    "RemoteSourceUnavailable",
                    format!("ws facade connect failed ({host}:{remote_port}): {error}"),
                )
            });
        match facade {
            Ok(facade) => {
                let facade: Arc<dyn Channel> = facade;
                Ok(self.connect_with_tunnel(
                    workspace_path,
                    workspace_identity,
                    facade,
                    Some(tunnel),
                ))
            }
            Err(error) => {
                let mut tunnel = Some(tunnel);
                kill_tunnel(&mut tunnel);
                Err(error)
            }
        }
    }

    fn connect_with_tunnel(
        &self,
        workspace_path: impl Into<String>,
        workspace_identity: impl Into<String>,
        tasks: Arc<dyn Channel>,
        tunnel: Option<std::process::Child>,
    ) -> RemoteSessionSnapshot {
        let workspace_path = workspace_path.into();
        let workspace_identity = workspace_identity.into();
        let remote_session_id = self.next_id("remote-session");
        self.lock().insert(
            remote_session_id.clone(),
            Entry {
                workspace_path: workspace_path.clone(),
                workspace_identity: workspace_identity.clone(),
                generation: 1,
                state: RemoteConnectionState::Online,
                tasks,
                tunnel,
            },
        );
        RemoteSessionSnapshot {
            remote_session_id,
            workspace_path,
            workspace_identity,
            generation: 1,
        }
    }

    /// SSH transport gate: spawn `ssh` (argv form, no shell) to prove the
    /// target is reachable before registering the forwarded facade. Fails
    /// closed with `RemoteSourceUnavailable` when `ssh` is missing, times out,
    /// or the probe output lacks the marker — never registers a fake session.
    pub fn connect_ssh(
        &self,
        host: &str,
        port: u16,
        workspace_path: impl Into<String>,
        workspace_identity: impl Into<String>,
        tasks: Arc<TaskService>,
    ) -> Result<RemoteSessionSnapshot, ServiceError> {
        let host = host.trim();
        if host.is_empty() {
            return Err(ServiceError::invalid_params("ssh host is required"));
        }
        let ssh_bin = std::env::var("ZCODE_SSH_BIN").unwrap_or_else(|_| "ssh".to_string());
        self.ssh_probe(&ssh_bin, host, port).map_err(|reason| {
            ServiceError::named(
                "RemoteSourceUnavailable",
                format!("ssh target unreachable ({host}:{port}): {reason}"),
            )
        })?;
        Ok(self.connect(workspace_path, workspace_identity, tasks))
    }

    /// SSH tunnel: `ssh -N -L <local>:<host>:<remotePort>` to a remote
    /// `zcode-server`, health-probed via HTTP `/healthz` on the local end.
    /// The tunnel `Child` is owned by the registry entry and killed on
    /// dispose/disconnect. The registered facade is still the in-process
    /// `TaskService` (WSL bind / Docker volume share parity); a WS-channel
    /// proxy over the tunnel is the documented next step for pure SSH hosts.
    pub fn connect_ssh_tunnel(
        &self,
        host: &str,
        remote_port: u16,
        workspace_path: impl Into<String>,
        workspace_identity: impl Into<String>,
        tasks: Arc<TaskService>,
    ) -> Result<RemoteSessionSnapshot, ServiceError> {
        let host = host.trim();
        if host.is_empty() {
            return Err(ServiceError::invalid_params("ssh host is required"));
        }
        let ssh_bin = std::env::var("ZCODE_SSH_BIN").unwrap_or_else(|_| "ssh".to_string());
        let tunnel = self
            .ssh_tunnel(&ssh_bin, host, remote_port)
            .map_err(|reason| {
                ServiceError::named(
                    "RemoteSourceUnavailable",
                    format!("ssh tunnel failed ({host}:{remote_port}): {reason}"),
                )
            })?;
        let tasks: Arc<dyn Channel> = tasks;
        Ok(self.connect_with_tunnel(workspace_path, workspace_identity, tasks, Some(tunnel)))
    }

    fn ssh_tunnel(
        &self,
        ssh_bin: &str,
        host: &str,
        remote_port: u16,
    ) -> Result<std::process::Child, String> {
        Ok(self.ssh_tunnel_with_port(ssh_bin, host, remote_port)?.0)
    }

    fn ssh_tunnel_with_port(
        &self,
        ssh_bin: &str,
        host: &str,
        remote_port: u16,
    ) -> Result<(std::process::Child, u16), String> {
        let local_port = free_tcp_port().ok_or_else(|| "cannot bind a local port".to_string())?;
        // Forward to `127.0.0.1` on the remote side (the server listens on
        // loopback), not the SSH hostname — so `host` may carry
        // `user@host` as the SSH destination without breaking `-L`.
        let forward = format!("{local_port}:127.0.0.1:{remote_port}");
        let mut child = std::process::Command::new(ssh_bin)
            .args([
                "-N",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-L",
                &forward,
                host,
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn ssh ({ssh_bin}): {e}"))?;
        // Poll the local port: the remote `zcode-server` must answer /healthz.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            if child
                .try_wait()
                .map_err(|e| format!("wait ssh tunnel: {e}"))?
                .is_some()
            {
                return Err("ssh tunnel exited before health probe".to_string());
            }
            if tcp_probe_health(local_port) {
                return Ok((child, local_port));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = nc_code_proc::kill_tree(child.id(), 2_000);
        let _ = child.kill();
        Err("ssh tunnel /healthz probe timed out after 15s".to_string())
    }

    fn ssh_probe(&self, ssh_bin: &str, host: &str, port: u16) -> Result<String, String> {
        let port_str = port.to_string();
        let mut child = std::process::Command::new(ssh_bin)
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-p",
                &port_str,
                host,
                "echo zcode-remote-ok",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn ssh ({ssh_bin}): {e}"))?;
        let pid = child.id();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait());
        });
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Ok(status)) => {
                    if !status.success() {
                        return Err(format!("ssh exited with {status}"));
                    }
                    return Ok("zcode-remote-ok".to_string());
                }
                Ok(Err(e)) => return Err(format!("wait ssh: {e}")),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("ssh wait channel closed".to_string())
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = nc_code_proc::kill_tree(pid, 2_000);
        Err("ssh probe timed out after 15s".to_string())
    }

    /// Bind (or rebind) a logical session to a workspace context: bumps the
    /// generation so stale owners cannot write through the old context.
    pub fn bind_workspace_context(
        &self,
        remote_session_id: &str,
        workspace_path: impl Into<String>,
        workspace_identity: impl Into<String>,
    ) -> Result<u64, ServiceError> {
        let workspace_path = workspace_path.into();
        let workspace_identity = workspace_identity.into();
        let mut sessions = self.lock();
        let entry = sessions.get_mut(remote_session_id).ok_or_else(|| {
            ServiceError::named(
                "RemoteSourceUnavailable",
                format!("remote session not found, remoteSessionId={remote_session_id}"),
            )
        })?;
        entry.workspace_path = workspace_path;
        entry.workspace_identity = workspace_identity;
        entry.generation += 1;
        entry.state = RemoteConnectionState::Online;
        Ok(entry.generation)
    }

    /// Resolve the owning facade for a fully-scoped attachment/mutation.
    /// Fails closed: unknown id, mismatched workspace, or non-online state
    /// all report `RemoteSourceUnavailable` (a routing fact, not a silent miss).
    pub fn resolve_scoped(
        &self,
        remote_session_id: &str,
        workspace_path: &str,
        workspace_identity: &str,
    ) -> Result<Arc<dyn Channel>, ServiceError> {
        let sessions = self.lock();
        let entry = sessions.get(remote_session_id).ok_or_else(|| {
            ServiceError::named(
                "RemoteSourceUnavailable",
                format!("remote session not found, remoteSessionId={remote_session_id}"),
            )
        })?;
        if entry.workspace_path != workspace_path || entry.workspace_identity != workspace_identity
        {
            return Err(ServiceError::named(
                "RemoteSourceUnavailable",
                format!("remote attachment scope does not match logical session, remoteSessionId={remote_session_id}"),
            ));
        }
        if !matches!(entry.state, RemoteConnectionState::Online) {
            return Err(ServiceError::named(
                "RemoteSourceUnavailable",
                format!("remote connection is not online, remoteSessionId={remote_session_id}"),
            ));
        }
        Ok(Arc::clone(&entry.tasks))
    }

    /// Find the unique logical session owning a workspace. More than one
    /// match is ambiguous and fails closed (mirrors TS `findSessionForWorkspace`).
    pub fn find_session_for_workspace(
        &self,
        workspace_path: &str,
        workspace_identity: &str,
    ) -> Result<Option<RemoteSessionSnapshot>, ServiceError> {
        let sessions = self.lock();
        let matches: Vec<(String, &Entry)> = sessions
            .iter()
            .filter(|(_, e)| {
                e.workspace_path == workspace_path && e.workspace_identity == workspace_identity
            })
            .map(|(id, e)| (id.clone(), e))
            .collect();
        if matches.len() > 1 {
            return Err(ServiceError::message(format!(
                "remote workspace scope matches multiple logical sessions, workspacePath={workspace_path}"
            )));
        }
        Ok(matches
            .into_iter()
            .next()
            .map(|(id, e)| RemoteSessionSnapshot {
                remote_session_id: id,
                workspace_path: e.workspace_path.clone(),
                workspace_identity: e.workspace_identity.clone(),
                generation: e.generation,
            }))
    }

    pub fn get_session(&self, remote_session_id: &str) -> Option<RemoteSessionSnapshot> {
        let sessions = self.lock();
        sessions
            .get(remote_session_id)
            .map(|e| RemoteSessionSnapshot {
                remote_session_id: remote_session_id.to_string(),
                workspace_path: e.workspace_path.clone(),
                workspace_identity: e.workspace_identity.clone(),
                generation: e.generation,
            })
    }

    pub fn list_sessions(&self) -> Vec<RemoteSessionSnapshot> {
        let sessions = self.lock();
        let mut list: Vec<RemoteSessionSnapshot> = sessions
            .iter()
            .map(|(id, e)| RemoteSessionSnapshot {
                remote_session_id: id.clone(),
                workspace_path: e.workspace_path.clone(),
                workspace_identity: e.workspace_identity.clone(),
                generation: e.generation,
            })
            .collect();
        list.sort_by(|a, b| a.remote_session_id.cmp(&b.remote_session_id));
        list
    }

    pub fn stats(&self) -> (usize, usize) {
        let sessions = self.lock();
        let online = sessions
            .values()
            .filter(|e| matches!(e.state, RemoteConnectionState::Online))
            .count();
        (sessions.len(), online)
    }

    /// Dispose a logical session (idempotent: unknown id is a no-op).
    /// Kills the owned SSH tunnel child when present (kill tree first, then
    /// direct kill as fallback — a lingering `ssh -N` must not survive the session).
    pub fn dispose_session(&self, remote_session_id: &str) {
        let mut sessions = self.lock();
        if let Some(mut entry) = sessions.remove(remote_session_id) {
            kill_tunnel(&mut entry.tunnel);
        }
    }

    /// Mark a session disconnected (frozen rows stay, writes fail closed).
    pub fn disconnect_session(&self, remote_session_id: &str) {
        let mut sessions = self.lock();
        if let Some(entry) = sessions.get_mut(remote_session_id) {
            entry.state = RemoteConnectionState::Disconnected;
        }
    }
}

impl Default for RemoteConnectionRegistry {
    fn default() -> Self {
        Self {
            sessions: std::sync::Mutex::new(HashMap::new()),
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }
}

fn free_tcp_port() -> Option<u16> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|listener| listener.local_addr().ok())
        .map(|addr| addr.port())
}

fn tcp_probe_health(local_port: u16) -> bool {
    use std::io::{Read, Write};
    let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", local_port)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
    let request = format!(
        "GET /healthz HTTP/1.0\r\nHost: 127.0.0.1:{local_port}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 4096];
    match stream.read(&mut buf) {
        Ok(n) => {
            let text = String::from_utf8_lossy(&buf[..n]);
            text.contains("200") && text.contains("\"status\":\"ok\"")
        }
        Err(_) => false,
    }
}

fn kill_tunnel(tunnel: &mut Option<std::process::Child>) {
    if let Some(mut child) = tunnel.take() {
        let _ = nc_code_proc::kill_tree(child.id(), 2_000);
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Snapshot JSON matching `WindowRemoteLogicalSessionSnapshot` (strict fields).
pub fn snapshot_json(snap: &RemoteSessionSnapshot) -> serde_json::Value {
    json!({
        "remoteSessionId": snap.remote_session_id,
        "workspacePath": snap.workspace_path,
        "workspaceIdentity": snap.workspace_identity,
        "generation": snap.generation,
        "state": "online",
        "sourceAvailability": "online",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_service_core::AppPaths;
    use nc_code_services_agent::AgentServices;
    use nc_code_services_provider::ProviderStore;

    fn test_tasks(tag: &str) -> (Arc<TaskService>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("nc-remote-reg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let paths = AppPaths::new(dir.join("home"), dir.join("data"));
        std::fs::create_dir_all(paths.app_config_dir()).unwrap();
        let agent = AgentServices::new(paths.clone(), ProviderStore::new(paths.clone()));
        let tasks = agent.tasks.clone();
        (tasks, dir)
    }

    #[test]
    fn connect_bind_resolve_round_trip() {
        let (tasks, dir) = test_tasks("roundtrip");
        let registry = RemoteConnectionRegistry::new();
        let snap = registry.connect("/ws-remote", "id-remote", tasks.clone());
        assert_eq!(snap.generation, 1);
        let resolved = registry
            .resolve_scoped(&snap.remote_session_id, "/ws-remote", "id-remote")
            .expect("resolve");
        // Behavioral proof the resolved facade is the registered one: a real
        // `listTasks` call through the resolved Channel answers (never a
        // nil facade). The in-process facade returns the (empty) real list.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let listed = runtime.block_on(async {
            let ctx = nc_code_channels::ChannelContext::new(
                "test",
                nc_code_channels::ClientMode::DesktopContinuous,
            );
            resolved
                .call(
                    &ctx,
                    "listTasks",
                    vec![nc_code_rpc::WireValue::Json(serde_json::json!({
                        "workspacePath": "/ws-remote",
                    }))],
                    nc_code_channels::CancelToken::new(),
                )
                .await
        });
        assert!(
            matches!(listed, Ok(nc_code_rpc::WireValue::Json(ref v)) if v.is_array()),
            "resolved facade must answer listTasks, got: {listed:?}"
        );
        // Rebind bumps generation.
        let gen = registry
            .bind_workspace_context(&snap.remote_session_id, "/ws-remote-2", "id-remote-2")
            .expect("rebind");
        assert_eq!(gen, 2);
        assert!(registry
            .resolve_scoped(&snap.remote_session_id, "/ws-remote", "id-remote")
            .is_err());
        assert!(registry
            .resolve_scoped(&snap.remote_session_id, "/ws-remote-2", "id-remote-2")
            .is_ok());
        assert_eq!(registry.list_sessions().len(), 1);
        registry.dispose_session(&snap.remote_session_id);
        assert!(registry.get_session(&snap.remote_session_id).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_for_workspace_fails_closed_on_ambiguity() {
        let (tasks, dir) = test_tasks("ambiguity");
        let registry = RemoteConnectionRegistry::new();
        let a = registry.connect("/ws", "id", Arc::clone(&tasks));
        let _b = registry.connect("/ws", "id", Arc::clone(&tasks));
        let err = registry.find_session_for_workspace("/ws", "id");
        assert!(
            err.is_err(),
            "two logical sessions for one workspace must fail closed"
        );
        registry.dispose_session(&a.remote_session_id);
        let found = registry
            .find_session_for_workspace("/ws", "id")
            .expect("single owner")
            .expect("must find one");
        assert_eq!(found.workspace_path, "/ws");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_session_is_named_remote_unavailable() {
        let registry = RemoteConnectionRegistry::new();
        let err = match registry.resolve_scoped("missing", "/p", "id") {
            Ok(_) => panic!("unknown session must fail"),
            Err(e) => e,
        };
        assert_eq!(err.name, "RemoteSourceUnavailable");
    }

    #[test]
    fn ssh_connect_fails_closed_on_empty_host() {
        let registry = RemoteConnectionRegistry::new();
        // Empty host is a validation error, never a registered session.
        let (tasks, dir) = test_tasks("ssh-empty");
        let err = registry
            .connect_ssh("", 22, "/ws", "id", Arc::clone(&tasks))
            .unwrap_err();
        assert_eq!(err.name, "ZodError");
        assert!(registry.list_sessions().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ssh_connect_fails_closed_on_missing_binary() {
        let registry = RemoteConnectionRegistry::new();
        std::env::set_var("ZCODE_SSH_BIN", "zcode-nonexistent-ssh-bin-xyz");
        let (tasks, dir) = test_tasks("ssh-missing");
        let err = registry
            .connect_ssh("127.0.0.1", 22, "/ws", "id", Arc::clone(&tasks))
            .unwrap_err();
        assert_eq!(err.name, "RemoteSourceUnavailable");
        assert!(err.message.contains("spawn ssh"));
        assert!(registry.list_sessions().is_empty());
        std::env::remove_var("ZCODE_SSH_BIN");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ssh_tunnel_fails_closed_on_empty_host_and_missing_binary() {
        let registry = RemoteConnectionRegistry::new();
        let (tasks, dir) = test_tasks("ssh-tunnel-empty");
        let err = registry
            .connect_ssh_tunnel("", 8080, "/ws", "id", Arc::clone(&tasks))
            .unwrap_err();
        assert_eq!(err.name, "ZodError");
        std::env::set_var("ZCODE_SSH_BIN", "zcode-nonexistent-ssh-bin-xyz");
        let err = registry
            .connect_ssh_tunnel("127.0.0.1", 8080, "/ws", "id", Arc::clone(&tasks))
            .unwrap_err();
        assert_eq!(err.name, "RemoteSourceUnavailable");
        assert!(registry.list_sessions().is_empty());
        std::env::remove_var("ZCODE_SSH_BIN");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ssh_tunnel_ws_fails_closed_on_empty_host() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let registry = RemoteConnectionRegistry::new();
        let err = runtime
            .block_on(registry.connect_ssh_tunnel_ws("", 8080, "/ws", "id"))
            .unwrap_err();
        assert_eq!(err.name, "ZodError");
        assert!(registry.list_sessions().is_empty());
    }

    #[test]
    fn ssh_tunnel_ws_fails_closed_on_missing_binary() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let registry = RemoteConnectionRegistry::new();
        std::env::set_var("ZCODE_SSH_BIN", "zcode-nonexistent-ssh-bin-xyz");
        let err = runtime
            .block_on(registry.connect_ssh_tunnel_ws("127.0.0.1", 8080, "/ws", "id"))
            .unwrap_err();
        assert_eq!(err.name, "RemoteSourceUnavailable");
        assert!(registry.list_sessions().is_empty());
        std::env::remove_var("ZCODE_SSH_BIN");
    }

    #[test]
    fn dispose_kills_tunnel_child_process_no_leak() {
        // Real long-lived tunnel-child stand-in: `ping -t` never exits on
        // its own on Windows; `sleep 30` on unix. kill_tunnel must tree-kill
        // and reap it (the same code path dispose runs for `ssh -N`).
        #[cfg(windows)]
        let mut child = std::process::Command::new("ping")
            .args(["-t", "127.0.0.1"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn ping tunnel stand-in");
        #[cfg(not(windows))]
        let mut child = std::process::Command::new("sleep")
            .args(["30"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sleep tunnel stand-in");
        let pid = child.id();
        assert!(child.try_wait().expect("try_wait").is_none());
        let mut tunnel = Some(child);
        kill_tunnel(&mut tunnel);
        assert!(tunnel.is_none(), "disposed tunnel handle must be consumed");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let reaped = loop {
            if std::process::Command::new("tasklist")
                .arg("/FI")
                .arg(format!("PID eq {pid}"))
                .output()
                .map(|out| {
                    let text = String::from_utf8_lossy(&out.stdout).to_string();
                    !text.contains(&pid.to_string())
                })
                .unwrap_or(true)
            {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert!(reaped, "tunnel child pid {pid} must be reaped after dispose");
    }
}
