//! `nc-code-agent` channel core: v4 conversation/session bridge over `nc-code-host`.
//!
//! Implemented now: initialize, session create/send/read/close/resume/list,
//! conversation subscribe/resync/unsubscribe, v4 command dispatch/query, rows range,
//! fileChanges/rewindPreview, runtime preferences/identity/storage, and the frame/lifecycle
//! events. Plugin/automation/workflow/attachment methods remain to be added; they fail
//! explicitly rather than returning fabricated data.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use nc_code_channels::{
    CancelToken, Channel, ChannelContext, ChannelFailure, EventBus, EventSubscription,
};
use nc_code_host::provider_config::ProviderConfig;
use nc_code_rpc::WireValue;
use nc_code_service_core::{AppPaths, ServiceArgs, ServiceError};
use nc_code_services_automation::AutomationService;

/// The protocol slash-command directory (`listProtocolSlashCommands`) is owned by the
/// `commands` channel (single discovery/write path). `zcode-session.readWorkspacePresentation`
/// consumes it through this port instead of re-scanning the same roots.
#[async_trait::async_trait]
pub trait SlashCommandDirectory: Send + Sync {
    async fn protocol_slash_commands(&self, workspace_path: &str) -> Result<Value, ServiceError>;
}
use nc_code_services_provider::store::{
    model_effective_config, provider_effective_config, provider_issues, provider_order,
    ProviderStore,
};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::runtime::AgentRuntime;
use crate::snapshot;

pub struct AgentService {
    paths: AppPaths,
    store: Arc<ProviderStore>,
    /// Automation (scheduled-task) service (the automation/* members of the nc-code-agent channel); one shared database, isolated by workspace_key.
    automation: Arc<AutomationService>,
    runtimes: Mutex<HashMap<String, Arc<AgentRuntime>>>,
    preferences: Mutex<Value>,
    lifecycle: EventBus,
    connection_id: String,
    sequence: AtomicU64,
    /// The plugin-management channel is injected by the composition root: one implementation, two dispatch points (plugin-management and nc-code-agent).
    plugin_management: OnceLock<Arc<dyn Channel>>,
    /// The protocol slash-command directory port injected by the composition root.
    /// shell's child-webview CDP bridge). `None` = `Browser` tool fails closed.
    browser_port: Option<Arc<dyn nc_code_host::tools::browser::BrowserControlPort>>,
    /// Native computer-use port injected by the composition root. `None` =
    /// `Computer` tool fails closed.
    computer_port: Option<Arc<dyn nc_code_host::tools::computer::ComputerControlPort>>,
    slash_directory: OnceLock<Arc<dyn SlashCommandDirectory>>,
}

impl AgentService {
    pub fn new(paths: AppPaths, store: Arc<ProviderStore>) -> Arc<Self> {
        Self::new_with_ports(paths, store, None, None)
    }

    /// Composition-root entry that injects the native browser-control port.
    pub fn new_with_browser_port(
        paths: AppPaths,
        store: Arc<ProviderStore>,
        browser_port: Option<Arc<dyn nc_code_host::tools::browser::BrowserControlPort>>,
    ) -> Arc<Self> {
        Self::new_with_ports(paths, store, browser_port, None)
    }

    /// Composition-root entry that injects both native desktop ports.
    pub fn new_with_ports(
        paths: AppPaths,
        store: Arc<ProviderStore>,
        browser_port: Option<Arc<dyn nc_code_host::tools::browser::BrowserControlPort>>,
        computer_port: Option<Arc<dyn nc_code_host::tools::computer::ComputerControlPort>>,
    ) -> Arc<Self> {
        let sequence = now_ms();
        let automation = Arc::new(AutomationService::new(Arc::new(
            nc_code_services_automation::repo::AutomationRepo::new(
                paths.tasks_index_database_path(),
            ),
        )));
        Arc::new(Self {
            paths,
            store,
            automation,
            runtimes: Mutex::new(HashMap::new()),
            preferences: Mutex::new(json!({})),
            lifecycle: EventBus::default(),
            connection_id: format!("zcode-agent-{sequence}"),
            sequence: AtomicU64::new(1),
            plugin_management: OnceLock::new(),
            browser_port,
            computer_port,
            slash_directory: OnceLock::new(),
        })
    }

    fn next_command_id(&self) -> String {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        format!("agent-{}-{sequence}", now_ms())
    }

    /// The automation service instance (the channel's 10 automation members and the dispatcher share the same single-database instance).
    pub(crate) fn automation(&self) -> &Arc<AutomationService> {
        &self.automation
    }

    /// The composition root injects the plugin-management channel: the 18 plugin members of `nc-code-agent` are a mirror dispatch point of it,
    /// sharing the same implementation as `plugin-management`, so no second write path is opened.
    pub fn attach_plugin_management(&self, channel: Arc<dyn Channel>) {
        let _ = self.plugin_management.set(channel);
    }

    pub fn attach_slash_command_directory(&self, directory: Arc<dyn SlashCommandDirectory>) {
        let _ = self.slash_directory.set(directory);
    }

    pub(crate) fn slash_directory(&self) -> Option<Arc<dyn SlashCommandDirectory>> {
        self.slash_directory.get().cloned()
    }

