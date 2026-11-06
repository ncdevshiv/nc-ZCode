# -*- coding: utf-8 -*-
# AgentService shared session layer: true snapshot (replacing legacy stub), provider store model facts,
# Session configuration command (setModel/setThoughtLevel/setMode → v4 control command), message reading, list.
# Number of assertion hits for each replacement; the semantic basis is written in the comments of the new code (corresponding to TS file: line).
import io

failures = 0
path = "native/crates/zcode-services-agent/src/service.rs"
text = io.open(path, encoding="utf-8").read()


def patch(old, new, label):
    global text, failures
    hits = text.count(old)
    if hits != 1:
        print("MISS(%d) [%s]" % (hits, label))
        failures += 1
        return
    text = text.replace(old, new)
    print("ok [%s]" % label)


# ── 1. imports ──
patch(
    """use zcode_services_provider::store::{
    provider_effective_config, provider_issues, provider_order, ProviderStore,
};

use crate::runtime::AgentRuntime;""",
    """use zcode_services_provider::store::{
    model_effective_config, provider_effective_config, provider_issues, provider_order,
    ProviderStore,
};

use crate::runtime::AgentRuntime;
use crate::snapshot;""",
    "imports",
)

# ── 2. create_session: Create → Subscription → Apply session configuration → True snapshot ──
patch(
    """ let _ = runtime
            .request(
                "v4/conversation/subscribe",
                json!({
                    "sessionId": session_id,
                    "connectionId": self.connection_id,
                    "clientMode": "desktop-continuous",
                }),
            )
            .await?;
        Ok(legacy_snapshot(&runtime.workspace_path, &session_id))
    }

    async fn send_prompt(&self, params: &Value) -> Result<Value, ServiceError> {""",
    """ let _ = runtime
            .request(
                "v4/conversation/subscribe",
                json!({
                    "sessionId": session_id,
                    "connectionId": self.connection_id,"clientMode": "desktop-continuous",
                }),
            )
            .await?;
        // The mode/model/thoughtLevel of TS createSession is not "accept and save": after creation, it will be processed in the same batch
        // v4 control command is implemented, and the snapshot is read back to the real state (zcodeSessionService.ts:221 is passed parameter by parameter
        // agentService.createSession, host side config.rs:65-81 is the only writing point).
        let mut snapshot_params = params.clone();
        snapshot_params["sessionId"] = json!(session_id);
        self.apply_session_config(&runtime, &session_id, params)
            .await?;
        self.build_snapshot(&snapshot_params, None).await
    }

    async fn send_prompt(&self, params: &Value) -> Result<Value, ServiceError> {""",
    "create_session -> real snapshot",
)

# ── 3. read_session: Replace the entire section with a shared construction point ──
patch(
    """ async fn read_session(&self, params: &Value) -> Result<Value, ServiceError> {
        let runtime = self.ensure_runtime(params).await?;
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| ServiceError::invalid_params("sessionId is required"))?;
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
        let snapshot = response
            .frames
            .iter()
            .find_map(|frame| frame.get("snapshot").cloned())
            .unwrap_or(Value::Null);
        Ok(legacy_snapshot_from_v4(
            &runtime.workspace_path,
            session_id,
            &snapshot,
        ))
    }""",""" async fn read_session(&self, params: &Value) -> Result<Value, ServiceError> {
        // messageLimit is the tail window of TS ReadParams (zcodeSession.ts:69; same note as SnapshotInput).
        let message_limit = params
            .get("messageLimit")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        self.build_snapshot(params, message_limit).await
    }""",
    "read_session -> shared builder",
)

# ── 4. Convert create/read/close to pub(crate) for reuse by zcode-session channel ──
patch(
    " async fn create_session(&self, params: &Value) -> Result<Value, ServiceError> {",
    " pub(crate) async fn create_session(&self, params: &Value) -> Result<Value, ServiceError> {",
    "create_session pub(crate)",
)
patch(
    " async fn read_session(&self, params: &Value) -> Result<Value, ServiceError> {",
    " pub(crate) async fn read_session(&self, params: &Value) -> Result<Value, ServiceError> {",
    "read_session pub(crate)",
)
patch(
    " async fn close_session(&self, params: &Value) -> Result<Value, ServiceError> {",
    " pub(crate) async fn close_session(&self, params: &Value) -> Result<Value, ServiceError> {",
    "close_session pub(crate)",
)

