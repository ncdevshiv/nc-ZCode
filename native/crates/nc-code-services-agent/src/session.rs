//! The `nc-code-session` channel — the session application-service surface of TS `packages/services/src/nc-code-session/`.
//!
//! On the TS side `zcodeSessionService.ts` is a facade over the agent service (it forwards method by method to `agentService.*`,
//! see the comment at zcodeSession.ts:155), so the Rust side likewise only does parameter parsing and failure semantics, while all behaviour
//! reuses the shared implementation of `AgentService`: snapshot construction, session configuration commands and message reading each have exactly one write path
//! (the AGENTS.md single-owner rule), with no second copy here.
//!
//! Unwired members of this channel (see `specs/rust-backend-replacement.md` §4.4.1):
//! `readSessionEvents` (host has no event-log subsystem), `promoteDeferredDraftSession` /
//! `closeDeferredDraftSession` (the deferred draft registry is TS in-process state).
//! They fall through to the default `Method not found` (fail-fast), instead of forging an empty result.
//! `readWorkspacePresentation` is wired: the slash-command directory is injected by the
//! composition root (`commands` channel) — one discovery path, no second scanner.

use std::sync::Arc;

use serde_json::{json, Value};

use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::{ServiceArgs, ServiceError};

use crate::service::AgentService;

pub struct SessionService {
    agent: Arc<AgentService>,
}

impl SessionService {
    pub fn new(agent: Arc<AgentService>) -> Arc<Self> {
        Arc::new(Self { agent })
    }

    /// `ZCodeSessionWorkspaceTarget`: workspacePath is required; the identity ?? path rule lives in
    /// `AgentService::workspace_of` (AGENTS.md Workspace Identity).
    fn target(params: &Value) -> Result<(String, String), ServiceError> {
        AgentService::workspace_of(params)
    }

    fn session_id(params: &Value) -> Result<String, ServiceError> {
        params
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| ServiceError::invalid_params("sessionId is required"))
    }
}

#[async_trait::async_trait]
impl Channel for SessionService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        match command {
            // `initializeWorkspace`: an absent provider is a legal state (the reasonCode at zcodeSession.ts:118-126),
            // not an error; every other failure propagates honestly.
            "initializeWorkspace" => {
                let params = args.required_json(0, "params")?;
                let (_, key) = Self::target(&params)?;
                match self.agent.ensure_runtime(&params).await {
                    Ok(_runtime) => Ok(WireValue::Json(json!({
                        "available": true,
                        "workspaceKey": key,
                        "protocolName": "ZCode Protocol",
                        "protocolVersion": 1,
                        "transportKind": "stdio",
                    }))),
                    Err(error) if error.message.contains("Provider not configured") => {
                        Ok(WireValue::Json(json!({
                            "available": false,
                            "workspaceKey": key,
                            "reason": error.message,
                            "reasonCode": "provider_not_ready",
                        })))
                    }
                    Err(error) => Err(error.into()),
                }
            }
            // The existing-only probe (zcodeSession.ts:128-133): with no runtime present, no runtime is created.
            "getWorkspaceRuntimeIdentity" => {
                let params = args.required_json(0, "params")?;
                let (_, key) = Self::target(&params)?;
                if self.agent.runtime_for(&key).await.is_none() {
                    return Err(ServiceError::named(
                        "ZCodeWorkspaceRuntimeNotFound",
                        format!("workspace runtime is not running: {key}"),
                    )
                    .into());
                }
                Ok(WireValue::Json(json!({
                    // The in-process host has no runtime generation recycling (runtime.rs has no recycle path),
                    // so generation is always 1; processId is the process of the managed host.
                    "generation": 1,
                    "identity": key,
                    "processId": std::process::id(),
                    "workspaceKey": key,
                })))
            }
            "readWorkspacePresentation" => {
                let params = args.required_json(0, "params")?;
                Ok(WireValue::Json(
                    self.agent.read_workspace_presentation(&params).await?,
                ))
            }
            "createSession" => {
                let params = args.required_json(0, "params")?;
                Ok(WireValue::Json(self.agent.create_session(&params).await?))
            }
            "resumeSession" => {
                let params = args.required_json(0, "params")?;
                Ok(WireValue::Json(self.agent.resume_session(&params).await?))
            }
            "listSessions" => {
                let params = args.required_json(0, "params")?;
                Ok(WireValue::Json(
                    self.agent.list_sessions_info(&params).await?,
                ))
            }
            "readSession" => {
                let params = args.required_json(0, "params")?;
                Ok(WireValue::Json(self.agent.read_session(&params).await?))
            }
            "readSessionMessages" => {
                let params = args.required_json(0, "params")?;
                Self::session_id(&params)?;
                Ok(WireValue::Json(
                    self.agent.read_session_messages(&params).await?,
                ))
            }
            "closeSession" => {
                let params = args.required_json(0, "params")?;
                Self::session_id(&params)?;
                Ok(WireValue::Json(self.agent.close_session(&params).await?))
            }
            "setModel" | "setThoughtLevel" => {
                // the host's `switchModelConfig` writes provider/model/thought as one whole group
                // (commands/config.rs:65), so thoughtLevel goes through the same command.
                let params = args.required_json(0, "params")?;
                Self::session_id(&params)?;
                Ok(WireValue::Json(
                    self.agent
                        .set_model_config(&params, "switchModelConfig")
                        .await?,
                ))
            }
            "setMode" => {
                let params = args.required_json(0, "params")?;
                Self::session_id(&params)?;
                Ok(WireValue::Json(
                    self.agent
                        .set_model_config(&params, "switchCollaborationMode")
                        .await?,
                ))
            }
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;
    use nc_code_service_core::AppPaths;
    use nc_code_services_provider::store::{
        default_model_config, ModelRecord, ProviderRecord, ProviderStore,
    };