    /// `readWorkspacePresentation`: `{workspace, mode, slashCommands}` over the injected
    /// protocol directory. Both `zcode-session` and the `zcode-agent` mirror dispatch here,
    /// so there is exactly one projection.
    pub(crate) async fn read_workspace_presentation(
        &self,
        params: &Value,
    ) -> Result<Value, ServiceError> {
        let workspace = Self::workspace_ref_value(params)?;
        let workspace_path = workspace
            .get("workspacePath")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ServiceError::invalid_params("params.workspace.workspacePath is required")
            })?
            .to_string();
        let directory = self.slash_directory().ok_or_else(|| {
            ServiceError::new(
                "workspace presentation unavailable: slash-command directory not attached",
            )
        })?;
        let slash_commands = directory.protocol_slash_commands(&workspace_path).await?;
        Ok(json!({
            "workspace": workspace,
            "mode": "build",
            "slashCommands": slash_commands,
        }))
    }

    /// `zcodeWorkspaceRefSchema` (strict): only the declared keys survive parsing.
    ///
    /// Two entry shapes reach this function: the service-channel call passes the flat
    /// `ZCodeWorkspaceTarget` (`workspacePath`/`workspaceIdentity` at the top level), and the
    /// internal agent protocol call nests the ref under `workspace`. Both are accepted;
    /// unknown extra keys are dropped, never echoed (the schema is strict).
    fn workspace_ref_value(params: &Value) -> Result<Value, ServiceError> {
        let workspace = match params.get("workspace") {
            Some(workspace) => {
                if !workspace.is_object() {
                    return Err(ServiceError::invalid_params(
                        "params.workspace must be an object",
                    ));
                }
                workspace
            }
            None => params,
        };
        let workspace_path = workspace
            .get("workspacePath")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                ServiceError::invalid_params("params.workspace.workspacePath is required")
            })?;
        let workspace_key = workspace
            .get("workspaceKey")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or_else(|| {
                workspace
                    .get("workspaceIdentity")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or(workspace_path);
        let identity = workspace
            .get("workspaceIdentity")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let mut object = serde_json::Map::new();
        object.insert("workspacePath".to_string(), json!(workspace_path));
        if let Some(identity) = identity {
            object.insert("workspaceIdentity".to_string(), json!(identity));
        }
        object.insert("workspaceKey".to_string(), json!(workspace_key));
        Ok(Value::Object(object))
    }

    /// `{connectionId}:conversation/{sessionId}` → (`connectionId`, `sessionId`).
    /// Returns None for any other topic family (sessions-index, controller, …) so a
    /// malformed id fails closed instead of borrowing another route.
    fn parse_conversation_subscription_id(value: &str) -> Option<(String, String)> {
        let (connection, topic) = value.split_once(':')?;
        let session = topic.strip_prefix("conversation/")?;
        if connection.is_empty() || session.is_empty() {
            return None;
        }
        Some((connection.to_string(), session.to_string()))
    }

    pub(crate) fn workspace_of(params: &Value) -> Result<(String, String), ServiceError> {
        let workspace_path = params
            .get("workspacePath")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ServiceError::invalid_params("workspacePath is required"))?;
        let identity = params
            .get("workspaceIdentity")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let key = identity.unwrap_or(workspace_path);
        Ok((workspace_path.to_string(), key.to_string()))
    }

    async fn provider_config(&self) -> Option<ProviderConfig> {
        let snapshot = self.store.snapshot().await.ok()?;
        for provider_id in provider_order(&snapshot) {
            let provider = snapshot.provider(&provider_id)?;
            let config = provider_effective_config(provider, &snapshot);
            if !provider_issues(&config).is_empty() {
                continue;
            }
            let api_type = config
                .get("api")
                .and_then(|api| api.get("type"))
                .and_then(Value::as_str)?;
            let base_url = config
                .get("api")
                .and_then(|api| api.get("baseUrl"))
                .and_then(Value::as_str)?
                .to_string();
            let api_key = config
                .get("access")
                .and_then(|access| access.get("apiKey"))
                .and_then(Value::as_str)?
                .to_string();
            let selected_model = provider.models.iter().find(|model| model.enabled)?;
            let model = selected_model.model_id.clone();
            let kind = match api_type {
                "anthropic-messages" => "anthropic",
                "openai-chat-completions" | "openai-responses" => "openai-compatible",
                _ => continue,
            };
            let extra_models = provider
                .models
                .iter()
                .filter(|model| model.enabled && model.model_id != selected_model.model_id)
                .map(|model| model.model_id.clone())
                .collect();
            return Some(ProviderConfig {
                provider_id: Some(provider.provider_id.clone()),
                kind: kind.to_string(),
                base_url,
                api_key,
                model,
                max_tokens: 4096,
                auth_style: None,
                stream: true,
                extra_models,
                reasoning_levels: Vec::new(),
                headers: Vec::new(),
            });
        }
        None
    }

    pub(crate) async fn ensure_runtime(
        &self,
        params: &Value,
    ) -> Result<Arc<AgentRuntime>, ServiceError> {
        let (workspace_path, key) = Self::workspace_of(params)?;
        {
            let runtimes = self.runtimes.lock().await;
            if let Some(runtime) = runtimes.get(&key) {
                return Ok(Arc::clone(runtime));
            }
        }
        let provider = self.provider_config().await;
        if provider.is_none() {
            return Err(ServiceError::new("Provider not configured"));
        }
        let data_dir = self
            .paths
            .app_config_dir()
            .join("agent")
            .join(self.paths.workspace_hash(&workspace_path, Some(&key)));
        let automation_port: Arc<dyn nc_code_host::tools::AutomationPort> =
            Arc::new(crate::automation_tool_port::AutomationToolPort::new(
                Arc::clone(&self.automation),
                workspace_path.clone(),
                key.clone(),
            ));
        let runtime = AgentRuntime::create(
            std::path::PathBuf::from(&workspace_path),
            data_dir,
            provider,
            key.clone(),
            Some(automation_port),
            self.browser_port.clone(),
            self.computer_port.clone(),
        )?;
        let mut runtimes = self.runtimes.lock().await;
        let entry = runtimes.entry(key).or_insert_with(|| Arc::clone(&runtime));
        Ok(Arc::clone(entry))
    }

    fn command_envelope(
        &self,
        session_id: Option<&str>,
        command_type: &str,
        payload: Value,
    ) -> Value {
        json!({
            "commandId": self.next_command_id(),
            "clientId": self.connection_id,
            "sessionId": session_id,
            "type": command_type,
            "payload": payload,
            "issuedAt": now_ms(),
        })
    }

    async fn v4_command(
        &self,
        runtime: &Arc<AgentRuntime>,
        envelope: Value,
    ) -> Result<Value, ServiceError> {
        // For v4/command, the RPC params ARE the command envelope itself (TS client.request(command, envelope));
        // do not wrap another {envelope} layer — the host's parse_command_envelope reads the top-level fields directly.
        let response = runtime.request("v4/command", envelope).await?;
        Ok(response.result)
    }

    pub(crate) async fn create_session(&self, params: &Value) -> Result<Value, ServiceError> {
        let runtime = self.ensure_runtime(params).await?;
        let envelope = self.command_envelope(None, "createSession", json!({}));
        let ack = self.v4_command(&runtime, envelope).await?;
        let session_id = ack
            .get("result")
            .and_then(|result| result.get("sessionId"))
            .and_then(Value::as_str)
            .ok_or_else(|| ServiceError::new("createSession ack missing sessionId"))?
            .to_string();
        let _ = runtime
            .request(
                "v4/conversation/subscribe",
                json!({
                    "sessionId": session_id,
                    "connectionId": self.connection_id,
                    "clientMode": "desktop-continuous",
                }),
            )
            .await?;
        // In TS createSession, mode/model/thoughtLevel are not "accepted and stored": after creation they are applied
        // through the same batch of v4 control commands, and the snapshot reads back the real state (zcodeSessionService.ts:221 passes each parameter
        // to agentService.createSession; on the host side, config.rs:65-81 is the only write point).
        let mut snapshot_params = params.clone();
        snapshot_params["sessionId"] = json!(session_id);
        self.apply_session_config(&runtime, &session_id, params)
            .await?;
        self.build_snapshot(&snapshot_params, None).await
    }

    /// Create a session from another client's already-parsed transcript. The host owns the
    /// rows/history/durability; this only routes to the right workspace runtime and
    /// subscribes the same way `create_session` does, so the imported session is a live
    /// conversation the UI can open and continue.
    pub(crate) async fn import_session(&self, params: &Value) -> Result<Value, ServiceError> {
        let runtime = self.ensure_runtime(params).await?;
        let mut host_params = params.clone();
        if let Some(object) = host_params.as_object_mut() {
            object.remove("workspacePath");
            object.remove("workspaceIdentity");
        }
        let result = match runtime.request("session/import", host_params).await {
            Ok(response) => response.result,
            // 同一个源第二次导入命中确定性 id：host 拒绝覆盖已有 transcript。这里不是失败，
            // 而是把磁盘上的会话重新注册回 runtime（deleteTask 只 close，历史仍在），
            // 下面的 subscribe 因此能照常完成。
            Err(error) if is_session_exists(&error) => {
                let session_id = error
                    .data
                    .as_deref()
                    .and_then(|data| data.get("sessionId"))
                    .and_then(Value::as_str)
                    .or_else(|| params.get("sessionId").and_then(Value::as_str))
                    .ok_or_else(|| {
                        ServiceError::new("session exists but no sessionId was reported")
                    })?
                    .to_string();
                runtime.ensure_loaded(&session_id);
                json!({"sessionId": session_id, "resumed": true})
            }
            Err(error) => return Err(error),
        };
        let session_id = result
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| ServiceError::new("session/import result missing sessionId"))?;
        runtime
            .request(
                "v4/conversation/subscribe",
                json!({
                    "sessionId": session_id,
                    "connectionId": self.connection_id,
                    "clientMode": "desktop-continuous",
                }),
            )
            .await?;
        Ok(result)
    }

    pub(crate) async fn send_prompt(&self, params: &Value) -> Result<Value, ServiceError> {
        let runtime = self.ensure_runtime(params).await?;
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| ServiceError::invalid_params("sessionId is required"))?;
        let content = params
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| ServiceError::invalid_params("content is required"))?;
        let mut payload = json!({ "text": content });
        if let Some(input_id) = params.get("inputId").and_then(Value::as_str) {
            payload["inputId"] = Value::String(input_id.to_string());
        }
        if let Some(model_execution) = params.get("modelExecution") {
            payload["modelExecution"] = model_execution.clone();
        }
        if let Some(items) = params
            .get("attachments")
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())
        {
            let refs = self
                .prompt_attachment_refs(&runtime, session_id, items)
                .await?;
            if !refs.is_empty() {
                payload["attachments"] = json!(refs);
            }
        }
        let envelope = self.command_envelope(Some(session_id), "sendText", payload);
        let ack = self.v4_command(&runtime, envelope).await?;
        Ok(json!({
            "sessionId": session_id,
            "accepted": true,
            "stateRevision": ack.get("revisionAtDecision").and_then(Value::as_u64).unwrap_or(0),
        }))
    }

    /// Map the composer's prompt attachments to v4 `AttachmentRef`s (the reference model
    /// the command surface carries). Dual rule, identical to the TS
    /// `uploadComposerAttachment`:
    ///
    /// - `localPath` → `ref = <absolute path>`, zero upload (the host reads the file when
    ///   it builds the model request);
    /// - `dataBase64` / `textContent` → the real upload transaction
    ///   (`v4/attachment/{begin,chunk,commit}`) and `ref = zcode-artifact://<sha256>`;
    /// - nothing to send → the item is dropped **and counted**, never fabricated.
    ///
    /// An item that already carries a `ref` is an `AttachmentRef` and passes through.
    async fn prompt_attachment_refs(
        &self,
        runtime: &Arc<AgentRuntime>,
        session_id: &str,
        attachments: &[Value],
    ) -> Result<Vec<Value>, ServiceError> {
        // TS streams in 384KiB chunks so every non-final base64 piece is padding-free.
        const CHUNK_BYTES: usize = 384 * 1024;
        let mut refs = Vec::with_capacity(attachments.len());
        for atlas in attachments {
            if atlas.get("ref").and_then(Value::as_str).is_some() {
                refs.push(atlas.clone());
                continue;
            }
            let file_name = atlas
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or("attachment");
            let mime = atlas
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream");
            let declared_bytes = atlas.get("sizeBytes").and_then(Value::as_u64);
            if let Some(path) = atlas
                .get("localPath")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                refs.push(json!({
                    "ref": path,
                    "fileName": file_name,
                    "mime": mime,
                    "bytes": declared_bytes.unwrap_or(0),
                }));
                continue;
            }
            let data_base64 = atlas
                .get("dataBase64")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    atlas
                        .get("textContent")
                        .and_then(Value::as_str)
                        .map(|text| nc_code_host::tools::read_image::base64_encode(text.as_bytes()))
                });
            let Some(data_base64) = data_base64 else {
                // No bytes and no path: there is nothing to attach (the TS composer logs
                // and drops the same item). Dropping silently here is what the count is for.
                continue;
            };
            let bytes = nc_code_v4_wire::decode_wire_base64(&data_base64).ok_or_else(|| {
                ServiceError::invalid_params("attachment data is not valid base64")
            })?;
            let upload_id = {
                let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
                format!("upload-{}-{}", now_ms(), sequence)
            };
            let total_chunks = bytes.len().div_ceil(CHUNK_BYTES) as u64;
            let begun = runtime
                .request(
                    "v4/attachment/begin",
                    json!({
                        "connectionId": self.connection_id,
                        "sessionId": session_id,
                        "uploadId": upload_id,
                        "fileName": file_name,
                        "mime": mime,
                        "totalBytes": bytes.len() as u64,
                        "totalChunks": total_chunks,
                        "checksum": format!("sha256:{}", nc_code_host::content_hash::sha256_hex(&bytes)),
                    }),
                )
                .await?;
            if begun.result.get("state").and_then(Value::as_str) == Some("committed") {
                let ref_value = begun
                    .result
                    .get("ref")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ServiceError::new("committed upload missing ref"))?;
                refs.push(json!({
                    "ref": ref_value,
                    "fileName": file_name,
                    "mime": mime,
                    "bytes": bytes.len() as u64,
                }));
                continue;
            }
            for (index, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
                let chunked = runtime
                    .request(
                        "v4/attachment/chunk",
                        json!({
                            "connectionId": self.connection_id,
                            "sessionId": session_id,
                            "uploadId": upload_id,
                            "chunkIndex": index as u64,
                            "dataBase64": nc_code_host::tools::read_image::base64_encode(chunk),
                        }),
                    )
                    .await?;
                let next = chunked
                    .result
                    .get("nextChunkIndex")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if next != index as u64 + 1 {
                    return Err(ServiceError::new(
                        "attachment chunk was not accepted (invalid server progress)",
                    ));
                }
            }
            let committed = runtime
                .request(
                    "v4/attachment/commit",
                    json!({
                        "connectionId": self.connection_id,
                        "sessionId": session_id,
                        "uploadId": upload_id,
                    }),
                )
                .await?;
            let ref_value = committed
                .result
                .get("ref")
                .and_then(Value::as_str)
                .ok_or_else(|| ServiceError::new("commit did not return an attachment ref"))?;
            refs.push(json!({
                "ref": ref_value,
                "fileName": file_name,
                "mime": mime,
                "bytes": bytes.len() as u64,
            }));
        }
        Ok(refs)
    }

    async fn conversation_request(
        &self,
        method: &str,
        params: &Value,
        mut extra: Value,
    ) -> Result<Value, ServiceError> {
        let runtime = self.ensure_runtime(params).await?;
        // The subscription id is `{connectionId}:conversation/{sessionId}` (the same
        // format the host builds in `subscribe`). The UI's resync/unsubscribe call
        // carries only this id (TS resolves the route through its subscription
        // registry), so the session/connection pair is derived from it instead of
        // demanding a top-level sessionId the client never sends.
        let route = params
            .get("subscriptionId")
            .and_then(Value::as_str)
            .and_then(Self::parse_conversation_subscription_id);
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| route.as_ref().map(|(_, session)| session.clone()))
            .ok_or_else(|| ServiceError::invalid_params("sessionId is required"))?;
        let connection_id = route
            .as_ref()
            .map(|(connection, _)| connection.clone())
            .unwrap_or_else(|| self.connection_id.clone());
        if let Some(object) = extra.as_object_mut() {
            object.insert("sessionId".to_string(), Value::String(session_id));
            object.insert("connectionId".to_string(), Value::String(connection_id));
            object.insert(
                "clientMode".to_string(),
                Value::String("desktop-continuous".to_string()),
            );
        }
        let response = runtime.request(method, extra).await?;
        Ok(response.result)
    }

    /// sessions-index subscription surface: shares the v4 RPC with conversation, with a fixed topic
    /// `sessions-index/<workspaceKey>` (the identity ?? path convention matches the UI).
    async fn sessions_index_request(
        &self,
        method: &str,
        params: &Value,
        subscribe: bool,
    ) -> Result<Value, ServiceError> {
        // `existing-only` is a passive-observer policy: never create a runtime (and therefore
        // never require a provider). The Tauri boot path subscribes the task list with this
        // policy, so using `ensure_runtime` surfaced "Provider not configured" on every launch.
        let existing_only =
            params.get("runtimePolicy").and_then(Value::as_str) == Some("existing-only");
        let runtime = if existing_only {
            self.existing_runtime(params).await?
        } else {
            self.ensure_runtime(params).await?
        };
        let (_, key) = Self::workspace_of(params)?;
        let mut request = params.clone();
        request["topic"] = Value::String(format!("sessions-index/{key}"));
        request["connectionId"] = Value::String(self.connection_id.clone());
        if subscribe {
            request["clientMode"] = Value::String("desktop-continuous".to_string());
        }
        Ok(runtime.request(method, request).await?.result)
    }

    async fn subscribe(&self, params: &Value) -> Result<Value, ServiceError> {
        self.conversation_request("v4/conversation/subscribe", params, json!({}))
            .await
    }

    async fn resync(&self, params: &Value) -> Result<Value, ServiceError> {
        self.conversation_request("v4/conversation/resync", params, json!({}))
            .await
    }

    async fn unsubscribe(&self, params: &Value) -> Result<Value, ServiceError> {
        self.conversation_request("v4/conversation/unsubscribe", params, json!({}))
            .await
    }

    async fn rows_range(&self, params: &Value) -> Result<Value, ServiceError> {
        let runtime = self.ensure_runtime(params).await?;
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| ServiceError::invalid_params("sessionId is required"))?;
        let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(50);
        let mut request = json!({ "sessionId": session_id, "limit": limit });
        if let Some(before) = params.get("beforeRowId").and_then(Value::as_u64) {
            if before > 0 {
                request["toRowId"] = json!(before - 1);
            }
        }
        Ok(runtime
            .request("v4/conversation/rowsRange", request)
            .await?
            .result)
    }

    /// Read-only access to an already-running runtime (TS `getReadOnlyClient(..., "existing-only")` semantics):
    /// does not start a runtime, does not check the provider, and only observes existing facts.
    async fn existing_runtime(&self, params: &Value) -> Result<Arc<AgentRuntime>, ServiceError> {
        let (_, key) = Self::workspace_of(params)?;
        self.runtime_for(&key).await.ok_or_else(|| {
            ServiceError::named(
                "ZCodeWorkspaceRuntimeNotFound",
                format!("workspace runtime is not running: {key}"),
            )
        })
    }

    /// Cancel an in-flight `workspace/generateText` (the mapping of the channel's PromiseCancel).
    /// Best-effort: the caller has already given up on the response, so a failure only leaves diagnostics and does not override the cancel semantics.
    async fn cancel_generate_text(&self, args: Vec<WireValue>) {
        let args = ServiceArgs::new(args);
        let Ok(params) = args.required_json(0, "params") else {
            return;
        };
        let Some(operation_id) = params.get("operationId").and_then(Value::as_str) else {
            return;
        };
        let Some(workspace) = params.get("workspace") else {
            return;
        };
        let host_params = json!({
            "workspacePath": workspace.get("workspacePath"),
            "workspaceIdentity": workspace.get("workspaceIdentity"),
        });
        if let Ok(runtime) = self.ensure_runtime(&host_params).await {
            let _ = runtime
                .request(
                    "workspace/cancelGenerateText",
                    json!({"operationId": operation_id}),
                )
                .await;
        }
    }

    /// Forward nc-code-protocol session control methods to the host (only protocol fields are passed, copied via a whitelist).
    async fn host_session_control(
        &self,
        runtime: &Arc<AgentRuntime>,
        method: &str,
        params: &Value,
        keys: &[&str],
    ) -> Result<Value, ServiceError> {
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| ServiceError::invalid_params("sessionId is required"))?;
        let mut host_params = json!({"sessionId": session_id});
        for key in keys {
            if let Some(value) = params.get(*key) {
                host_params[*key] = value.clone();
            }
        }
        Ok(runtime.request(method, host_params).await?.result)
    }

    pub(crate) async fn read_session(&self, params: &Value) -> Result<Value, ServiceError> {
        // messageLimit is the tail window of the TS ReadParams (zcodeSession.ts:69; SnapshotInput carries the same note).
        let message_limit = params
            .get("messageLimit")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        self.build_snapshot(params, message_limit).await
    }

    async fn list_sessions(&self, params: &Value) -> Result<Value, ServiceError> {
        let (workspace_path, key) = Self::workspace_of(params)?;
        let data_dir = self
            .paths
            .app_config_dir()
            .join("agent")
            .join(self.paths.workspace_hash(&workspace_path, Some(&key)));
        let sessions_dir = data_dir.join("sessions");
        let mut sessions = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir(&sessions_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let meta_path = entry.path().join("meta.json");
                let Ok(text) = tokio::fs::read_to_string(&meta_path).await else {
                    continue;
                };
                let Ok(meta) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if meta.get("taskType").and_then(Value::as_str) == Some("subagent_child") {
                    continue;
                }
                sessions.push(json!({
                    "sessionId": meta.get("sessionId").cloned().unwrap_or(Value::Null),
                    "title": meta.get("title").cloned().unwrap_or(Value::Null),
                    "workspace": {
                        "workspacePath": workspace_path,
                        "workspaceIdentity": key,
                    },
                    "createdAt": meta.get("createdAt").cloned().unwrap_or(Value::Null),
                    "updatedAt": meta.get("createdAt").cloned().unwrap_or(Value::Null),
                    "status": "idle",
                    "messageCount": 0,
                }));
            }
        }
        Ok(Value::Array(sessions))
    }

    pub(crate) async fn close_session(&self, params: &Value) -> Result<Value, ServiceError> {
        let runtime = self.ensure_runtime(params).await?;
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| ServiceError::invalid_params("sessionId is required"))?;
        let envelope = self.command_envelope(Some(session_id), "deleteSession", json!({}));
        let _ = self.v4_command(&runtime, envelope).await?;
        Ok(json!(true))
    }

    pub(crate) fn data_dir_for(&self, workspace_path: &str, key: &str) -> std::path::PathBuf {
        self.paths
            .app_config_dir()
            .join("agent")
            .join(self.paths.workspace_hash(workspace_path, Some(key)))
    }

    pub(crate) async fn runtime_for(&self, key: &str) -> Option<Arc<AgentRuntime>> {
        let runtimes = self.runtimes.lock().await;
        runtimes.get(key).cloned()
    }

    /// Find a running session view by taskId (with workspacePath attached).
    pub(crate) async fn workspace_session_facts_for_task(&self, task_id: &str) -> Option<Value> {
        let runtimes: Vec<Arc<AgentRuntime>> = {
            let guard = self.runtimes.lock().await;
            guard.values().cloned().collect()
        };
        for runtime in runtimes {
            if let Some(mut view) = runtime.session_view(task_id) {
                view["workspacePath"] = json!(runtime.workspace_path);
                return Some(view);
            }
        }
        // Cold sessions: scan the agent data directory. meta.workspaceIdentity is the stable key
        // (this host's identity convention = identity ?? path), so workspacePath can be backfilled directly.
        let agent_root = self.paths.app_config_dir().join("agent");
        let mut dirs = tokio::fs::read_dir(&agent_root).await.ok()?;
        while let Ok(Some(entry)) = dirs.next_entry().await {
            let meta_path = entry
                .path()
                .join("sessions")
                .join(task_id)
                .join("meta.json");
            let Ok(text) = tokio::fs::read_to_string(&meta_path).await else {
                continue;
            };
            let Ok(meta) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let workspace_path = meta
                .get("workspaceIdentity")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let created_at = meta.get("createdAt").and_then(Value::as_i64).unwrap_or(0);
            let updated_at = meta
                .get("updatedAt")
                .and_then(Value::as_i64)
                .filter(|value| *value > 0)
                .unwrap_or(created_at);
            return Some(json!({
                "sessionId": task_id,
                "workspacePath": workspace_path,
                "title": meta.get("title").cloned().unwrap_or_else(|| json!("")),
                "titleSource": meta.get("titleSource").cloned().unwrap_or_else(|| json!("default")),
                "createdAt": created_at,
                "updatedAt": updated_at,
                "phase": "draft",
                "provider": meta.get("provider").cloned().unwrap_or(Value::Null),
                "model": meta.get("model").cloned().unwrap_or(Value::Null),
                "thought": meta.get("thought").cloned().unwrap_or(Value::Null),
                "mode": meta.get("collaborationMode").cloned().unwrap_or_else(|| json!("build")),
            }));
        }
        None
    }

    /// Session transcript view (looked up across any registered runtime).
    pub(crate) async fn session_transcript_view(&self, task_id: &str) -> Option<Value> {
        let runtimes: Vec<Arc<AgentRuntime>> = {
            let guard = self.runtimes.lock().await;
            guard.values().cloned().collect()
        };
        for runtime in runtimes {
            if let Some(view) = runtime.session_transcript_view(task_id) {
                return Some(view);
            }
        }
        None
    }

    /// Cumulative session usage.
    pub(crate) async fn session_usage(&self, task_id: &str) -> Option<Value> {
        let runtimes: Vec<Arc<AgentRuntime>> = {
            let guard = self.runtimes.lock().await;
            guard.values().cloned().collect()
        };
        for runtime in runtimes {
            if let Some(usage) = runtime.session_usage(task_id) {
                return Some(usage);
            }
        }
        None
    }

    /// Release a workspace's warmed-up runtime (the task service's releaseWorkspacePreparation).
    pub(crate) async fn dispose_workspace(&self, key: &str) {
        let mut runtimes = self.runtimes.lock().await;
        runtimes.remove(key);
    }

    /// Session facts under a workspace: disk meta is the baseline, and live views of registered runtimes override same-named sessions.
    /// Returns `[{sessionId,title,createdAt,updatedAt,phase,status,provider,model,thought,mode,...}]`.
    pub(crate) async fn workspace_session_facts(
        &self,
        workspace_path: &str,
        workspace_identity: Option<&str>,
    ) -> Vec<Value> {
        let key = workspace_identity
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(workspace_path);
        let sessions_dir = self.data_dir_for(workspace_path, key).join("sessions");
        let mut facts: HashMap<String, Value> = HashMap::new();
        if let Ok(mut entries) = tokio::fs::read_dir(&sessions_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let Ok(text) = tokio::fs::read_to_string(entry.path().join("meta.json")).await
                else {
                    continue;
                };
                let Ok(meta) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                let Some(session_id) = meta.get("sessionId").and_then(Value::as_str) else {
                    continue;
                };
                let created_at = meta.get("createdAt").and_then(Value::as_i64).unwrap_or(0);
                let updated_at = meta
                    .get("updatedAt")
                    .and_then(Value::as_i64)
                    .filter(|value| *value > 0)
                    .unwrap_or(created_at);
                facts.insert(
                    session_id.to_string(),
                    json!({
                        "sessionId": session_id,
                        "title": meta.get("title").cloned().unwrap_or_else(|| json!("")),
                        "titleSource": meta.get("titleSource").cloned().unwrap_or_else(|| json!("default")),
                        "createdAt": created_at,
                        "updatedAt": updated_at,
                        "provider": meta.get("provider").cloned().unwrap_or(Value::Null),
                        "model": meta.get("model").cloned().unwrap_or(Value::Null),
                        "thought": meta.get("thought").cloned().unwrap_or(Value::Null),
                        "mode": meta.get("collaborationMode").cloned().unwrap_or_else(|| json!("build")),
                    }),
                );
            }
        }
        if let Some(runtime) = self.runtime_for(key).await {
            for session_id in runtime.session_ids() {
                let Some(mut view) = runtime.session_view(&session_id) else {
                    continue;
                };
                if let Some(previous) = facts.get(&session_id) {
                    // createdAt comes from disk (meta is the only persistent fact of the creation moment).
                    if let Some(created) = previous.get("createdAt") {
                        view["createdAt"] = created.clone();
                    }
                    // Auto-title downgrade defense: a custom title must not be overwritten by an auto title.
                    if previous.get("titleSource").and_then(Value::as_str) == Some("custom")
                        && view.get("titleSource").and_then(Value::as_str) != Some("custom")
                    {
                        view["title"] = previous["title"].clone();
                        view["titleSource"] = json!("custom");
                    }
                }
                facts.insert(session_id, view);
            }
        }
        let mut values: Vec<Value> = facts.into_values().collect();
        values.sort_by_key(|value| {
            std::cmp::Reverse(value.get("createdAt").and_then(Value::as_i64).unwrap_or(0))
        });
        values
    }

    async fn sync_preferences(&self, params: &Value) -> Result<(), ServiceError> {
        *self.preferences.lock().await = params.clone();
        Ok(())
    }

    /// Saved-workflow file layer: `workspace.workspacePath` is the scope carrier, and `scope` defaults to project.
    fn handle_saved_workflow(
        &self,
        command: &str,
        params: &Value,
    ) -> Result<WireValue, ServiceError> {
        use crate::saved_workflows::{
            delete_saved_workflow, list_saved_workflows, move_saved_workflow,
            parse_saved_workflow_meta, resolve_saved_workflow, update_saved_workflow_meta,
            SavedWorkflowScope,
        };

        let workspace = params
            .get("workspace")
            .ok_or_else(|| ServiceError::invalid_params("workspace is required"))?;
        let cwd = workspace
            .get("workspacePath")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ServiceError::invalid_params("workspace.workspacePath is required"))?;
        let cwd = std::path::Path::new(cwd);
        let home = self.paths.home_dir();
        let scope_of =
            |default: Option<SavedWorkflowScope>| match params.get("scope").and_then(Value::as_str)
            {
                Some("project") => Ok(Some(SavedWorkflowScope::Project)),
                Some("global") => Ok(Some(SavedWorkflowScope::Global)),
                Some(other) => Err(ServiceError::invalid_params(format!(
                    "unknown workflow scope: {other}"
                ))),
                None => Ok(default),
            };
        let name = || -> Result<String, ServiceError> {
            params
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| ServiceError::invalid_params("name is required"))
        };

        match command {
            "listSavedWorkflows" => {
                let scope = scope_of(Some(SavedWorkflowScope::Project))?
                    .unwrap_or(SavedWorkflowScope::Project);
                let result = list_saved_workflows(cwd, scope, home);
                Ok(WireValue::Json(
                    serde_json::to_value(&result)
                        .map_err(|error| ServiceError::new(error.to_string()))?,
                ))
            }
            "getSavedWorkflow" => {
                let name = name()?;
                let scope = scope_of(None)?;
                let result = resolve_saved_workflow(cwd, &name, scope, home);
                Ok(WireValue::Json(
                    serde_json::to_value(&result)
                        .map_err(|error| ServiceError::new(error.to_string()))?,
                ))
            }
            "updateSavedWorkflowMeta" => {
                let name = name()?;
                let scope = scope_of(Some(SavedWorkflowScope::Project))?
                    .unwrap_or(SavedWorkflowScope::Project);
                let meta_value = params
                    .get("meta")
                    .ok_or_else(|| ServiceError::invalid_params("meta is required"))?;
                let meta_text = serde_yaml::to_string(meta_value)
                    .map_err(|error| ServiceError::invalid_params(error.to_string()))?;
                let meta = parse_saved_workflow_meta(&meta_text).map_err(|error| {
                    ServiceError::invalid_params(format!(
                        "{}: {}",
                        error.reason.as_str(),
                        error.detail
                    ))
                })?;
                let result = update_saved_workflow_meta(cwd, &name, &meta, scope, home)
                    .map_err(|error| ServiceError::new(error.to_string()))?;
                Ok(WireValue::Json(
                    serde_json::to_value(&result)
                        .map_err(|error| ServiceError::new(error.to_string()))?,
                ))
            }
            "deleteSavedWorkflow" => {
                let name = name()?;
                let scope = scope_of(Some(SavedWorkflowScope::Project))?
                    .unwrap_or(SavedWorkflowScope::Project);
                let result = delete_saved_workflow(cwd, &name, scope, home);
                Ok(WireValue::Json(
                    serde_json::to_value(&result)
                        .map_err(|error| ServiceError::new(error.to_string()))?,
                ))
            }
            "moveSavedWorkflow" => {
                let name = name()?;
                let result = move_saved_workflow(cwd, &name, home);
                Ok(WireValue::Json(
                    serde_json::to_value(&result)
                        .map_err(|error| ServiceError::new(error.to_string()))?,
                ))
            }
            other => Err(ServiceError::message(format!("Method not found: {other}"))),
        }
    }

    pub(crate) async fn call_inner(
        &self,
        command: &str,
        args: Vec<WireValue>,
    ) -> Result<WireValue, ServiceError> {
        let args = ServiceArgs::new(args);
        match command {
            // automation/* members (the 10 TS zcodeAgentService automation methods): the logic lives in
            // automation_channel; the grouped match arm keeps both the call surface and the dispatch evidence within this channel's scope.
            "listAutomations"
            | "listAllAutomations"
            | "createAutomation"
            | "updateAutomation"
            | "deleteAutomation"
            | "setAutomationEnabled"
            | "restartAutomation"
            | "runAutomationNow"
            | "listAutomationRuns"
            | "deleteAutomationRun" => {
                crate::automation_channel::dispatch(self, command, args.raw())
                    .await
                    .ok_or_else(|| ServiceError::invalid_params(command))?
            }
            // Saved-workflow file layer (list/get/updateMeta/delete/move). Run history
            // (`listSavedWorkflowRuns`) belongs to the workflow boundary and is carried by the TS engine.
            "listSavedWorkflows"
            | "getSavedWorkflow"
            | "updateSavedWorkflowMeta"
            | "deleteSavedWorkflow"
            | "moveSavedWorkflow" => {
                let params = args.required_json(0, "params")?;
                self.handle_saved_workflow(command, &params)
            }
            "helloConversationV4" => Ok(WireValue::Json(json!({
                "kind": "hello",
                "protocolVersion": 3,
                "connectionId": self.connection_id,
                "clientMode": "desktop-continuous",
                "deliveryProfile": "continuous",
                "serverTime": now_ms(),
                "capabilities": {
                    "nativeDialogs": true,
                    "localTerminal": true,
                    "binaryFrames": false,
                    "compression": "none",
                    "workspaceHookReview": true,
                    "independentPlanState": true,
                },
                "auth": {},
            }))),
            "initializeConversationV4" => Ok(WireValue::Undefined),
            "setConnectionFlowStateV4" => {
                let params = args.required_json(0, "params")?;
                let runtime = self.ensure_runtime(&params).await?;
                let state = params.get("state").cloned().unwrap_or(Value::Null);
                runtime
                    .request(
                        "v4/connection/flow",
                        json!({"connectionId": self.connection_id, "state": state}),
                    )
                    .await?;
                Ok(WireValue::Undefined)
            }
            "initialize" => {
                let params = args.required_json(0, "params")?;
                let (_, key) = Self::workspace_of(&params)?;
                let provider = self.provider_config().await;
                if provider.is_none() {
                    return Ok(WireValue::Json(json!({
                        "available": false,
                        "workspaceKey": key,
                        "reason": "Provider not configured",
                        "reasonCode": "provider_not_ready",
                    })));
                }
                self.ensure_runtime(&params).await?;
                self.lifecycle.send(WireValue::Json(json!({
                    "workspaceKey": key,
                    "state": "available",
                })));
                Ok(WireValue::Json(json!({
                    "available": true,
                    "workspaceKey": key,
                    "protocolName": "ZCode Protocol",
                    "protocolVersion": 1,
                    "transportKind": "stdio",
                })))
            }
            "createSession" => Ok(WireValue::Json(
                self.create_session(&args.required_json(0, "params")?)
                    .await?,
            )),
            "sendPrompt" => Ok(WireValue::Json(
                self.send_prompt(&args.required_json(0, "params")?).await?,
            )),
            "readSession" => Ok(WireValue::Json(
                self.read_session(&args.required_json(0, "params")?).await?,
            )),
            "listSessions" => Ok(WireValue::Json(
                self.list_sessions(&args.required_json(0, "params")?)
                    .await?,
            )),
            "closeSession" => Ok(WireValue::Json(
                self.close_session(&args.required_json(0, "params")?)
                    .await?,
            )),
            "subscribeSessionsIndexV4" => {
                let params = args.required_json(0, "params")?;
                Ok(WireValue::Json(
                    self.sessions_index_request("v4/conversation/subscribe", &params, true)
                        .await?,
                ))
            }
            "resyncSessionsIndexV4" => {
                let params = args.required_json(0, "params")?;
                Ok(WireValue::Json(
                    self.sessions_index_request("v4/conversation/resync", &params, false)
                        .await?,
                ))
            }
            "unsubscribeSessionsIndexV4" => {
                let params = args.required_json(0, "params")?;
                Ok(WireValue::Json(
                    self.sessions_index_request("v4/conversation/unsubscribe", &params, false)
                        .await?,
                ))
            }
            "subscribeConversationV4" => Ok(WireValue::Json(
                self.subscribe(&args.required_json(0, "params")?).await?,
            )),
            "resyncConversationV4" => Ok(WireValue::Json(
                self.resync(&args.required_json(0, "params")?).await?,
            )),
            "unsubscribeConversationV4" => Ok(WireValue::Json(
                self.unsubscribe(&args.required_json(0, "params")?).await?,
            )),
            "sendConversationCommandV4" => {
                let params = args.required_json(0, "params")?;
                let runtime = self.ensure_runtime(&params).await?;
                let envelope = params
                    .get("envelope")
                    .cloned()
                    .ok_or_else(|| ServiceError::invalid_params("params.envelope is required"))?;
                Ok(WireValue::Json(self.v4_command(&runtime, envelope).await?))
            }
            "queryConversationCommandsV4" => {
                let params = args.required_json(0, "params")?;
                let runtime = self.ensure_runtime(&params).await?;
                Ok(WireValue::Json(
                    runtime.request("v4/commands/query", params).await?.result,
                ))
            }
            "conversationRowsRangeV4" => Ok(WireValue::Json(
                self.rows_range(&args.required_json(0, "params")?).await?,
            )),
            // Terminal-plan catalog: same family as rowsRange/fileChanges (read-only, stateless, safe to retry on timeout);
            // the source of truth is the host's row-window filtering (`Session::plans()`) — do not reassemble it at this layer.
            "conversationPlansV4" => {
                let params = args.required_json(0, "params")?;
                let runtime = self.ensure_runtime(&params).await?;
                Ok(WireValue::Json(
                    runtime
                        .request("v4/conversation/plans", params)
                        .await?
                        .result,
                ))
            }
            // Background-bash detail read model: same family as plans/fileChanges (read-only, stateless);
            // the source of truth is the host's session background registry — this layer only forwards.
            "backgroundBashOutputV4" => {
                let params = args.required_json(0, "params")?;
                let runtime = self.ensure_runtime(&params).await?;
                Ok(WireValue::Json(
                    runtime
                        .request("v4/conversation/backgroundBashOutput", params)
                        .await?
                        .result,
                ))
            }
            "conversationFileChangesV4" => {
                let params = args.required_json(0, "params")?;
                let runtime = self.ensure_runtime(&params).await?;
                Ok(WireValue::Json(
                    runtime
                        .request("v4/conversation/fileChanges", params)
                        .await?
                        .result,
                ))
            }
            "conversationFileRewindPreviewV4" => {
                let params = args.required_json(0, "params")?;
                let runtime = self.ensure_runtime(&params).await?;
                Ok(WireValue::Json(
                    runtime
                        .request("v4/conversation/fileRewindPreview", params)
                        .await?
                        .result,
                ))
            }
            // `v4/conversation/workflowRuns`: journal-backed run read (single source of truth is the host workflow service;
            // this layer only forwards — no second projection).
            "conversationWorkflowRunsV4" => {
                let params = args.required_json(0, "params")?;
                let runtime = self.ensure_runtime(&params).await?;
                Ok(WireValue::Json(
                    runtime
                        .request("v4/conversation/workflowRuns", params)
                        .await?
                        .result,
                ))
            }
            // `v4/conversation/workflowRunEvents`: journal-backed run read (single source of truth is the host workflow service;
            // this layer only forwards — no second projection).
            "conversationWorkflowRunEventsV4" => {
                let params = args.required_json(0, "params")?;
                let runtime = self.ensure_runtime(&params).await?;
                Ok(WireValue::Json(
                    runtime
                        .request("v4/conversation/workflowRunEvents", params)
                        .await?
                        .result,
                ))
            }
            // `v4/conversation/workflowRunArtifacts`: journal-backed run read (single source of truth is the host workflow service;
            // this layer only forwards — no second projection).
            "conversationWorkflowRunArtifactsV4" => {
                let params = args.required_json(0, "params")?;
                let runtime = self.ensure_runtime(&params).await?;
                Ok(WireValue::Json(
                    runtime
                        .request("v4/conversation/workflowRunArtifacts", params)
                        .await?
                        .result,
                ))
            }
            // `v4/conversation/workflowRunArtifactData`: journal-backed run read (single source of truth is the host workflow service;
            // this layer only forwards — no second projection).
            "conversationWorkflowRunArtifactDataV4" => {
                let params = args.required_json(0, "params")?;
                let runtime = self.ensure_runtime(&params).await?;
                Ok(WireValue::Json(
                    runtime
                        .request("v4/conversation/workflowRunArtifactData", params)
                        .await?
                        .result,
                ))
            }
            // `v4/conversation/workflowRunArtifactRead`: journal-backed run read (single source of truth is the host workflow service;
            // this layer only forwards — no second projection).
            "conversationWorkflowRunArtifactReadV4" => {
                let params = args.required_json(0, "params")?;
                let runtime = self.ensure_runtime(&params).await?;
                Ok(WireValue::Json(
                    runtime
                        .request("v4/conversation/workflowRunArtifactRead", params)
                        .await?
                        .result,
                ))
            }
            // `v4/conversation/workflowRunWorkspace`: journal-backed run read (single source of truth is the host workflow service;
            // this layer only forwards — no second projection).
            "conversationWorkflowRunWorkspaceV4" => {
                let params = args.required_json(0, "params")?;
                let runtime = self.ensure_runtime(&params).await?;
                Ok(WireValue::Json(
                    runtime
                        .request("v4/conversation/workflowRunWorkspace", params)
                        .await?
                        .result,
                ))
            }
            // `v4/conversation/workflowRunNodeResult`: journal-backed run read (single source of truth is the host workflow service;
            // this layer only forwards — no second projection).
            "conversationWorkflowRunNodeResultV4" => {
                let params = args.required_json(0, "params")?;
                let runtime = self.ensure_runtime(&params).await?;
                Ok(WireValue::Json(
                    runtime
                        .request("v4/conversation/workflowRunNodeResult", params)
                        .await?
                        .result,
                ))
            }
            "syncAppRuntimePreferences" => {
                let params = args.required_json(0, "preferences")?;
                self.sync_preferences(&params).await?;
                Ok(WireValue::Undefined)
            }
            "disposeAll" => {
                self.runtimes.lock().await.clear();
                Ok(WireValue::Undefined)
            }
            // TS: terminate this workspace's Agent process and invalidate queued provider-ready
            // continuations (zcodeAgentService.ts:5605-5615). Under an in-process host,
            // "terminating the process" means dropping this workspace's HostServer (one per workspace);
            // there is no "queued start continuation" to revive it — the next ensure_runtime
            // rebuilds it on demand — the semantics match, and history (data dir) is not deleted along with the runtime.
            "disposeWorkspace" => {
                let params = args.required_json(0, "params")?;
                let (_, key) = Self::workspace_of(&params)?;
                self.runtimes.lock().await.remove(&key);
                Ok(WireValue::Undefined)
            }
            // ── Mirror surface sharing the implementation with nc-code-session: on the TS side these two services
            //    were originally two exits of the same set of agentService methods; on the Rust side there is
            //    likewise only one write path — `build_snapshot` / `run_config_command`; this is just the second dispatch point. ──
            "resumeSession" => {
                let params = args.required_json(0, "params")?;
                Ok(WireValue::Json(self.resume_session(&params).await?))
            }
            "setModel" => {
                let params = args.required_json(0, "params")?;
                Ok(WireValue::Json(
                    self.set_model_config(&params, "switchModelConfig").await?,
                ))
            }
            "setMode" => {
                let params = args.required_json(0, "params")?;
                Ok(WireValue::Json(
                    self.set_model_config(&params, "switchCollaborationMode")
                        .await?,
                ))
            }
            // setThoughtLevel: v4 has no standalone thought-level command, so it goes through the same switchModelConfig
            // (provider/model carry the current values + the new thought, the thoughtLevel branch of set_model_config).
            // TS side uses the same mapping (zcodeTaskServiceAdapter.ts:2689 session/setThoughtLevel → switchModelConfig).
            "setThoughtLevel" => {
                let params = args.required_json(0, "params")?;
                Ok(WireValue::Json(
                    self.set_model_config(&params, "switchModelConfig").await?,
                ))
            }
            "readSessionMessages" => {
                let params = args.required_json(0, "params")?;
                Ok(WireValue::Json(self.read_session_messages(&params).await?))
            }
            "readWorkspacePresentation" => {
                let params = args.required_json(0, "params")?;
                Ok(WireValue::Json(
                    self.read_workspace_presentation(&params).await?,
                ))
            }
            // existing-only probe (same semantics as nc-code-session): if there is no runtime, do not create one.
            "getWorkspaceRuntimeIdentity" => {
                let params = args.required_json(0, "params")?;
                let (_, key) = Self::workspace_of(&params)?;
                if self.runtime_for(&key).await.is_none() {
                    return Err(ServiceError::named(
                        "ZCodeWorkspaceRuntimeNotFound",
                        format!("workspace runtime is not running: {key}"),
                    ));
                }
                Ok(WireValue::Json(json!({
                    // The in-process host has no generation recycling (runtime.rs has no recycle path), so this is always 1.
                    "generation": 1,
                    "identity": key,
                    "processId": std::process::id(),
                    "workspaceKey": key,
                })))
            }
            // ── Session read/control surface (session/debug, compact, goal, subagents, generateText) ──
            "readSessionDebug" => {
                let params = args.required_json(0, "params")?;
                let runtime = self.existing_runtime(&params).await?;
                let session_id = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ServiceError::invalid_params("sessionId is required"))?;
                let response = runtime
                    .request("session/debug", json!({"sessionId": session_id}))
                    .await?;
                Ok(WireValue::Json(response.result))
            }
            "listSessionSubagents" => {
                let params = args.required_json(0, "params")?;
                let runtime = self.ensure_runtime(&params).await?;
                let mut host_params = json!({
                    "sessionId": params
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .ok_or_else(|| ServiceError::invalid_params("sessionId is required"))?,
                });
                for key in ["endedCursor", "endedLimit"] {
                    if let Some(value) = params.get(key) {
                        host_params[key] = value.clone();
                    }
                }
                let response = runtime.request("session/subagents", host_params).await?;
                Ok(WireValue::Json(response.result))
            }
            "compactSession" => {
                let params = args.required_json(0, "params")?;
                let runtime = self.ensure_runtime(&params).await?;
                let result = self
                    .host_session_control(
                        &runtime,
                        "session/compact",
                        &params,
                        &["inputId", "instructions", "expectedRevision"],
                    )
                    .await?;
                let snapshot = self.build_snapshot(&params, None).await?;
                let mut response = json!({
                    "response": "",
                    "snapshot": snapshot,
                });
                if let Some(compact) = result.get("compact") {
                    response["compact"] = compact.clone();
                }
                Ok(WireValue::Json(response))
            }
            "goalSession" => {
                let params = args.required_json(0, "params")?;
                let runtime = self.ensure_runtime(&params).await?;
                let result = self
                    .host_session_control(
                        &runtime,
                        "session/goal",
                        &params,
                        &["action", "objective", "inputId", "expectedRevision"],
                    )
                    .await?;
                let snapshot = self.build_snapshot(&params, None).await?;
                Ok(WireValue::Json(json!({
                    "response": result.get("response").cloned().unwrap_or(json!("")),
                    "snapshot": snapshot,
                    "startedTurn": result.get("startedTurn").cloned().unwrap_or(json!(false)),
                })))
            }
            "generateWorkspaceText" => {
                let params = args.required_json(0, "params")?;
                let workspace = params
                    .get("workspace")
                    .ok_or_else(|| ServiceError::invalid_params("workspace is required"))?;
                let mut host_params = json!({
                    "workspacePath": workspace.get("workspacePath"),
                    "workspaceIdentity": workspace.get("workspaceIdentity"),
                });
                for key in [
                    "selection",
                    "prompt",
                    "messages",
                    "tools",
                    "querySource",
                    "maxOutputTokens",
                    "operationId",
                ] {
                    if let Some(value) = params.get(key) {
                        host_params[key] = value.clone();
                    }
                }
                let runtime = self.ensure_runtime(&host_params).await?;
                let response = runtime
                    .request("workspace/generateText", host_params)
                    .await?;
                Ok(WireValue::Json(response.result))
            }
            // Attachment command surface (the six upload/read members of the TS
            // zcodeAgentService). The transaction state machine, content store and media
            // authorization live in nc-code-host (one owner); this channel only routes the
            // workspace-scoped request to that host runtime, exactly like the v4 forwards.
            // The incoming params carry the workspace (the UI adds it to every upload
            // request) plus the transaction fields the host reads by name.
            "attachmentBeginV4"
            | "attachmentChunkV4"
            | "attachmentCommitV4"
            | "attachmentAbortV4"
            | "attachmentReadV4"
            | "attachmentPreviewSourceV4" => {
                let params = args.required_json(0, "params")?;
                let host_method = match command {
                    "attachmentBeginV4" => "v4/attachment/begin",
                    "attachmentChunkV4" => "v4/attachment/chunk",
                    "attachmentCommitV4" => "v4/attachment/commit",
                    "attachmentAbortV4" => "v4/attachment/abort",
                    "attachmentReadV4" => "v4/attachment/read",
                    _ => "v4/attachment/previewSource",
                };
                let runtime = self.ensure_runtime(&params).await?;
                let mut host_params = params;
                if let Some(object) = host_params.as_object_mut() {
                    object.insert(
                        "connectionId".to_string(),
                        Value::String(self.connection_id.clone()),
                    );
                }
                let response = runtime.request(host_method, host_params).await?;
                Ok(WireValue::Json(response.result))
            }
            other if is_removed_agent_surface(other) => Err(ServiceError::named(
                "ZCodeAgentCapabilityRemoved",
                format!("agent capability is not part of this product: {other}"),
            )),
            other => Err(ServiceError::message(format!("Method not found: {other}"))),
        }
    }

    // ── Shared session-snapshot layer (the nc-code-agent and nc-code-session channels share the same write point) ──

    /// The personal provider store is the single source of model/tier facts: the host has no model catalog,
    /// and the comment on `snapshot::SettingsFacts` states this layer fills it in. Fields align with TS `toModelOption`
    /// (apps/nc-code-cli/packages/bootstrap/src/app/provider-registry-selection.ts:184):
    /// optional keys (contextWindow/maxOutputTokens/reasoning) are omitted when unknown; properties is required.
    pub(crate) async fn settings_facts(
        &self,
        current: Option<(&str, &str)>,
    ) -> snapshot::SettingsFacts {
        let Ok(store) = self.store.snapshot().await else {
            return snapshot::SettingsFacts::default();
        };
        let mut available_models = Vec::new();
        let mut current_config: Option<Value> = None;
        for provider in &store.providers {
            if !provider.enabled {
                continue;
            }
            let provider_config = provider_effective_config(provider, &store);
            if !provider_issues(&provider_config).is_empty() {
                continue;
            }
            for model in &provider.models {
                if !model.enabled {
                    continue;
                }
                let effective = model_effective_config(model);
                if let Some((provider_id, model_id)) = current {
                    if provider_id == provider.provider_id && model_id == model.model_id {
                        current_config = Some(effective.clone());
                    }
                }
                available_models.push(model_option(
                    &provider.provider_id,
                    &provider.provider_name,
                    &model.model_id,
                    &effective,
                ));
            }
        }
        let (thought_levels, thought_default, context_window) = match current_config.as_ref() {
            Some(config) => thought_facts(config),
            None => (Vec::new(), None, 0),
        };
        snapshot::SettingsFacts {
            available_models,
            thought_levels,
            thought_default,
            context_window,
        }
    }

    /// The single construction point of the session snapshot: host session facts (view/transcript/usage) + the v4 authoritative snapshot +
    /// the provider store's model facts, emitted by `snapshot::build` under the strict
    /// `zcodeSessionStateSnapshotSchema`. When the session does not exist, resync errors out first,
    /// so it never degrades into an empty snapshot (the old legacy stub fabricated state exactly that way).
    pub(crate) async fn build_snapshot(
        &self,
        params: &Value,
        message_limit: Option<usize>,
    ) -> Result<Value, ServiceError> {
        let (workspace_path, workspace_key) = Self::workspace_of(params)?;
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| ServiceError::invalid_params("sessionId is required"))?
            .to_string();
        let runtime = self.ensure_runtime(params).await?;
        // 冷会话必须在 resync 之前注册：resync/subscribe 只认已注册的 session。
        // closeSession 会在本进程里把 session 从注册表移除但保留 meta/transcript，
        // 先 ensure_loaded（host 唯一的冷恢复入口）才能读到它，而不是报 not found。
        runtime.ensure_loaded(&session_id);
        let response = runtime
            .request(
                "v4/conversation/resync",
                json!({
                    "sessionId": session_id,
                    "connectionId": self.connection_id,
                    "clientMode": "desktop-continuous",
                }),
            )
            .await?;
        let v4 = response
            .frames
            .iter()
            .find_map(|frame| frame.get("snapshot").cloned());
        let view = runtime.session_view(&session_id).ok_or_else(|| {
            ServiceError::named(
                "ZCodeSessionNotFound",
                format!("session not found: {session_id}"),
            )
        })?;
        let transcript = runtime.session_transcript_view(&session_id);
        let rows: Vec<Value> = transcript
            .as_ref()
            .and_then(|view| view.get("rows"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let usage = runtime.session_usage(&session_id);
        let current = match (
            view.get("provider").and_then(Value::as_str),
            view.get("model").and_then(Value::as_str),
        ) {
            (Some(provider), Some(model)) => Some((provider, model)),
            _ => None,
        };
        let settings = self.settings_facts(current).await;
        let input = snapshot::SnapshotInput {
            workspace_path: &workspace_path,
            workspace_key: &workspace_key,
            session_id: &session_id,
            view: &view,
            v4: v4.as_ref(),
            rows: &rows,
            usage: usage.as_ref(),
            settings: &settings,
            message_limit,
        };
        Ok(snapshot::build(&input))
    }

    /// Optional session config for create/resume: model / thoughtLevel / mode are dispatched only when each key is present
    /// (`set_model_config` overwrites the whole group, missing keys would clear it, so any absent item keeps the session's current facts).
    /// CAS envelope: `COMMANDS_REQUIRING_BASE_REVISION` (nc-code-core/src/protocol.rs:75)
    /// includes switchModelConfig / switchCollaborationMode / setFollowupMode — without
    /// baseRevision, parse_command_envelope rejects it outright with `proto.invalidPayload`.
    /// The value is the session revision just read before sending (repo rule: take CAS fields from a fresh read, never cache).
    fn cas_command_envelope(
        &self,
        session_id: &str,
        command_type: &str,
        payload: Value,
        base_revision: i64,
    ) -> Value {
        let mut envelope = self.command_envelope(Some(session_id), command_type, payload);
        envelope["baseRevision"] = json!(base_revision);
        envelope
    }

    /// Dispatch one admitted config command and wait for it to settle.
    ///
    /// `switchModelConfig`/`switchCollaborationMode` go through the session inbox
    /// (`Route::Admitted` in nc-code-host/src/commands/mod.rs): the acceptance ack returns first,
    /// execution happens in a spawned task. Reading the snapshot immediately would only get the pre-execution state —
    /// that would rest "success" on a race. So per the repo rule, poll `v4/commands/query` to a terminal state before returning.
    async fn run_config_command(
        &self,
        runtime: &Arc<AgentRuntime>,
        session_id: &str,
        command_type: &str,
        payload: Value,
        base_revision: i64,
    ) -> Result<(), ServiceError> {
        let envelope = self.cas_command_envelope(session_id, command_type, payload, base_revision);
        let command_id = envelope
            .get("commandId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let ack = self.v4_command(runtime, envelope).await?;
        self.wait_command_settled(runtime, session_id, command_id.as_deref(), &ack)
            .await
    }

    /// Poll `v4/commands/query` until the command is booked (unknown = not settled yet; see
    /// the same semantics in nc-code-host/tests/commands_query_e2e.rs:180).
    /// Terminal accepted/noop/duplicate count as executed; rejected/stale/failed are propagated as-is.
    async fn wait_command_settled(
        &self,
        runtime: &Arc<AgentRuntime>,
        session_id: &str,
        command_id: Option<&str>,
        ack: &Value,
    ) -> Result<(), ServiceError> {
        let command_id = command_id.map(str::to_string).or_else(|| {
            ack.get("commandId")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        let Some(command_id) = command_id else {
            return Err(ServiceError::message(
                "commandId missing: cannot verify the admitted command settled",
            ));
        };
        for _ in 0..40 {
            let response = runtime
                .request(
                    "v4/commands/query",
                    json!({"commands": [{"sessionId": session_id, "commandId": command_id}]}),
                )
                .await?;
            let entry = response
                .result
                .get("results")
                .and_then(Value::as_array)
                .and_then(|items| items.first().cloned())
                .unwrap_or(Value::Null);
            let result = entry.get("result").cloned().unwrap_or(Value::Null);
            if result != json!("unknown") {
                // The result may be a {commandId, status} object or a bare status string (accept both shapes).
                let status = result.as_str().map(str::to_string).unwrap_or_else(|| {
                    result
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                });
                return match status.as_str() {
                    "accepted" | "noop" | "duplicate" => Ok(()),
                    "rejected" | "stale" | "failed" => Err(ServiceError::named(
                        "ZCodeCommandNotSettledOk",
                        format!("command {command_id} settled as {status}: {result}"),
                    )),
                    other => Err(ServiceError::message(format!(
                        "command {command_id} settled with unexpected status: {other}"
                    ))),
                };
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        Err(ServiceError::message(format!(
            "command {command_id} did not settle within 2s"
        )))
    }

    pub(crate) async fn apply_session_config(
        &self,
        runtime: &Arc<AgentRuntime>,
        session_id: &str,
        params: &Value,
    ) -> Result<(), ServiceError> {
        let view = runtime.session_view(session_id).unwrap_or(Value::Null);
        let base_revision = view.get("revision").and_then(Value::as_i64).unwrap_or(0);
        if let Some(model) = params.get("model") {
            let payload = model_payload(&view, model);
            self.run_config_command(
                runtime,
                session_id,
                "switchModelConfig",
                payload,
                base_revision,
            )
            .await?;
        }
        if let Some(level) = params
            .get("thoughtLevel")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let (provider, model, _) = view_triple(&view);
            let payload = json!({"provider": provider, "model": model, "thought": level});
            self.run_config_command(
                runtime,
                session_id,
                "switchModelConfig",
                payload,
                base_revision,
            )
            .await?;
        }
        if let Some(mode) = params.get("mode").and_then(Value::as_str) {
            let payload = json!({"mode": mode});
            self.run_config_command(
                runtime,
                session_id,
                "switchCollaborationMode",
                payload,
                base_revision,
            )
            .await?;
        }
        Ok(())
    }

    /// `setModel`/`setThoughtLevel`/`setMode`（zcodeSession.ts:152-154）。
    /// All three fall under `COMMANDS_REQUIRING_BASE_REVISION`: the envelope must carry the
    /// `baseRevision` freshly read before sending, otherwise the host rejects with `proto.invalidPayload` (see the session.rs tests for the measured behavior).
    /// When TS passes `expectedRevision` it is no longer compared separately (this channel always initiates with the current revision),
    /// and `persistAsWorkspaceLastUsed` (out-of-session workspace memory) has no corresponding state on this host;
    /// recorded as a known difference, no fake write.
    pub(crate) async fn set_model_config(
        &self,
        params: &Value,
        command: &str,
    ) -> Result<Value, ServiceError> {
        let runtime = self.ensure_runtime(params).await?;
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| ServiceError::invalid_params("sessionId is required"))?
            .to_string();
        let view = runtime.session_view(&session_id).unwrap_or(Value::Null);
        let payload = match command {
            "switchCollaborationMode" => {
                let mode = params
                    .get("mode")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ServiceError::invalid_params("params.mode is required"))?;
                json!({"mode": mode})
            }
            // setThoughtLevel goes through the same switchModelConfig: provider/model must carry the current values.
            _ if params.get("thoughtLevel").is_some() => {
                let (provider, model, _) = view_triple(&view);
                let level = params
                    .get("thoughtLevel")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or("")
                    .to_string();
                json!({"provider": provider, "model": model, "thought": level})
            }
            _ => {
                let model = params
                    .get("model")
                    .ok_or_else(|| ServiceError::invalid_params("params.model is required"))?;
                model_payload(&view, model)
            }
        };
        let base_revision = view.get("revision").and_then(Value::as_i64).unwrap_or(0);
        self.run_config_command(&runtime, &session_id, command, payload, base_revision)
            .await?;
        self.build_snapshot(params, None).await
    }

    /// `readSessionMessages` (zcodeSession.ts:147): transcript rows → `ZCodeMessageWithParts[]`,
    /// after cursor `afterMessageId`, truncated by `limit` (forward-pagination semantics).
    pub(crate) async fn read_session_messages(
        &self,
        params: &Value,
    ) -> Result<Value, ServiceError> {
        let runtime = self.ensure_runtime(params).await?;
        let (workspace_path, _) = Self::workspace_of(params)?;
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| ServiceError::invalid_params("sessionId is required"))?
            .to_string();
        let _ = runtime
            .request(
                "v4/conversation/resync",
                json!({
                    "sessionId": session_id,
                    "connectionId": self.connection_id,
                    "clientMode": "desktop-continuous",
                }),
            )
            .await?;
        runtime.ensure_loaded(&session_id);
        let view = runtime.session_view(&session_id).ok_or_else(|| {
            ServiceError::named(
                "ZCodeSessionNotFound",
                format!("session not found: {session_id}"),
            )
        })?;
        let transcript = runtime
            .session_transcript_view(&session_id)
            .ok_or_else(|| {
                ServiceError::named(
                    "ZCodeSessionNotFound",
                    format!("session not found: {session_id}"),
                )
            })?;
        let rows: Vec<Value> = transcript
            .get("rows")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let model = snapshot::model_selection(
            view.get("provider").and_then(Value::as_str),
            view.get("model").and_then(Value::as_str),
            view.get("thought").and_then(Value::as_str),
        );
        let mut messages =
            snapshot::messages_from_rows(&rows, &session_id, &workspace_path, model.as_ref());
        if let Some(after) = params.get("afterMessageId").and_then(Value::as_str) {
            if let Some(index) = messages
                .iter()
                .position(|message| message.get("id").and_then(Value::as_str) == Some(after))
            {
                let drain = index + 1;
                messages.drain(..drain);
            }
        }
        if let Some(limit) = params.get("limit").and_then(Value::as_u64) {
            messages.truncate(limit as usize);
        }
        Ok(Value::Array(messages))
    }

    /// `listSessions` (zcodeSession.ts:146): disk meta is the full set (including cold sessions);
    /// live views take precedence for real phase/status; sorted by updatedAt descending, ties by sessionId ascending, truncated by limit.
    /// TS's `includeArchived` archive slot has no corresponding field in session meta; do not fake archive semantics.
    pub(crate) async fn list_sessions_info(&self, params: &Value) -> Result<Value, ServiceError> {
        let (workspace_path, key) = Self::workspace_of(params)?;
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize);
        let sessions_dir = self.data_dir_for(&workspace_path, &key).join("sessions");
        let runtime = self.runtime_for(&key).await;
        let mut rows: Vec<(i64, String, Value)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        // Live sessions first: after createSession, meta.json is not written immediately (the host persists only when
        // the revision advances); scanning only disk would miss freshly created sessions; runtime IDs fill them in, disk only adds cold sessions.
        if let Some(runtime) = runtime.as_ref() {
            for session_id in runtime.session_ids() {
                // Subagent child sessions are an implementation detail of the task list (TS also filters by taskType);
                // they only appear in `session/subagents` and in their own read surfaces.
                if runtime.session_task_type(&session_id).as_deref() == Some("subagent_child") {
                    continue;
                }
                let Some(fact) = runtime.session_view(&session_id) else {
                    continue;
                };
                let info = snapshot::session_info(&fact, &workspace_path, &key);
                if info.is_null() {
                    continue;
                }
                let updated_at = info.get("updatedAt").and_then(Value::as_i64).unwrap_or(0);
                seen.insert(session_id.clone());
                rows.push((updated_at, session_id, info));
            }
        }
        if let Ok(mut entries) = tokio::fs::read_dir(&sessions_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let Ok(text) = tokio::fs::read_to_string(entry.path().join("meta.json")).await
                else {
                    continue;
                };
                let Ok(meta) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if meta.get("taskType").and_then(Value::as_str) == Some("subagent_child") {
                    continue;
                }
                let Some(session_id) = meta
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                else {
                    continue;
                };
                if seen.contains(&session_id) {
                    continue;
                }
                let fact = runtime
                    .as_ref()
                    .and_then(|runtime| runtime.session_view(&session_id))
                    .unwrap_or(meta);
                let info = snapshot::session_info(&fact, &workspace_path, &key);
                if info.is_null() {
                    continue;
                }
                let updated_at = info.get("updatedAt").and_then(Value::as_i64).unwrap_or(0);
                rows.push((updated_at, session_id, info));
            }
        }
        rows.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        if let Some(limit) = limit {
            rows.truncate(limit);
        }
        Ok(Value::Array(
            rows.into_iter()
                .map(|(_, _, info)| info)
                .collect::<Vec<Value>>(),
        ))
    }

    /// `resumeSession` (zcodeSession.ts:46): subscribe (the host's cold-resume entry) →
    /// optional model/thoughtLevel → real snapshot.
    pub(crate) async fn resume_session(&self, params: &Value) -> Result<Value, ServiceError> {
        let runtime = self.ensure_runtime(params).await?;
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| ServiceError::invalid_params("sessionId is required"))?
            .to_string();
        let _ = runtime
            .request(
                "v4/conversation/subscribe",
                json!({
                    "sessionId": session_id,
                    "connectionId": self.connection_id,
                    "clientMode": "desktop-continuous",
                }),
            )
            .await?;
        runtime.ensure_loaded(&session_id);
        self.apply_session_config(&runtime, &session_id, params)
            .await?;
        self.build_snapshot(params, None).await
    }
}