# ── 5. New method: inserted between the end of impl and Channel impl; free fn follows ──
anchor = """ other => Err(ServiceError::message(format!("Method not found: {other}"))),
        }
    }
}

#[async_trait]
impl Channel for AgentService {"""

new_block = """ other => Err(ServiceError::message(format!("Method not found: {other}"))),
        }
    }

    //── Session snapshot sharing layer (zcode-agent and zcode-session two channels share the same write point)──

    /// The only source of model/stall facts is the personal provider store: host has no model directory,
    /// The annotation of `snapshot::SettingsFacts` indicates that it is completed by this layer. Field-by-field alignment TS `toModelOption`/// (apps/zcode-cli/packages/bootstrap/src/app/provider-registry-selection.ts:184):
    /// The optional key (contextWindow/maxOutputTokens/reasoning) is omitted if it is unknown, and properties are required.
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
            available_models,thought_levels,
            thought_default,
            context_window,
        }
    }

    /// The only construction point for session snapshots: host session facts (views/transcriptions/usage) + v4 authoritative snapshot +
    /// Provider store model facts, by `snapshot::build` by strict
    /// `zcodeSessionStateSnapshotSchema` output. When the session does not exist, resync will report an error first.
    /// Will not degenerate into an empty snapshot (that's how the old legacy stub faked state).
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
        // resync will cold resume; go through it again to ensure that the cold session can read the facts.
        runtime.ensure_loaded(&session_id);
        let view = runtime
            .session_view(&session_id)
            .ok_or_else(|| ServiceError::named("ZCodeSessionNotFound", format!("session not found: {session_id}")))?;
        let transcript = runtime.session_transcript_view(&session_id);
        let rows: Vec<Value> = transcript
            .as_ref()
            .and_then(|view| view.get("rows"))
            .and_then(Value::as_array)
            .cloned().unwrap_or_default();
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

    /// Optional session configuration for create/resume: model / thoughtLevel / mode will be issued only after each item exists.
    /// (`set_model_config` is an entire set of overrides, and missing keys will be cleared, so items not provided will always use the current facts of the session).
    pub(crate) async fn apply_session_config(
        &self,
        runtime: &Arc<AgentRuntime>,
        session_id: &str,
        params: &Value,
    ) -> Result<(), ServiceError> {
        let view = runtime.session_view(session_id).unwrap_or(Value::Null);
        if let Some(model) = params.get("model") {
            let payload = model_payload(&view, model);
            self.v4_command(runtime, self.command_envelope(Some(session_id), "switchModelConfig", payload))
                .await?;
        }
        if let Some(level) = params
            .get("thoughtLevel")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let (provider, model, _) = view_triple(&view);
            let payload = json!({"provider": provider, "model": model, "thought": level});self.v4_command(runtime, self.command_envelope(Some(session_id), "switchModelConfig", payload))
                .await?;
        }
        if let Some(mode) = params.get("mode").and_then(Value::as_str) {
            let payload = json!({"mode": mode});
            self.v4_command(
                runtime,
                self.command_envelope(Some(session_id), "switchCollaborationMode", payload),
            )
            .await?;
        }
        Ok(())
    }

    /// `setModel`/`setThoughtLevel`/`setMode`(zcodeSession.ts:152-154).
    /// The control command of host is not CAS (control group of commands/mod.rs:135), but TS
    /// `expectedRevision` and `persistAsWorkspaceLastUsed` (out-of-session workspace memory)
    /// There is no corresponding status in this host: the difference is recorded as a known difference and no forged writes are made.
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
            // setThoughtLevel takes the same switchModelConfig: provider/model must bring the current value.
            _ if params.contains_key("thoughtLevel") => {
                let (provider, model, _) = view_triple(&view);
                let level = params.get("thoughtLevel")
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
        let envelope = self.command_envelope(Some(&session_id), command, payload);
        self.v4_command(&runtime, envelope).await?;
        self.build_snapshot(params, None).await
    }

    /// `readSessionMessages`(zcodeSession.ts:147): transcribed line → `ZCodeMessageWithParts[]`,
    /// After the cursor `afterMessageId`, `limit` is truncated (forward paging semantics).
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
        let view = runtime.session_view(&session_id).ok_or_else(|| {ServiceError::named("ZCodeSessionNotFound", format!("session not found: {session_id}"))
        })?;
        let transcript = runtime.session_transcript_view(&session_id).ok_or_else(|| {
            ServiceError::named("ZCodeSessionNotFound", format!("session not found: {session_id}"))
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

    /// `listSessions` (zcodeSession.ts:146): The disk meta is the complete set (including cold sessions),
    /// The live view takes the real phase/status first; updatedAt is in descending order, the same value is in ascending order by sessionId, and limit is truncated.
    /// The `includeArchived` archive bit of TS has no corresponding field in the session meta and does not forge archiving semantics.
    pub(crate) async fn list_sessions_info(&self, params: &Value) -> Result<Value, ServiceError> {
        let (workspace_path, key) = Self::workspace_of(params)?;let limit = params.get("limit").and_then(Value::as_u64).map(|v| v as usize);
        let sessions_dir = self.data_dir_for(&workspace_path, &key).join("sessions");
        let runtime = self.runtime_for(&key).await;
        let mut rows: Vec<(i64, String, Value)> = Vec::new();
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
                let fact = runtime
                    .as_ref()
                    .and_then(|runtime| runtime.session_view(session_id))
                    .unwrap_or(meta);
                let info = snapshot::session_info(&fact, &workspace_path, &key);
                if info.is_null() {
                    continue;
                }
                let updated_at = info.get("updatedAt").and_then(Value::as_i64).unwrap_or(0);
                rows.push((updated_at, session_id.to_string(), info));
            }
        }
        rows.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        if let Some(limit) = limit {
            rows.truncate(limit);
        }
        Ok(Value::Array(
            rows.into_iter().map(|(_, _, info)| info).collect::<Vec<Value>>(),
        ))
    }

    /// `resumeSession` (zcodeSession.ts:46): subscribe (host cold resume entry) →/// optional model/thoughtLevel → true snapshot.
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
        .and_then(|spec| spec.get("max")){
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
        // toModelOption: defaultLevel = values.at(-1).
        if let Some(last) = levels.last() {
            reasoning.insert("defaultLevel".to_string(), last.clone());
        }
        option.insert("reasoning".to_string(), Value::Object(reasoning));
    }
    // properties are strict required: `default_model_config()` guaranteed inputFormat/outputFormat
    // Exists; when the key is missing, strict parse will fail directly and no placeholder value will be written to cover it up.
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

/// The gear facts and context window of the current model (see snapshot.rs:55-69 for consumers of snapshot::SettingsFacts).
fn thought_facts(config: &Value) -> (Vec<Value>, Option<String>, u64) {
    let reasoning=config
        .get("optionSpecs")
        .and_then(|specs| specs.get("reasoningLevel"));
    let levels = reasoning
        .and_then(|spec| spec.get("values"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let thought_levels = levels
        .iter().map(|value| json!({"value": value, "label": value}))
        .collect::<Vec<Value>>();
    let thought_default = reasoning
        .and_then(|spec| spec.get("default"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            levels
                .last()
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|_| !thought_levels.is_empty());
    let context_window = config
        .get("properties")
        .and_then(|value| value.get("contextWindow"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    (thought_levels, thought_default, context_window)
}

/// The provider/model/thought of switchModelConfig is the entire set of triples (host is the entire set of overwrite writes).
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

/// The current provider/model/thought ternary of the session (for missing items, press the empty string, and the default payload of the host is also "").
fn view_triple(view: &Value) -> (String, String, String) {
    let read = |key: &str| {
        view.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)            .unwrap_or_default()
    };
    (read("provider"), read("model"), read("thought"))
}

#[async_trait]
impl Channel for AgentService {"""

patch(anchor, new_block, "shared session layer methods")

io.open(path, "w", encoding="utf-8", newline="").write(text)
if failures:
    raise SystemExit("FAILED: %d replacement(s)" % failures)
print("service.rs patched")