    fn temp_paths(tag: &str) -> (AppPaths, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "zcode-session-test-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).expect("root");
        (AppPaths::new(root.join("home"), root.join("data")), root)
    }

    async fn configured_service(tag: &str) -> (Arc<SessionService>, std::path::PathBuf) {
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
                        "api": {"type": "openai-chat-completions", "baseUrl": "https://api.example.com/v1"},
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
        let agent = AgentService::new(paths, store);
        (SessionService::new(agent), root)
    }

    fn context() -> ChannelContext {
        ChannelContext::new("conn-session-test", ClientMode::DesktopContinuous)
    }

    async fn call(service: &Arc<SessionService>, method: &str, params: Value) -> Value {
        let result = service
            .call(
                &context(),
                method,
                vec![WireValue::Json(params)],
                CancelToken::new(),
            )
            .await
            .unwrap_or_else(|error| panic!("{method} must succeed: {error:?}"));
        match result {
            WireValue::Json(value) => value,
            other => panic!("{method} must return JSON, got {other:?}"),
        }
    }

    async fn workspace(root: &std::path::Path) -> Value {
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        json!({"workspacePath": workspace.to_string_lossy()})
    }

    fn session_key_set(snapshot: &Value) -> Vec<String> {
        snapshot
            .as_object()
            .expect("snapshot object")
            .keys()
            .cloned()
            .collect()
    }

    #[tokio::test]
    async fn create_session_returns_the_strict_snapshot_shape_with_real_model_facts() {
        let (service, root) = configured_service("create").await;
        let target = workspace(&root).await;
        let snapshot = call(&service, "createSession", target.clone()).await;

        // The key set of the strict `zcodeSessionStateSnapshotSchema`: goalStats appears only when a goal is present.
        assert_eq!(
            session_key_set(&snapshot),
            vec![
                "protocol",
                "session",
                "settings",
                "projection",
                "runtime",
                "messages",
                "todos",
                "slashCommands",
            ],
            "{snapshot:#}",
        );
        let session_id = snapshot["session"]["sessionId"]
            .as_str()
            .expect("sessionId")
            .to_string();
        assert_eq!(
            snapshot["session"]["workspace"]["workspacePath"],
            json!(target["workspacePath"]),
            "workspace is a nested ref (the flat key of the old stub will cause strict parsing to fail directly)",
        );
        assert_eq!(snapshot["projection"]["turnCount"], json!(0));
        assert_eq!(snapshot["messages"], json!([]));
        assert_eq!(
            snapshot["runtime"]["deliveryKind"],
            json!("desktop-continuous")
        );

        // The model facts come from the real provider store, not from an empty-array placeholder.
        let available = snapshot["settings"]["model"]["available"]
            .as_array()
            .expect("available array");
        assert_eq!(available.len(), 1, "{available:#?}");
        assert_eq!(available[0]["label"], json!("test-model"));
        assert_eq!(available[0]["ref"]["providerId"], json!("personal:1"));
        assert_eq!(
            available[0]["contextWindow"],
            json!(128_000),
            "contextWindow real default from default_model_config",
        );
        assert!(available[0]["properties"]["inputFormat"].is_object());
        // Without an explicitly bound model, current is omitted (TS: "an unbound state is a legal recovery state"); no placeholder is written.
        assert!(
            snapshot["settings"]["model"].get("current").is_none(),
            "{:#}",
            snapshot["settings"]["model"],
        );

        // A cold read of the same session: the snapshot key set matches, the row count is empty but every field is present.
        let read = call(
            &service,
            "readSession",
            json!({
                "workspacePath": target["workspacePath"],
                "sessionId": session_id,
                "messageLimit": 10,
            }),
        )
        .await;
        assert_eq!(session_key_set(&read), session_key_set(&snapshot));
        assert_eq!(read["messages"], json!([]));
    }