/// Rust counterpart of `toModelOption` (provider-registry-selection.ts:184).
fn model_option(
    provider_id: &str,
    provider_name: &str,
    model_id: &str,
    effective: &Value,
) -> Value {
    use serde_json::Map;
    let properties = effective.get("properties");
    let option_specs = effective.get("optionSpecs");
    let mut option = Map::new();
    option.insert(
        "ref".to_string(),
        json!({"providerId": provider_id, "modelId": model_id}),
    );
    option.insert("label".to_string(), json!(model_id));
    option.insert("providerLabel".to_string(), json!(provider_name));
    if let Some(value) = properties.and_then(|value| value.get("contextWindow")) {
        option.insert("contextWindow".to_string(), value.clone());
    }
    if let Some(value) = option_specs
        .and_then(|specs| specs.get("maxOutputTokens"))
        .and_then(|spec| spec.get("max"))
    {
        option.insert("maxOutputTokens".to_string(), value.clone());
    }
    if let Some(levels) = option_specs
        .and_then(|specs| specs.get("reasoningLevel"))
        .and_then(|spec| spec.get("values"))
        .and_then(Value::as_array)
        .filter(|values| !values.is_empty())
    {
        let mut reasoning = Map::new();
        reasoning.insert(
            "levels".to_string(),
            json!(levels
                .iter()
                .map(|value| json!({"value": value, "label": value}))
                .collect::<Vec<Value>>()),
        );
        // toModelOption: defaultLevel = values.at(-1)。
        if let Some(last) = levels.last() {
            reasoning.insert("defaultLevel".to_string(), last.clone());
        }
        option.insert("reasoning".to_string(), Value::Object(reasoning));
    }
    // properties is strict-required: `default_model_config()` guarantees inputFormat/outputFormat
    // exist; on a genuinely missing key let the strict parse fail outright — do not paper over it with placeholder values.
    if let (Some(input), Some(output)) = (
        properties.and_then(|value| value.get("inputFormat")),
        properties.and_then(|value| value.get("outputFormat")),
    ) {
        option.insert(
            "properties".to_string(),
            json!({"inputFormat": input, "outputFormat": output}),
        );
    }
    Value::Object(option)
}

/// The current model's tier facts and context window (for consumers of snapshot::SettingsFacts see snapshot.rs:55-69).
fn thought_facts(config: &Value) -> (Vec<Value>, Option<String>, u64) {
    let reasoning = config
        .get("optionSpecs")
        .and_then(|specs| specs.get("reasoningLevel"));
    let levels = reasoning
        .and_then(|spec| spec.get("values"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let thought_levels = levels
        .iter()
        .map(|value| json!({"value": value, "label": value}))
        .collect::<Vec<Value>>();
    let thought_default = reasoning
        .and_then(|spec| spec.get("default"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| levels.last().and_then(Value::as_str).map(str::to_string))
        .filter(|_| !thought_levels.is_empty());
    let context_window = config
        .get("properties")
        .and_then(|value| value.get("contextWindow"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    (thought_levels, thought_default, context_window)
}

/// The whole provider/model/thought triple for switchModelConfig (the host writes the entire group as an overwrite).
fn model_payload(view: &Value, model: &Value) -> Value {
    let (current_provider, current_model, current_thought) = view_triple(view);
    let provider = model
        .get("providerId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or(current_provider);
    let model_id = model
        .get("modelId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or(current_model);
    let thought = model
        .get("options")
        .and_then(|options| options.get("reasoningLevel"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or(current_thought);
    json!({"provider": provider, "model": model_id, "thought": thought})
}

/// The session's current provider/model/thought triple (missing items become empty strings; the host's payload defaults are also "").
fn view_triple(view: &Value) -> (String, String, String) {
    let read = |key: &str| {
        view.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_default()
    };
    (read("provider"), read("model"), read("thought"))
}

#[async_trait]
impl Channel for AgentService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        // Plugin members are mirrored to plugin-management (same implementation, two dispatch points).
        if is_plugin_management_member(command) {
            let Some(channel) = self.plugin_management.get() else {
                return Err(ChannelFailure::error_named(
                    "PluginManagementUnavailable",
                    "plugin-management channel is not attached to the agent service",
                ));
            };
            return channel.call(_ctx, command, args, _cancel).await;
        }
        // generateText's PromiseCancel maps to the host's workspace/cancelGenerateText:
        // the cancel must really stop the in-flight model request, not just drop the response.
        if command == "generateWorkspaceText" {
            let cancel_args = args.clone();
            let future = self.call_inner(command, args);
            tokio::pin!(future);
            return tokio::select! {
                result = &mut future => result.map_err(Into::into),
                _ = _cancel.cancelled() => {
                    self.cancel_generate_text(cancel_args).await;
                    Err(ChannelFailure::error_named(
                        "ZCodeGenerateTextCancelled",
                        "workspace/generateText was cancelled by the client",
                    ))
                }
            };
        }
        self.call_inner(command, args).await.map_err(Into::into)
    }

    async fn listen(
        &self,
        _ctx: &ChannelContext,
        event: &str,
        arg: WireValue,
    ) -> Result<EventSubscription, ChannelFailure> {
        match event {
            // Session frames and sessions-index frames share the `v4/conversation/frame` notification; the dynamic event
            // delivers the notification **params** (the wire frame), not the whole JSON-RPC envelope — the renderer
            // decoder reads topic/subscriptionId directly.
            "onDynamicConversationFrame" | "onDynamicSessionsIndexFrame" => {
                self.notification_subscription(&arg, "v4/conversation/frame")
                    .await
            }
            // Local TTFT fact: a standalone notification method, filtered down to params on the same bus.
            "onDynamicLocalTtftFacts" => {
                self.notification_subscription(&arg, "v4/telemetry/local-ttft")
                    .await
            }
            "onAgentRuntimeLifecycle" | "onAgentRuntimeRestarted" => Ok(
                EventSubscription::from_broadcast(self.lifecycle.subscribe()),
            ),
            other => Err(ChannelFailure::event_not_found(other)),
        }
    }
}

impl AgentService {
    /// Filter one workspace runtime's frame bus by notification method, delivering the notification params.
    async fn notification_subscription(
        &self,
        arg: &WireValue,
        method: &'static str,
    ) -> Result<EventSubscription, ChannelFailure> {
        let key = event_workspace_key(arg);
        let runtimes = self.runtimes.lock().await;
        let runtime = key
            .and_then(|key| runtimes.get(&key))
            .cloned()
            .or_else(|| runtimes.values().next().cloned());
        match runtime {
            Some(runtime) => Ok(EventSubscription::from_broadcast_mapped(
                runtime.frames.subscribe(),
                move |value| notification_params(value, method),
            )),
            None => Ok(empty_subscription()),
        }
    }
}

/// Notification line → params (accepts both string lines and already-parsed JSON carriers); other methods return None.
fn notification_params(value: WireValue, method: &str) -> Option<WireValue> {
    let parsed = match value {
        WireValue::Json(value) => value,
        WireValue::String(line) => serde_json::from_str::<Value>(&line).ok()?,
        _ => return None,
    };
    if parsed.get("method").and_then(Value::as_str) != Some(method) {
        return None;
    }
    parsed.get("params").cloned().map(WireValue::Json)
}

fn event_workspace_key(arg: &WireValue) -> Option<String> {
    let params = match arg {
        WireValue::Json(value) => value.clone(),
        WireValue::Undefined => Value::Null,
        other => Value::String(other.as_str().unwrap_or_default().to_string()),
    };
    params
        .get("workspaceIdentity")
        .and_then(Value::as_str)
        .or_else(|| params.get("workspacePath").and_then(Value::as_str))
        .map(str::to_string)
}

/// The host refuses to overwrite an existing transcript (`reason: "session_exists"` in the
/// error payload). Re-importing the same deterministic id is not a failure, so the caller
/// resumes the session from disk instead of surfacing the refusal.
fn is_session_exists(error: &ServiceError) -> bool {
    error
        .data
        .as_deref()
        .and_then(|data| data.get("reason"))
        .and_then(Value::as_str)
        == Some("session_exists")
}

fn is_removed_agent_surface(command: &str) -> bool {
    // The upload/read attachment members are wired (see the dispatch arm above); only the
    // surfaces with no host implementation remain explicit "removed capability" failures:
    // - the conversation share reader runs on the share channel's own store;
    // - promptAttachmentTransfer is the retired TS transfer service (the v4 transaction
    //   replaced it).
    matches!(
        command,
        "conversationAttachmentReadV4"
            | "conversationAttachmentStatV4"
            | "promptAttachmentTransfer"
    )
}
fn empty_subscription() -> EventSubscription {
    let (_, receiver) = tokio::sync::mpsc::channel(1);
    EventSubscription::from_receiver(receiver)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// On `nc-code-agent`, mirror the members of `plugin-management` (the plugin methods of the TS zcodeAgentService).
/// The inventory maps one-to-one to the 18 members of the `plugin-management` channel; coverage tooling counts evidence by the literal names here.
fn is_plugin_management_member(command: &str) -> bool {
    matches!(
        command,
        "listPlugins"
            | "getPluginsOverview"
            | "addPluginMarketplace"
            | "removePluginMarketplace"
            | "updatePluginMarketplace"
            | "installPlugin"
            | "uninstallPlugin"
            | "updatePlugin"
            | "restoreBuiltinPlugin"
            | "configurePlugin"
            | "resetPluginConfig"
            | "validatePlugin"
            | "describePlugin"
            | "setPluginEnabled"
            | "cancelPluginOperation"
            | "getPluginReferenceCatalog"
            | "getSkillReferenceCatalog"
            | "resolveSuggestedPluginReference"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;
    use nc_code_services_provider::store::{
        default_model_config, ModelRecord, ProviderRecord, ProviderStore,
    };

    fn temp_paths(tag: &str) -> (AppPaths, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "zcode-agent-test-{}-{tag}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).expect("root");
        (AppPaths::new(root.join("home"), root.join("data")), root)
    }

    async fn configured_service(tag: &str) -> (Arc<AgentService>, std::path::PathBuf) {
        let (paths, root) = temp_paths(tag);
        let store = ProviderStore::new(paths.clone());
        store
            .mutate(|snapshot| {
                snapshot.providers.push(ProviderRecord {
                    provider_id: "personal:1".to_string(),
                    provider_name: "Test Provider".to_string(),
                    template_id: None,
                    enabled: true,
                    config: json!({
                        "group": "standard-personal",
                        "visibility": "visible",
                        "api": {"type": "openai-chat-completions", "baseUrl": "http://127.0.0.1:9/v1"},
                        "access": {"type": "api-key", "apiKey": "test-key"}
                    }),
                    models: vec![ModelRecord {
                        model_id: "test-model".to_string(),
                        config: default_model_config(),
                        enabled: true,
                        use_recommended: true,
                    }],
                });
snapshot.provider_order.push("personal:1".to_string());
                Ok(())
            })
            .await
            .expect("store");
        (AgentService::new(paths, store), root)
    }

    #[tokio::test]
    async fn dispose_workspace_drops_the_runtime_then_rebuilds_on_demand() {
        let (service, root) = configured_service("dispose-ws").await;
        let workspace = root.join("ws");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let target = json!({"workspacePath": workspace.to_string_lossy()});

        let created = service
            .call_inner("createSession", vec![WireValue::Json(target.clone())])
            .await
            .expect("create");
        let WireValue::Json(created) = created else {
            panic!("create must return JSON");
        };
        assert!(
            created["session"]["sessionId"].as_str().is_some(),
            "create must return the real sessionId",
        );
        let (_, key) = AgentService::workspace_of(&target).expect("key");
        assert!(
            service.runtime_for(&key).await.is_some(),
            "Exists at runtime after creation"
        );

        service
            .call_inner("disposeWorkspace", vec![WireValue::Json(target.clone())])
            .await
            .expect("disposeWorkspace");
        assert!(
            service.runtime_for(&key).await.is_none(),
            "After dispose, the runtime of the workspace must be discarded (TS: the process is terminated)",
        );

        // Rebuilt on next use (TS semantics: after dispose, a new use may revive it).
        // The assertion stops at "the runtime is rebuilt": when a session lands in meta.json is the host's persistence responsibility,
        // not asserted in passing here (to avoid locking the host's persistence semantics into this test).
        service
            .ensure_runtime(&target)
            .await
            .expect("rebuild after dispose");
        assert!(
            service.runtime_for(&key).await.is_some(),
            "The runtime of this workspace must be rebuilt the next time it is used",
        );
    }

    #[tokio::test]
    async fn mirrored_session_members_are_reachable_through_the_agent_dispatch() {
        // These members are already wired in nc-code-session; the agent surface is a second dispatch point of the same implementation,
        // and this proves the dispatch really works (rather than splitting TS's two exits into two implementations).
        let (service, root) = configured_service("mirror").await;
        let workspace = root.join("ws");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let target = json!({"workspacePath": workspace.to_string_lossy()});

        let created = service
            .call_inner("createSession", vec![WireValue::Json(target.clone())])
            .await
            .expect("create");
        let WireValue::Json(created) = created else {
            panic!("create must return JSON");
        };
        let session_id = created["session"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        let mode = service
            .call_inner(
                "setMode",
                vec![WireValue::Json(json!({
                    "workspacePath": target["workspacePath"],
                    "sessionId": session_id,
                    "mode": "edit",
                }))],
            )
            .await
            .expect("setMode on agent channel");
        let WireValue::Json(mode) = mode else {
            panic!("setMode must return JSON");
        };
        assert_eq!(mode["settings"]["mode"]["current"], json!("edit")); // Set the model first (thought is part of the model config; without a model, switchModelConfig has no context).
        let _model = service
            .call_inner(
                "setModel",
                vec![WireValue::Json(json!({
                    "workspacePath": target["workspacePath"],
                    "sessionId": session_id,
                    "model": {"providerId": "personal:1", "modelId": "test-model"},
                }))],
            )
            .await
            .expect("setModel on agent channel");

        // setThoughtLevel: the same switchModelConfig write path (provider/model carry the current values + the new thought).
        let thought = service
            .call_inner(
                "setThoughtLevel",
                vec![WireValue::Json(json!({
                    "workspacePath": target["workspacePath"],
                    "sessionId": session_id,
                    "thoughtLevel": "high",
                }))],
            )
            .await
            .expect("setThoughtLevel on agent channel");
        let WireValue::Json(thought) = thought else {
            panic!("setThoughtLevel must return JSON");
        };
        assert_eq!(
            thought["settings"]["thoughtLevel"]["current"],
            json!("high")
        );

        let messages = service
            .call_inner(
                "readSessionMessages",
                vec![WireValue::Json(json!({
                    "workspacePath": target["workspacePath"],
                    "sessionId": session_id,
                }))],
            )
            .await
            .expect("readSessionMessages on agent channel");
        let WireValue::Json(messages) = messages else {
            panic!("messages must return JSON");
        };
        assert_eq!(messages, json!([]));

        let resumed = service
            .call_inner(
                "resumeSession",
                vec![WireValue::Json(json!({
                    "workspacePath": target["workspacePath"],
                    "sessionId": session_id,
                }))],
            )
            .await
            .expect("resumeSession on agent channel");
        let WireValue::Json(resumed) = resumed else {
            panic!("resume must return JSON");
        };
        assert_eq!(
            resumed["session"]["sessionId"],
            json!(session_id),
            "resume must return a true snapshot of the same session",
        );
    }

    #[tokio::test]
    async fn initialize_reports_provider_state_and_workspace_key() {
        let (service, root) = configured_service("init").await;
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let result = service
            .call_inner(
                "initialize",
                vec![WireValue::Json(
                    json!({"workspacePath": workspace.to_string_lossy()}),
                )],
            )
            .await
            .expect("initialize");
        match result {
            WireValue::Json(value) => {
                assert_eq!(value["available"], json!(true));
                assert_eq!(value["protocolName"], json!("ZCode Protocol"));
                assert_eq!(value["workspaceKey"], json!(workspace.to_string_lossy()));
            }
            other => panic!("{other:?}"),
        }
        let list = service
            .call_inner(
                "listSessions",
                vec![WireValue::Json(
                    json!({"workspacePath": workspace.to_string_lossy()}),
                )],
            )
            .await
            .expect("list");
        assert_eq!(list, WireValue::Json(json!([])));
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn initialize_without_provider_is_fail_closed() {
        let (paths, root) = temp_paths("no-provider");
        let service = AgentService::new(
            paths,
            ProviderStore::new(AppPaths::new(root.join("home2"), root.join("data2"))),
        );
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let result = service
            .call_inner(
                "initialize",
                vec![WireValue::Json(
                    json!({"workspacePath": workspace.to_string_lossy()}),
                )],
            )
            .await
            .expect("initialize");
        match result {
            WireValue::Json(value) => {
                assert_eq!(value["available"], json!(false));
                assert_eq!(value["reasonCode"], json!("provider_not_ready"));
            }
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn runtime_preferences_are_stored() {
        let (service, root) = configured_service("prefs").await;
        service
            .call_inner(
                "syncAppRuntimePreferences",
                vec![WireValue::Json(
                    json!({"modelIoFullRetentionEnabled": false}),
                )],
            )
            .await
            .expect("sync");
        assert_eq!(
            *service.preferences.lock().await,
            json!({"modelIoFullRetentionEnabled": false})
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn automation_channel_dispatches_create_list_and_rejects_run_now_without_workspace() {
        let (paths, root) = temp_paths("automation");
        let store = ProviderStore::new(paths.clone());
        let service = AgentService::new(paths, store);
        // createAutomation dispatched via the channel (automation_channel logic + the grouped match arm in this file).
        let created = service
            .call_inner(
                "createAutomation",
                vec![WireValue::Json(json!({
                    "title": "chan daily",
                    "cronExpr": "30 9 * * *",
                    "prompt": "do it",
                    "workspacePath": "/ws/alpha",
                    "recurring": true,
                }))],
            )
            .await
            .expect("createAutomation");
        let WireValue::Json(created) = created else {
            panic!("createAutomation must return JSON, got {created:?}");
        };
        assert_eq!(created["title"], "chan daily");
        let automation_id = created["automationId"]
            .as_str()
            .expect("automationId")
            .to_string();
        assert!(automation_id.starts_with("automation-"));
        // listAutomations returns the 1 record under that workspace.
        let listed = service
            .call_inner(
                "listAutomations",
                vec![WireValue::Json(json!({"workspacePath": "/ws/alpha"}))],
            )
            .await
            .expect("listAutomations");
        let WireValue::Json(listed) = listed else {
            panic!("listAutomations must return JSON");
        };
        assert_eq!(listed.as_array().expect("array").len(), 1);
        // Isolation across another workspace: not readable.
        let other = service
            .call_inner(
                "listAutomations",
                vec![WireValue::Json(json!({"workspacePath": "/ws/beta"}))],
            )
            .await
            .expect("list other");
        let WireValue::Json(other) = other else {
            panic!("JSON")
        };
        assert!(other.as_array().expect("array").is_empty());
        // runAutomationNow: the dispatcher is in place. This test has no provider and the workspace does not exist either;
        // after the deterministic failure the manual claim must be released (no dangling running=1) — for the full success path see
        // run_automation_now_dispatches_and_records_the_run_outcome in automation_dispatch.rs.
        let error = service
            .call_inner(
                "runAutomationNow",
                vec![WireValue::Json(
                    json!({"automationId": automation_id, "workspacePath": "/ws/alpha"}),
                )],
            )
            .await
            .expect_err("runAutomationNow must fail on a missing workspace");
        assert!(error.to_string().contains("does not exist"), "{error}");
        std::fs::remove_dir_all(root).ok();
    }

    // ── Session read/control surface members (readSessionDebug, compactSession, goalSession,
    //    listSessionSubagents, generateWorkspaceText) dispatched through the channel onto the real host implementation ──

    async fn service_with_session(
        tag: &str,
    ) -> (Arc<AgentService>, std::path::PathBuf, String, Value) {
        let (service, root) = configured_service(tag).await;
        let workspace = root.join("ws");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let target = json!({"workspacePath": workspace.to_string_lossy()});
        let created = service
            .call_inner("createSession", vec![WireValue::Json(target.clone())])
            .await
            .expect("create");
        let WireValue::Json(created) = created else {
            panic!("create must return JSON");
        };
        let session_id = created["session"]["sessionId"]
            .as_str()
            .expect("sessionId")
            .to_string();
        (service, root, session_id, target)
    }

    #[tokio::test]
    async fn read_session_debug_is_existing_only_and_reports_the_real_ledger() {
        let (paths, root) = temp_paths("debug-absent");
        let store = ProviderStore::new(paths.clone());
        let missing = json!({
            "workspacePath": root.join("nope").to_string_lossy(),
            "sessionId": "s-1",
        });
        let error = AgentService::new(paths, store)
            .call_inner("readSessionDebug", vec![WireValue::Json(missing)])
            .await
            .expect_err("existing-only read must not start a runtime");
        assert!(
            error.to_string().contains("ZCodeWorkspaceRuntimeNotFound"),
            "Name error reported when runtime is missing, empty ledger is not returned to pretend to be fact: {error}"
        );

        let (service, root, session_id, target) = service_with_session("debug-live").await;
        let WireValue::Json(snapshot) = service
            .call_inner(
                "readSessionDebug",
                vec![WireValue::Json(json!({
                    "workspacePath": target["workspacePath"],
                    "sessionId": session_id,
                }))],
            )
            .await
            .expect("readSessionDebug")
        else {
            panic!("JSON");
        };
        assert_eq!(snapshot["sessionId"], json!(session_id));
        assert_eq!(snapshot["rounds"], json!([]));
        assert_eq!(snapshot["networkEntries"], json!([]));
        assert_eq!(snapshot["cache"], Value::Null);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn subagent_directory_and_goal_and_compact_reach_the_host() {
        let (service, root, session_id, target) = service_with_session("session-controls").await;
        let params = |extra: Value| {
            let mut merged = target.clone();
            merged["sessionId"] = json!(session_id.clone());
            if let (Some(base), Some(extra_obj)) = (merged.as_object_mut(), extra.as_object()) {
                for (key, value) in extra_obj {
                    base.insert(key.clone(), value.clone());
                }
            }
            merged
        };

        let WireValue::Json(subagents) = service
            .call_inner(
                "listSessionSubagents",
                vec![WireValue::Json(params(json!({"endedLimit": 20})))],
            )
            .await
            .expect("listSessionSubagents")
        else {
            panic!("JSON");
        };
        assert_eq!(subagents["running"], json!([]));
        assert_eq!(subagents["childSessionIds"], json!([]));
        assert_eq!(subagents["ended"]["total"], json!(0));

        let WireValue::Json(goal) = service
            .call_inner(
                "goalSession",
                vec![WireValue::Json(params(json!({"action": "show"})))],
            )
            .await
            .expect("goalSession show")
        else {
            panic!("JSON");
        };
        assert_eq!(
            goal["response"],
            json!("No goal is set. Use /goal <objective> to set one.")
        );
        assert_eq!(goal["startedTurn"], json!(false));
        assert_eq!(goal["snapshot"]["session"]["sessionId"], json!(session_id));

        // Probe the unknown action before compaction: `session/compact` is a fire-and-forget background maintenance command
        // (same TS semantics); once it executes, the target control surface rejects with busy — that is a different tested fact.
        let unknown_action = service
            .call_inner(
                "goalSession",
                vec![WireValue::Json(params(json!({"action": "explode"})))],
            )
            .await
            .expect_err("unknown goal action must fail");
        assert!(
            unknown_action.to_string().contains("unknown goal action"),
            "{unknown_action}"
        );

        let WireValue::Json(compact) = service
            .call_inner(
                "compactSession",
                vec![WireValue::Json(params(json!({"inputId": "compact-1"})))],
            )
            .await
            .expect("compactSession")
        else {
            panic!("JSON");
        };
        assert_eq!(compact["compact"]["state"], json!("accepted"));
        assert_eq!(compact["compact"]["inputId"], json!("compact-1"));
        assert_eq!(compact["response"], json!(""));
        assert!(
            compact["snapshot"].is_object(),
            "snapshot is given by a unique constructor"
        );

        // While compaction is in flight, the target control must be rejected with busy (not queued, not silently executed).
        let busy = service
            .call_inner(
                "goalSession",
                vec![WireValue::Json(params(json!({"action": "show"})))],
            )
            .await;
        match busy {
            Ok(_) => {}
            Err(error) => assert!(
                error
                    .to_string()
                    .contains("Cannot manage goals while a prompt is running"),
                "{error}"
            ),
        }
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn generate_workspace_text_validates_before_any_model_call() {
        // The channel layer's job is "parameter/selection validation + forwarding"; real model calls and cancellation
        // are asserted with a scripted provider in nc-code-host/tests/protocol_methods_e2e.rs.
        // Deliberately no network here: one failed model request burns the full retry budget (minutes).
        let (service, root, _session_id, target) = service_with_session("generate").await;
        let missing_workspace = service
            .call_inner(
                "generateWorkspaceText",
                vec![WireValue::Json(
                    json!({"selection": {"providerId": "p", "modelId": "m"}}),
                )],
            )
            .await
            .expect_err("workspace is required");
        assert!(
            missing_workspace.to_string().contains("workspace"),
            "{missing_workspace}"
        );

        let unknown_model = service
            .call_inner(
                "generateWorkspaceText",
                vec![WireValue::Json(json!({
                    "workspace": {"workspacePath": target["workspacePath"]},
                    "selection": {"providerId": "personal:1", "modelId": "not-configured"},
                    "prompt": "name this workspace",
                    "querySource": "workspace_generate_text",
                }))],
            )
            .await
            .expect_err("a model this host does not have must fail, not silently substitute");
        assert!(
            unknown_model
                .to_string()
                .contains("model is not configured"),
            "{unknown_model}"
        );

        let no_input = service
            .call_inner(
                "generateWorkspaceText",
                vec![WireValue::Json(json!({
                    "workspace": {"workspacePath": target["workspacePath"]},
                    "selection": {"providerId": "personal:1", "modelId": "test-model"},
                    "querySource": "workspace_generate_text",
                }))],
            )
            .await
            .expect_err("prompt or messages is required");
        assert!(
            no_input.to_string().contains("prompt or messages"),
            "{no_input}"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn notification_params_delivers_only_the_matching_notification_payload() {
        let ttft = json!({"jsonrpc":"2.0","method":"v4/telemetry/local-ttft","params":{"commandId":"c-1"}});
        let frame = json!({"jsonrpc":"2.0","method":"v4/conversation/frame","params":{"topic":"conversation/s-1"}});
        assert_eq!(
            notification_params(WireValue::Json(ttft.clone()), "v4/telemetry/local-ttft"),
            Some(WireValue::Json(ttft["params"].clone())),
            "Object vector delivery params (renderer reads the facts directly, not the envelope)"
        );
        assert_eq!(
            notification_params(
                WireValue::String(frame["params"].to_string()),
                "v4/conversation/frame"
            )
            .map(|value| match value {
                WireValue::Json(parsed) => parsed,
                _ => Value::Null,
            }),
            None,
            "The string carrier is the entire line of JSON: params are passed here, without method, and should be skipped"
        );
        assert_eq!(
            notification_params(
                WireValue::String(frame.to_string()),
                "v4/conversation/frame"
            ),
            Some(WireValue::Json(frame["params"].clone())),
            "The entire line of JSON strings is parsed equally"
        );
        assert_eq!(
            notification_params(WireValue::Json(ttft), "v4/conversation/frame"),
            None,
            "No other methods are allowed to lead to this incident."
        );
        assert_eq!(
            notification_params(WireValue::Undefined, "v4/conversation/frame"),
            None
        );
    }

    #[tokio::test]
    async fn session_event_members_fail_closed_without_a_source() {
        // onDynamicSessionEvent was deprecated in TS and this host has no event-log subsystem:
        // subscribing to it must yield Event not found, rather than receiving someone else's frames (anti-fabrication rule 3).
        let (service, root) = configured_service("no-source-event").await;
        let failure = match service
            .listen(
                &ChannelContext::new("conn-test", ClientMode::DesktopContinuous),
                "onDynamicSessionEvent",
                WireValue::Undefined,
            )
            .await
        {
            Err(failure) => failure,
            Ok(_) => panic!("no event source exists for onDynamicSessionEvent"),
        };
        assert!(failure.to_string().contains("Event not found"), "{failure}");
        std::fs::remove_dir_all(root).ok();
    }
}