    #[tokio::test]
    async fn set_mode_and_set_model_land_on_the_real_session_state() {
        let (service, root) = configured_service("config").await;
        let target = workspace(&root).await;
        let created = call(&service, "createSession", target.clone()).await;
        let session_id = created["session"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        // "edit" is inside the closed set of snapshot::sanitize_mode (plan/build/edit/yolo/auto).
        let after_mode = call(
            &service,
            "setMode",
            json!({
                "workspacePath": target["workspacePath"],
                "sessionId": session_id,
                "mode": "edit",
            }),
        )
        .await;
        assert_eq!(after_mode["settings"]["mode"]["current"], json!("edit"));
        assert_eq!(after_mode["session"]["mode"], json!("edit"));

        let after_model = call(
            &service,
            "setModel",
            json!({
                "workspacePath": target["workspacePath"],
                "sessionId": session_id,
            "model": {"providerId": "personal:1", "modelId": "test-model"},
            }),
        )
        .await;
        assert_eq!(
            after_model["settings"]["model"]["current"],
            json!({"providerId": "personal:1", "modelId": "test-model"}),
            "switchModelConfig is a whole set of writes: current must actually appear after binding",
        );
        // When the current model carries level facts, defaultLevel takes the store's declared level/last level (provider-registry-selection.ts:194).
        assert!(
            after_model["settings"]["thoughtLevel"]
                .get("defaultLevel")
                .is_some(),
            "{:#}",
            after_model["settings"]["thoughtLevel"],
        );
    }

    #[tokio::test]
    async fn list_and_messages_round_trip_then_unknown_session_fails_loudly() {
        let (service, root) = configured_service("round").await;
        let target = workspace(&root).await;
        let created = call(&service, "createSession", target.clone()).await;
        let session_id = created["session"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        let listed = call(&service, "listSessions", target.clone()).await;
        let ids: Vec<&str> = listed
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|info| info["sessionId"].as_str())
            .collect();
        assert!(ids.contains(&session_id.as_str()), "{listed:#}");

        let messages = call(
            &service,
            "readSessionMessages",
            json!({
                "workspacePath": target["workspacePath"],
                "sessionId": session_id,
                "limit": 50,
            }),
        )
        .await;
        assert_eq!(
            messages,
            json!([]),
            "New session has no transcribed lines → empty array is fact"
        );

        // A session that does not exist must error; falling back to an empty snapshot/empty messages is not allowed (that is exactly how the old stub forged it).
        let unknown = service
            .call(
                &context(),
                "readSession",
                vec![WireValue::Json(json!({
                    "workspacePath": target["workspacePath"],
                    "sessionId": "does-not-exist",
                }))],
                CancelToken::new(),
            )
            .await;
        assert!(
            unknown.is_err(),
            "Unknown session reads must fail instead of returning an empty snapshot"
        );

        let unknown_messages = service
            .call(
                &context(),
                "readSessionMessages",
                vec![WireValue::Json(json!({
                    "workspacePath": target["workspacePath"],
                    "sessionId": "does-not-exist",
                }))],
                CancelToken::new(),
            )
            .await;
        assert!(
            unknown_messages.is_err(),
            "Reading messages from unknown sessions must fail"
        );
    }

    #[tokio::test]
    async fn initialize_workspace_without_provider_reports_not_ready() {
        let (paths, root) = temp_paths("no-provider");
        let store = ProviderStore::new(paths.clone());
        let service = SessionService::new(AgentService::new(paths, store));
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let result = call(
            &service,
            "initializeWorkspace",
            json!({"workspacePath": workspace.to_string_lossy()}),
        )
        .await;
        assert_eq!(result["available"], json!(false));
        assert_eq!(result["reasonCode"], json!("provider_not_ready"));
        assert_eq!(result["reason"], json!("Provider not configured"));
    }

    #[tokio::test]
    async fn undocumented_members_fail_fast_with_the_named_method() {
        let (service, root) = configured_service("fail-closed").await;
        let target = workspace(&root).await;
        for method in [
            "readSessionEvents",
            "promoteDeferredDraftSession",
            "closeDeferredDraftSession",
        ] {
            // The missing subsystems of these four members are recorded in specs/rust-backend-replacement.md §4.4.1;
            // unwired → the default `Method not found` (fail-fast), never an empty result.
            let outcome = service
                .call(
                    &context(),
                    method,
                    vec![WireValue::Json(target.clone())],
                    CancelToken::new(),
                )
                .await;
            let error = outcome.expect_err("missing subsystem must fail closed");
            assert!(
                error.to_string().contains("Method not found"),
                "{method}: {error:?}",
            );
        }
    }

    /// `readWorkspacePresentation` — the protocol directory is the builtin set plus the custom
    /// commands minus reserved names and `disable-noninteractive` entries; a workspace ref
    /// without `workspaceKey` derives it from the path (AGENTS.md identity ?? path rule).
    #[tokio::test]
    async fn read_workspace_presentation_returns_the_protocol_directory() {
        let (service, root) = configured_service("presentation").await;
        let workspace_path = root.join("workspace");
        std::fs::create_dir_all(workspace_path.join(".zcode").join("commands")).expect("dir");
        // Reserved name: never reaches the protocol directory.
        std::fs::write(
            workspace_path
                .join(".zcode")
                .join("commands")
                .join("goal.md"),
            "---\ndescription: shadow goal\n---\n\nbody\n",
        )
        .expect("reserved command");
        std::fs::write(
            workspace_path
                .join(".zcode")
                .join("commands")
                .join("standup.md"),
            "---\ndescription: daily standup\nargument-hint: <topic>\n---\n\nbody\n",
        )
        .expect("custom command");
        std::fs::write(
            workspace_path
                .join(".zcode")
                .join("commands")
                .join("headless.md"),
            "---\ndescription: headless only\ndisable-noninteractive: true\n---\n\nbody\n",
        )
        .expect("non-interactive command");

        struct Directory(std::path::PathBuf);
        #[async_trait::async_trait]
        impl crate::service::SlashCommandDirectory for Directory {
            async fn protocol_slash_commands(
                &self,
                workspace_path: &str,
            ) -> Result<Value, ServiceError> {
                // Same implementation the composition root injects (the commands channel).
                let service = nc_code_services_extensions::commands::CommandsService::new(
                    nc_code_service_core::AppPaths::new(self.0.clone(), self.0.clone()),
                );
                service.protocol_slash_commands(workspace_path).await
            }
        }
        service
            .agent
            .attach_slash_command_directory(Arc::new(Directory(root.clone())));

        let result = service
            .call(
                &context(),
                "readWorkspacePresentation",
                vec![WireValue::Json(json!({
                    "workspace": {
                        "workspacePath": workspace_path.to_string_lossy(),
                        "workspaceIdentity": workspace_path.to_string_lossy(),
                    },
                }))],
                CancelToken::new(),
            )
            .await
            .expect("presentation must be wired");
        let value = match result {
            WireValue::Json(value) => value,
            other => panic!("expected JSON, got {other:?}"),
        };
        assert_eq!(value["mode"], json!("build"));
        let directory = value["slashCommands"].as_array().expect("directory");
        let names: Vec<&str> = directory
            .iter()
            .filter_map(|entry| entry["name"].as_str())
            .collect();
        for builtin in ["goal", "compact", "init", "plan"] {
            assert!(names.contains(&builtin), "{} missing: {:?}", builtin, names);
        }
        assert!(
            !names.contains(&"headless"),
            "disable-noninteractive must be filtered: {names:?}"
        );
        let goal_entries: Vec<&Value> = directory
            .iter()
            .filter(|entry| entry["name"] == json!("goal"))
            .collect();
        assert_eq!(
            goal_entries.len(),
            1,
            "reserved custom name must not duplicate: {names:?}"
        );
        assert_eq!(goal_entries[0]["source"], json!("builtin"));
        let standup = directory
            .iter()
            .find(|entry| entry["name"] == json!("standup"))
            .expect("custom command in directory");
        assert_eq!(standup["source"], json!("custom"));
        assert_eq!(standup["inputHint"], json!("/standup <topic>"));
        // A ref without workspaceKey resolves it through the identity ?? path rule.
        let derived = service
            .call(
                &context(),
                "readWorkspacePresentation",
                vec![WireValue::Json(json!({
                    "workspace": {"workspacePath": workspace_path.to_string_lossy()},
                }))],
                CancelToken::new(),
            )
            .await
            .expect("presentation without key");
        let derived = match derived {
            WireValue::Json(value) => value,
            other => panic!("expected JSON, got {other:?}"),
        };
        assert_eq!(
            derived["workspace"]["workspaceKey"],
            json!(workspace_path.to_string_lossy())
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
