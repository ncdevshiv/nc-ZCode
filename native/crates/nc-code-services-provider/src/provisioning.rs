//! `provider-provisioning-target` channel - the receiving end across Environment Provisioning.
//!
//! TS peer implementation: `packages/services/src/model-provider/providerProvisioningTarget.ts`
//! (Interface contract `providerProvisioning.ts`, Provision fact `providerProvisioningSource.ts`,
//! envelope/result schema `packages/shared/src/provider-provisioning.ts`).
//!
//! Like TS, keep transaction and rollback invariants in the same file: idempotent (syncId), atomic write + rollback sequence,
//! Cross-process lock, credential allowlist and trusted host access control depend on each other's execution order, and dismantling them will cover up the coupling.
//!
//! The writing surface is the same as TS, there are three places: account settings (`setting`), credentials (`credential`),
//! Personal Provider Config (Rust side is the personal provider file of `store::ProviderStore`).

pub mod file_lock;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure, ClientMode};
use nc_code_rpc::WireValue;
use nc_code_service_core::atomic::{atomic_write_text, read_text_if_exists};
use nc_code_service_core::{AppPaths, ServiceArgs, ServiceError};
use nc_code_services_config::credential::CredentialService;
use nc_code_services_config::setting::SettingService;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::settings::ProviderSettingsService;
use crate::store::{
    default_model_config, model_effective_config, model_ids_from_config, provider_effective_config,
    provider_issues, ModelRecord, ProviderRecord, ProviderStore, StoreSnapshot,
};

/// `PROVISIONING_SCHEMA_VERSION` (providerProvisioningTarget.ts:26).
const PROVISIONING_SCHEMA_VERSION: u64 = 1;
/// `PROVIDER_PROVISIONING_OAUTH_CREDENTIAL_KEYS` (providerProvisioningSource.ts:22-31).
pub const PROVIDER_PROVISIONING_OAUTH_CREDENTIAL_KEYS: [&str; 8] = [
    "oauth:active_provider",
    "oauth:zai:access_token",
    "oauth:zai:refresh_token",
    "oauth:zai:user_info",
    "oauth:bigmodel:access_token",
    "oauth:bigmodel:refresh_token",
    "oauth:bigmodel:user_info",
    "zcodejwttoken",
];
/// `z.array(...).max(256)` (provider-provisioning.ts:80).
const MAX_PROVISIONING_CREDENTIALS: usize = 256;
/// The access control copy is completely consistent with TS (http.ts:113).
const TRUSTED_HOST_REQUIRED_MESSAGE: &str =
    "Provider Provisioning only supports trusted Desktop Host";
/// providerProvisioningTarget.ts:105.
const PERSONAL_CONFIG_CHANGED_MESSAGE: &str =
    "Personal Provider Config was modified by other operations during synchronization";
/// providerProvisioningTarget.ts:217.
const PERSONAL_CONFIG_SKIP_ROLLBACK_MESSAGE: &str =
    "Personal Provider Config was modified by other operations during synchronization, skipping rollback";
/// providerProvisioningTarget.ts:234.
fn credential_skip_rollback_message(key: &str) -> String {
    format!(
        "Credential {key} was modified by other operations during synchronization, skip rollback"
    )
}
/// providerProvisioningTarget.ts:253.
const SETTINGS_SKIP_ROLLBACK_MESSAGE: &str =
    "Account Settings were modified by other operations during synchronization, skip rollback";
/// providerProvisioningTarget.ts:119.
const DEFAULT_MODEL_UNSUPPORTED_MESSAGE: &str =
    "The synchronized remote Registry does not support the local default model";
/// Connectors for multiple rollback errors and "main error/rollback error" (providerProvisioningTarget.ts:142,265).
const ERROR_JOINER: &str = ";";
const ROLLBACK_FAILED_INFIX: &str = ";Rollback failed:";
/// `stateFilePath`: `join(appConfigDir, "runtime", "provider", "provisioning.json")`
/// (packages/services/src/node.ts:2607).
const STATE_FILE_SEGMENTS: [&str; 3] = ["runtime", "provider", "provisioning.json"];
/// `resolveCredentialFilePath(appConfigDir)` (providerProvisioningSource.ts:162-164).
const CREDENTIAL_FILE_NAME: &str = "credentials.json";
/// Account Provider request period API key shape: `^account-provider:.+:api-key$`
/// (provider-provisioning.ts:27-30).
const ACCOUNT_KEY_PREFIX: &str = "account-provider:";
const ACCOUNT_KEY_SUFFIX: &str = ":api-key";

/// `providerProvisioningResultSchema.shape.status` (provider-provisioning.ts:89).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProvisioningStatus {
    #[serde(rename = "applied")]
    Applied,
    #[serde(rename = "already-applied")]
    AlreadyApplied,
    #[serde(rename = "unsupported")]
    Unsupported,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "rollback_failed")]
    RollbackFailed,
}

/// `ProviderProvisioningResult`; the field order is consistent with the TS object literal
/// (providerProvisioningTarget.ts:122-129 and 137-153), no key is written when the optional field is omitted.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProvisioningResult {
    pub sync_id: String,
    pub status: ProvisioningStatus,
    pub personal_provider_count: usize,
    pub credential_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub rolled_back: bool,
}

/// `providerProvisioningCredentialScopeSchema` (provider-provisioning.ts:17-20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialScope {
    OAuthSession,
    AccountProvider,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CredentialEntry {
    pub scope: CredentialScope,
    pub key: String,
    pub value: String,
}

/// `providerProvisioningAccountSettingsSchema` (provider-provisioning.ts:51-60).
#[derive(Debug, Clone, PartialEq)]
pub struct AccountSettings {
    pub provider_family_domain: Option<String>,
    pub provider_family_connection_selections: Value,
}

/// `providerProvisioningPersonalConfigSchema` (provider-provisioning.ts:33-45).
/// The rule body is `z.array(z.unknown())` at the envelope layer, and the real field verification occurs in the Personal codec
/// (`parse_personal_update`), in the same order as TS's two-stage verification.
#[derive(Debug, Clone, PartialEq)]
pub struct PersonalConfig {
    pub provider_rules: Vec<Value>,
    pub provider_model_rules: Vec<Value>,
    pub manual_provider_model_rules: Vec<Value>,
    pub provider_order: Option<Vec<String>>,
    pub default_model_selection: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProvisioningEnvelope {
    pub sync_id: String,
    pub personal_config: PersonalConfig,
    pub account_settings: AccountSettings,
    pub credentials: Vec<CredentialEntry>,
}

/// `ProvisioningStateRecord` (providerProvisioningTarget.ts:41-49).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct StateRecord {
    sync_id: String,
    result: ProvisioningResult,
}

/// `ProvisioningStateFile`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct StateFile {
    schema_version: u64,
    records: Vec<StateRecord>,
}

/// `BeforeState`(providerProvisioningTarget.ts:160-164) - Rollback baseline.
/// `credentials` uses ordered Vec instead of HashMap, maintaining the insertion order of TS `Map` (oauth allowlist →
/// physical key → envelope key), the rollback traversal order is therefore consistent with TS.
struct BeforeState {
    personal: StoreSnapshot,
    settings: Value,
    credentials: Vec<(String, Option<String>)>,
}

/// `AppliedProvisioningState` (providerProvisioningTarget.ts:166-172) - Registered write collection.
struct AppliedState {
    settings: bool,
    settings_expected: AccountSettings,
    credentials: Vec<(String, Option<String>)>,
    personal: bool,
    personal_expected: StoreSnapshot,
}

pub struct ProviderProvisioningTargetService {
    paths: AppPaths,
    store: Arc<ProviderStore>,
    settings: Arc<SettingService>,
    credentials: Arc<CredentialService>,
    /// `registryService.refresh()` in TS allows provider consumers to see the new configuration; the corresponding fact in Rust
    /// The outlet is the `onDidChange` broadcast of `provider-settings`/`model-selection`.
    notifier: Arc<ProviderSettingsService>,
}

impl ProviderProvisioningTargetService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        paths: AppPaths,
        store: Arc<ProviderStore>,
        settings: Arc<SettingService>,
        credentials: Arc<CredentialService>,
        notifier: Arc<ProviderSettingsService>,
    ) -> Arc<Self> {
        Arc::new(Self {
            paths,
            store,
            settings,
            credentials,
            notifier,
        })
    }

    /// `{appConfigDir}/runtime/provider/provisioning.json` (host path of idempotent state files and locks).
    pub fn state_file(&self) -> PathBuf {
        let mut path = self.paths.app_config_dir();
        for segment in STATE_FILE_SEGMENTS {
            path.push(segment);
        }
        path
    }

    pub fn settings_service(&self) -> &Arc<SettingService> {
        &self.settings
    }

    pub fn credential_service(&self) -> &Arc<CredentialService> {
        &self.credentials
    }

    /// `apply(envelope)`: First perform envelope verification (TS:57, outside the lock), and then enter the cross-process lock to execute the transaction.
    pub async fn apply(&self, input: &Value) -> Result<ProvisioningResult, ServiceError> {
        let envelope = parse_envelope(input)?;
        let state_file = self.state_file();
        // TS:58 —— `withFileLock(stateFilePath, …)`; see file_lock.rs (cross-process) for lock semantics.
        file_lock::with_file_lock(&state_file, file_lock::LockOptions::default(), || async {
            self.apply_locked(&envelope, &state_file).await
        })
        .await
    }

    async fn apply_locked(
        &self,
        envelope: &ProvisioningEnvelope,
        state_file: &Path,
    ) -> Result<ProvisioningResult, ServiceError> {
        let previous_state = read_state_file(state_file).await;
        // Idempotent (TS:60-68): The same syncId directly plays back the registered results, only replace status with already-applied.
        if let Some(record) = previous_state.as_ref().and_then(|state| {
            state
                .records
                .iter()
                .find(|record| record.sync_id == envelope.sync_id)
        }) {
            let mut replayed = record.result.clone();
            replayed.status = ProvisioningStatus::AlreadyApplied;
            return Ok(replayed);
        }

        // TS:70 - The credential key must fall within the allowlist and the scope matches the key category.
        validate_credential_entries(&envelope.credentials)?;
        // TS:71 —— `providerRuntime.start()`: The Rust side corresponds to "personal provider store is ready".
        self.store.snapshot().await?;

        let before = self.capture_before_state(envelope).await?;
        let personal_update = parse_personal_update(&envelope.personal_config)?;
        let mut applied = AppliedState {
            settings: false,
            settings_expected: envelope.account_settings.clone(),
            credentials: Vec::new(),
            personal: false,
            personal_expected: personal_update.clone(),
        };

        match self
            .commit(
                envelope,
                &before,
                &personal_update,
                &mut applied,
                previous_state,
                state_file,
            )
            .await
        {
            Ok(result) => Ok(result),
            Err(error) => {
                let rollback_error = self.rollback(&before, &applied).await;
                let message = error.message;
                match rollback_error {
                    Some(rollback_error) => Ok(ProvisioningResult {
                        sync_id: envelope.sync_id.clone(),
                        status: ProvisioningStatus::RollbackFailed,
                        personal_provider_count: before.personal.providers.len(),
                        credential_count: envelope.credentials.len(),
                        config_revision: None,
                        error_message: Some(format!(
                            "{message}{ROLLBACK_FAILED_INFIX}{}",
                            rollback_error.message
                        )),
                        rolled_back: false,
                    }),
                    None => Ok(ProvisioningResult {
                        sync_id: envelope.sync_id.clone(),
                        status: ProvisioningStatus::Failed,
                        personal_provider_count: before.personal.providers.len(),
                        credential_count: envelope.credentials.len(),
                        config_revision: None,
                        error_message: Some(message),
                        rolled_back: true,
                    }),
                }
            }
        }
    }

    /// The try block of TS:82-131: register first and then write. Any failure will trigger a rollback by the caller.
    async fn commit(
        &self,
        envelope: &ProvisioningEnvelope,
        before: &BeforeState,
        personal_update: &StoreSnapshot,
        applied: &mut AppliedState,
        previous_state: Option<StateFile>,
        state_file: &Path,
    ) -> Result<ProvisioningResult, ServiceError> {
        // providerProvisioningTarget.ts:83-84 - Register first. If the placement fails, it must also be in the rollback collection.
        applied.settings = true;
        self.settings
            .update(&settings_patch(&envelope.account_settings), None)
            .await?;

        // TS:91-99 - Based on the allowlist key set of before, replace: delete the keys that are not included in the envelope.
        let incoming: HashMap<&str, &str> = envelope
            .credentials
            .iter()
            .map(|entry| (entry.key.as_str(), entry.value.as_str()))
            .collect();
        for (key, _) in &before.credentials {
            let value = incoming.get(key.as_str()).map(|value| (*value).to_string());
            // TS:96 - First register the target value to be written this time (default is null), and only know what to restore when it fails.
            applied.credentials.push((key.clone(), value.clone()));
            match value {
                None => self.credentials.delete(key).await?,
                Some(value) => self.credentials.save(key, &value).await?,
            }
        }

        applied.personal = true;
        self.write_personal_config(before, personal_update).await?;

        // TS:110-112 - Determine whether the default model is still available after refresh.
        let refreshed = self.refresh_registry().await?;
        if let Some(selection) = personal_update.default_model_selection.as_ref() {
            if !default_selection_supported(&refreshed, selection) {
                return Err(ServiceError::new(DEFAULT_MODEL_UNSUPPORTED_MESSAGE));
            }
        }

        let result = ProvisioningResult {
            sync_id: envelope.sync_id.clone(),
            status: ProvisioningStatus::Applied,
            personal_provider_count: personal_update.providers.len(),
            credential_count: envelope.credentials.len(),
            // TS:127 `snapshot.sourceRevisions.config` – The config revision number of the Rust personal store.
            config_revision: Some(refreshed.revision.to_string()),
            error_message: None,
            rolled_back: false,
        };
        // TS:130 - The state file writing is also within the transaction: if it cannot be written, it will be regarded as a failure and rolled back.
        write_state_file(state_file, append_state_record(previous_state, &result)).await?;
        Ok(result)
    }

    /// TS:102-108 - Checking and replacing must be done within the same lock to avoid overwriting other writes.
    async fn write_personal_config(
        &self,
        before: &BeforeState,
        target: &StoreSnapshot,
    ) -> Result<StoreSnapshot, ServiceError> {
        let expected_fingerprint = personal_fingerprint(&before.personal);
        self.store
            .mutate_from_disk(|snapshot| {
                if personal_fingerprint(snapshot) != expected_fingerprint {
                    return Err(ServiceError::new(PERSONAL_CONFIG_CHANGED_MESSAGE));
                }
                // revision is incremented uniformly by the store (it's a CAS token on the Rust side, not part of the TS file shape).
                *snapshot = target.clone();
                Ok(())
            })
            .await
    }

    /// `captureBeforeState`(providerProvisioningTarget.ts:179-204).
    async fn capture_before_state(
        &self,
        envelope: &ProvisioningEnvelope,
    ) -> Result<BeforeState, ServiceError> {
        // TS:183-186 - Two reads are performed concurrently; the Rust side uses reload() to ensure reading "according to the true value of the disk",
        // Align providerProvisioningSource.ts:73-109 "Do not treat memory degradation of bad files as authoritative".
        let (personal, settings) = tokio::join!(self.store.reload(), self.settings.get());
        let personal = personal?;
        let settings = settings?;

        let listed = list_provisioning_credential_keys(&self.paths).await?;
        let mut keys: Vec<String> = Vec::new();
        for key in PROVIDER_PROVISIONING_OAUTH_CREDENTIAL_KEYS
            .iter()
            .map(|key| (*key).to_string())
            .chain(listed)
            .chain(envelope.credentials.iter().map(|entry| entry.key.clone()))
        {
            if keys.contains(&key) {
                continue;
            }
            // TS:194-196 - Physical keys that are neither in the oauth allowlist nor the account-provider API key do not participate in the transaction.
            if !is_oauth_credential_key(&key)
                && !is_provider_provisioning_account_credential_key(&key)
            {
                continue;
            }
            keys.push(key);
        }

        let mut credentials = Vec::with_capacity(keys.len());
        for key in keys {
            credentials.push((key.clone(), self.credentials.load(&key).await?));
        }
        Ok(BeforeState {
            personal,
            settings,
            credentials,
        })
    }

    /// `rollback`(providerProvisioningTarget.ts:206-274) - fixed order:
    /// Personal Config → Credentials → Account Settings → refresh.
    async fn rollback(&self, before: &BeforeState, applied: &AppliedState) -> Option<ServiceError> {
        let mut errors: Vec<String> = Vec::new();

        if applied.personal {
            let before_fingerprint = personal_fingerprint(&before.personal);
            let expected_fingerprint = personal_fingerprint(&applied.personal_expected);
            let before_snapshot = before.personal.clone();
            if let Err(error) = self
                .store
                .mutate_from_disk(move |snapshot| {
                    let current = personal_fingerprint(snapshot);
                    if current == before_fingerprint {
                        return Ok(());
                    }
                    if current != expected_fingerprint {
                        return Err(ServiceError::new(PERSONAL_CONFIG_SKIP_ROLLBACK_MESSAGE));
                    }
                    // Rollback restores semantic content; the revision of the store is the CAS token on the Rust side and remains monotonically increasing.
                    // (There is no revision field in the TS file, see specs/rust-backend-replacement.md §8).
                    *snapshot = before_snapshot;
                    Ok(())
                })
                .await
            {
                errors.push(error.message);
            }
        }

        for (key, value) in &applied.credentials {
            let previous = before
                .credentials
                .iter()
                .find(|(existing, _)| existing == key)
                .and_then(|(_, value)| value.clone());
            match self.credentials.load(key).await {
                Err(error) => {
                    errors.push(error.message);
                    continue;
                }
                Ok(current) => {
                    if current == previous {
                        continue;
                    }
                    if current != *value {
                        errors.push(credential_skip_rollback_message(key));
                        continue;
                    }
                    let result = match &previous {
                        None => self.credentials.delete(key).await,
                        Some(previous) => self.credentials.save(key, previous).await,
                    };
                    if let Err(error) = result {
                        errors.push(error.message);
                    }
                }
            }
        }

        if applied.settings {
            match self.settings.get().await {
                Err(error) => errors.push(error.message),
                Ok(current) => {
                    let previous_settings = account_projection(&before.settings);
                    if previous_settings == account_projection(&current) {
                        // TS:251 - The write failure occurred before disk placement, and the target was already in the state before rollback.
                    } else if account_projection(&current)
                        != account_settings_json(&applied.settings_expected)
                    {
                        errors.push(SETTINGS_SKIP_ROLLBACK_MESSAGE.to_string());
                    } else if let Err(error) = self
                        .settings
                        .update(&restore_settings_patch(&before.settings), None)
                        .await
                    {
                        errors.push(error.message);
                    }
                }
            }
        }

        if !errors.is_empty() {
            return Some(ServiceError::new(errors.join(ERROR_JOINER)));
        }
        // TS:267-272 - After the rollback is successful, it still needs to be refreshed. If the refresh fails, the rollback fails.
        self.refresh_registry().await.err()
    }

    /// TS:110-112 and 268-269 refresh: personal store is the only source of truth on the Rust side,
    /// Refresh = re-read disk + broadcast `provider-settings`/`model-selection` view.
    async fn refresh_registry(&self) -> Result<StoreSnapshot, ServiceError> {
        let snapshot = self.store.reload().await?;
        self.notifier.publish(&snapshot).await;
        Ok(snapshot)
    }
}

#[async_trait]
impl Channel for ProviderProvisioningTargetService {
    async fn call(
        &self,
        ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        match command {
            "apply" => {
                ensure_trusted_host(ctx)?;
                // Positional parameter array (specs/rust-backend-replacement.md §1/§8.4): single-parameter object reads index 0.
                let args = ServiceArgs::new(args);
                let envelope = args.required_json(0, "envelope")?;
                let result = self.apply(&envelope).await?;
                Ok(WireValue::Json(
                    serde_json::to_value(result)
                        .map_err(|error| ServiceError::new(error.to_string()))?,
                ))
            }
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

/// Trusted host access control. TS replaces the entire channel at the transport layer (http.ts:105-116): not/// `apply` of `desktop-continuous` connection always throws the same Error. Rust side `ChannelContext`
/// `client_mode` has been carried, and `nc-code-server` can only consume one-time host capability in `/ws/host`
/// `DesktopContinuous` (crates/nc-code-server/src/lib.rs:224-246) is given later.
/// Therefore, what is done here is the same real trust chain, rather than placeholder judgment.
fn ensure_trusted_host(ctx: &ChannelContext) -> Result<(), ServiceError> {
    if ctx.client_mode != ClientMode::DesktopContinuous {
        return Err(ServiceError::new(TRUSTED_HOST_REQUIRED_MESSAGE));
    }
    Ok(())
}

//--------------------------------------------------------------------------------
// envelope / result verification (equivalent implementation of providerProvisioningEnvelopeSchema)
//--------------------------------------------------------------------------------

fn invalid_envelope(message: impl Into<String>) -> ServiceError {
    // TS's `schema.parse` throws ZodError; the name on the wire must be ZodError for the client to distinguish
    // "Illegal input parameters" and "server failure".
    ServiceError::invalid_params(message)
}

fn expect_object<'value>(
    value: &'value Value,
    path: &str,
) -> Result<&'value Map<String, Value>, ServiceError> {
    value
        .as_object()
        .ok_or_else(|| invalid_envelope(format!("{path}: expected an object")))
}

fn reject_unknown_keys(
    object: &Map<String, Value>,
    allowed: &[&str],
    path: &str,
) -> Result<(), ServiceError> {
    // zod `.strict()`: Unknown key fails directly, and cannot be passed by cropping fields.
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(invalid_envelope(format!(
                "{path}: unknown key is not allowed: {key}"
            )));
        }
    }
    Ok(())
}

fn require_field<'value>(
    object: &'value Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<&'value Value, ServiceError> {
    object
        .get(key)
        .ok_or_else(|| invalid_envelope(format!("{path}.{key}: required")))
}
/// `z.string().trim().min(1)`: must be a string, non-empty after removing blanks, and output the value after removing blanks.
fn require_trimmed(
    object: &Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<String, ServiceError> {
    let value = require_field(object, key, path)?;
    let text = trimmed_string(value)
        .ok_or_else(|| invalid_envelope(format!("{path}.{key}: expected a non-empty string")))?;
    Ok(text)
}

fn trimmed_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn require_array<'value>(
    object: &'value Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<&'value Vec<Value>, ServiceError> {
    require_field(object, key, path)?
        .as_array()
        .ok_or_else(|| invalid_envelope(format!("{path}.{key}: expected an array")))
}

fn parse_envelope(input: &Value) -> Result<ProvisioningEnvelope, ServiceError> {
    let root = expect_object(input, "$")?;
    reject_unknown_keys(
        root,
        &[
            "schemaVersion",
            "syncId",
            "personalConfig",
            "accountSettings",
            "credentials",
        ],
        "$",
    )?;
    // `z.literal(1)`
    match require_field(root, "schemaVersion", "$")? {
        Value::Number(number) if number.as_u64() == Some(PROVISIONING_SCHEMA_VERSION) => {}
        other => {
            return Err(invalid_envelope(format!(
                "$.schemaVersion: expected 1, got {other}"
            )))
        }
    }
    let sync_id = require_trimmed(root, "syncId", "$")?;
    let personal_config = parse_personal_config(expect_object(
        require_field(root, "personalConfig", "$")?,
        "$.personalConfig",
    )?)?;
    let account_settings = parse_account_settings(expect_object(
        require_field(root, "accountSettings", "$")?,
        "$.accountSettings",
    )?)?;
    let raw_credentials = require_array(root, "credentials", "$")?;
    if raw_credentials.len() > MAX_PROVISIONING_CREDENTIALS {
        return Err(invalid_envelope(format!(
            "$.credentials: at most {MAX_PROVISIONING_CREDENTIALS} entries"
        )));
    }
    let mut credentials = Vec::with_capacity(raw_credentials.len());
    for (index, entry) in raw_credentials.iter().enumerate() {
        let path = format!("$.credentials[{index}]");
        let object = expect_object(entry, &path)?;
        reject_unknown_keys(object, &["scope", "key", "value"], &path)?;
        let scope = match require_field(object, "scope", &path)?.as_str() {
            Some("oauth-session") => CredentialScope::OAuthSession,
            Some("account-provider") => CredentialScope::AccountProvider,
            _ => {
                return Err(invalid_envelope(format!(
                    "{path}.scope: expected \"oauth-session\" | \"account-provider\""
                )))
            }
        };
        let key = require_trimmed(object, "key", &path)?;
        let value = require_field(object, "value", &path)?
            .as_str()
            .ok_or_else(|| invalid_envelope(format!("{path}.value: expected a string")))?
            .to_string();
        credentials.push(CredentialEntry { scope, key, value });
    }
    Ok(ProvisioningEnvelope {
        sync_id,
        personal_config,
        account_settings,
        credentials,
    })
}

fn parse_personal_config(object: &Map<String, Value>) -> Result<PersonalConfig, ServiceError> {
    let path = "$.personalConfig";
    reject_unknown_keys(
        object,
        &[
            "providerConfigRules",
            "modelConfigRules",
            "providerOrder",
            "defaultModelSelection",
        ],
        path,
    )?;
    let provider_config_rules = expect_object(
        require_field(object, "providerConfigRules", path)?,
        &format!("{path}.providerConfigRules"),
    )?;
    reject_unknown_keys(
        provider_config_rules,
        &["providerRules"],
        &format!("{path}.providerConfigRules"),
    )?;
    let provider_rules = require_array(
        provider_config_rules,
        "providerRules",
        &format!("{path}.providerConfigRules"),
    )?
    .clone();

    let model_config_rules = expect_object(
        require_field(object, "modelConfigRules", path)?,
        &format!("{path}.modelConfigRules"),
    )?;
    reject_unknown_keys(
        model_config_rules,
        &["providerModelRules", "manualProviderModelRules"],
        &format!("{path}.modelConfigRules"),
    )?;
    let provider_model_rules = require_array(
        model_config_rules,
        "providerModelRules",
        &format!("{path}.modelConfigRules"),
    )?
    .clone();
    let manual_provider_model_rules = require_array(
        model_config_rules,
        "manualProviderModelRules",
        &format!("{path}.modelConfigRules"),
    )?
    .clone();

    let provider_order = match object.get("providerOrder") {
        None => None,
        Some(value) => {
            let items = value.as_array().ok_or_else(|| {
                invalid_envelope(format!("{path}.providerOrder: expected an array"))
            })?;
            let mut order = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                let text = trimmed_string(item).ok_or_else(|| {
                    invalid_envelope(format!(
                        "{path}.providerOrder[{index}]: expected a non-empty string"
                    ))
                })?;
                order.push(text);
            }
            Some(order)
        }
    };
    let default_model_selection = match object.get("defaultModelSelection") {
        None => None,
        Some(value) => Some(parse_model_selection(
            expect_object(value, &format!("{path}.defaultModelSelection"))?,
            &format!("{path}.defaultModelSelection"),
        )?),
    };

    Ok(PersonalConfig {
        provider_rules,
        provider_model_rules,
        manual_provider_model_rules,
        provider_order,
        default_model_selection,
    })
}

/// `modelSelectionSchema` (packages/shared/src/model-selection.ts:4-15): strict,
/// All three string fields are trimmed, and the output is the standardized selection written to the store.
fn parse_model_selection(object: &Map<String, Value>, path: &str) -> Result<Value, ServiceError> {
    reject_unknown_keys(object, &["providerId", "modelId", "options"], path)?;
    let mut selection = Map::new();
    selection.insert(
        "providerId".to_string(),
        Value::String(require_trimmed(object, "providerId", path)?),
    );
    selection.insert(
        "modelId".to_string(),
        Value::String(require_trimmed(object, "modelId", path)?),
    );
    if let Some(options) = object.get("options") {
        let options = expect_object(options, &format!("{path}.options"))?;
        reject_unknown_keys(options, &["reasoningLevel"], &format!("{path}.options"))?;
        let mut normalized = Map::new();
        if let Some(level) = options.get("reasoningLevel") {
            normalized.insert(
                "reasoningLevel".to_string(),
                Value::String(trimmed_string(level).ok_or_else(|| {
                    invalid_envelope(format!(
                        "{path}.options.reasoningLevel: expected a non-empty string"
                    ))
                })?),
            );
        }
        selection.insert("options".to_string(), Value::Object(normalized));
    }
    Ok(Value::Object(selection))
}

fn parse_account_settings(object: &Map<String, Value>) -> Result<AccountSettings, ServiceError> {
    let path = "$.accountSettings";
    reject_unknown_keys(
        object,
        &["providerFamilyDomain", "providerFamilyConnectionSelections"],
        path,
    )?;
    let provider_family_domain = match require_field(object, "providerFamilyDomain", path)? {
        Value::Null => None,
        value => {
            let text = trimmed_string(value).ok_or_else(|| {
                invalid_envelope(format!(
                    "{path}.providerFamilyDomain: expected \"zai\" | \"bigmodel\" | null"
                ))
            })?;
            if text != "zai" && text != "bigmodel" {
                return Err(invalid_envelope(format!(
                    "{path}.providerFamilyDomain: expected \"zai\" | \"bigmodel\" | null"
                )));
            }
            Some(text)
        }
    };
    let selections = expect_object(
        require_field(object, "providerFamilyConnectionSelections", path)?,
        &format!("{path}.providerFamilyConnectionSelections"),
    )?;
    for (family, selection) in selections {
        // `providerFamilyConnectionSelectionSettingsSchema`: There are only two keys zai/bigmodel.
        let item_path = format!("{path}.providerFamilyConnectionSelections.{family}");
        if family != "zai" && family != "bigmodel" {
            return Err(invalid_envelope(format!(
                "{item_path}: unknown key is not allowed"
            )));
        }
        validate_connection_selection(selection, &item_path)?;
    }
    Ok(AccountSettings {
        provider_family_domain,
        provider_family_connection_selections: Value::Object(selections.clone()),
    })
}

/// `providerFamilyConnectionSelectionSchema` (provider-family-connection-selection.ts:4-16).
fn validate_connection_selection(selection: &Value, path: &str) -> Result<(), ServiceError> {
    let object = expect_object(selection, path)?;
    match require_field(object, "kind", path)?.as_str() {
        Some("start-plan") | Some("individual-coding-plan") => {
            reject_unknown_keys(object, &["kind"], path)?;
        }
        Some("team-coding-plan") => {
            reject_unknown_keys(
                object,
                &["kind", "productId", "organizationId", "projectId"],
                path,
            )?;
            for key in ["productId", "organizationId", "projectId"] {
                require_trimmed(object, key, path)?;
            }
        }
        _ => {
            return Err(invalid_envelope(format!(
                "{path}.kind: expected \"start-plan\" | \"individual-coding-plan\" | \"team-coding-plan\""
            )))
        }
    }
    Ok(())
}

/// `validateCredentialEntries`(providerProvisioningTarget.ts:302-312).
pub fn validate_credential_entries(entries: &[CredentialEntry]) -> Result<(), ServiceError> {
    let mut seen: HashSet<&str> = HashSet::new();
    for entry in entries {
        if !seen.insert(entry.key.as_str()) {
            return Err(ServiceError::new(format!(
                "Repeat Provisioning Credential key: {}",
                entry.key
            )));
        }
        let allowed = match entry.scope {
            CredentialScope::OAuthSession => is_oauth_credential_key(&entry.key),
            CredentialScope::AccountProvider => {
                is_provider_provisioning_account_credential_key(&entry.key)
            }
        };
        if !allowed {
            return Err(ServiceError::new(format!(
                "Credential key not allowed to be synchronized: {}",
                entry.key
            )));
        }
    }
    Ok(())
}

fn is_oauth_credential_key(key: &str) -> bool {
    PROVIDER_PROVISIONING_OAUTH_CREDENTIAL_KEYS.contains(&key)
}

/// `isProviderProvisioningAccountCredentialKey` (provider-provisioning.ts:27-30).
/// Equivalent to `key.trim() == key && /^account-provider:.+:api-key$/.test(key)`;
/// Regular `.` does not span lines, so the middle section cannot contain line breaks.
pub fn is_provider_provisioning_account_credential_key(key: &str) -> bool {
    let normalized = key.trim();
    if normalized != key {
        return false;
    }
    let Some(middle) = key
        .strip_prefix(ACCOUNT_KEY_PREFIX)
        .and_then(|rest| rest.strip_suffix(ACCOUNT_KEY_SUFFIX))
    else {
        return false;
    };
    !middle.is_empty() && !middle.contains(['\n', '\r'])
}

//--------------------------------------------------------------------------------
// Personal Provider Config: envelope → Rust personal provider store
//--------------------------------------------------------------------------------

/// `parsePersonalConfig` (providerProvisioningTarget.ts:174-177): Envelope and local storage reuse
/// Formal Personal codec, no additional field list or mode judgment is created on the receiving end. The same codec on the Rust side is
/// The personal provider layer of `ProviderStore`, so here put the TS online shape (providerRules/
/// providerModelRules/manualProviderModelRules/providerOrder/defaultModelSelection)
/// Mapping to the store snapshot, the `config` of provider/model is retained as is and the fields are not clipped.
fn parse_personal_update(config: &PersonalConfig) -> Result<StoreSnapshot, ServiceError> {
    let mut rules: Vec<ProviderRule> = Vec::with_capacity(config.provider_rules.len());
    let mut seen: HashSet<String> = HashSet::new();
    for (index, raw) in config.provider_rules.iter().enumerate() {
        let path = format!("$.personalConfig.providerConfigRules.providerRules[{index}]");
        let object = expect_object(raw, &path)?;
        let provider_id = require_trimmed(object, "providerId", &path)?;
        if !seen.insert(provider_id.clone()) {
            // Uniqueness refinement of personalProviderConfigRulesSchema (rule-data-schema.ts:144-153).
            return Err(invalid_envelope(format!(
                "Duplicate providerId: {provider_id}"
            )));
        }
        let template_id = optional_nullable_trimmed(object, "templateId", &path)?;
        let provider_name = optional_nullable_trimmed(object, "providerName", &path)?;
        let enabled = match object.get("enabled") {
            None => true,
            Some(value) => value
                .as_bool()
                .ok_or_else(|| invalid_envelope(format!("{path}.enabled: expected a boolean")))?,
        };
        let provider_config = object
            .get("config")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid_envelope(format!("{path}.config: expected an object")))?;
        rules.push(ProviderRule {
            provider_id,
            template_id,
            provider_name,
            enabled,
            config: Value::Object(provider_config.clone()),
        });
    }

    let mut smart: Vec<ModelRule> = Vec::new();
    for (index, raw) in config.provider_model_rules.iter().enumerate() {
        smart.push(parse_model_rule(
            raw,
            &format!("$.personalConfig.modelConfigRules.providerModelRules[{index}]"),
        )?);
    }
    let mut manual: Vec<ModelRule> = Vec::new();
    for (index, raw) in config.manual_provider_model_rules.iter().enumerate() {
        manual.push(parse_model_rule(
            raw,
            &format!("$.personalConfig.modelConfigRules.manualProviderModelRules[{index}]"),
        )?);
    }
    // Conflict check for personalModelConfigRulesSchema (rule-data-schema.ts:76-92).
    for manual_rule in &manual {
        if smart.iter().any(|rule| {
            rule.provider_id == manual_rule.provider_id && rule.model_id == manual_rule.model_id
        }) {
            return Err(invalid_envelope(
                "The same Provider/Model cannot declare both smart and manual configuration",
            ));
        }
    }

    let providers = rules
        .into_iter()
        .map(|rule| build_provider_record(&rule, &smart, &manual))
        .collect();

    let snapshot = StoreSnapshot {
        providers,
        provider_order: config.provider_order.clone().unwrap_or_default(),
        default_model_selection: config.default_model_selection.clone(),
        ..StoreSnapshot::default()
    };
    Ok(snapshot)
}

fn parse_model_rule(raw: &Value, path: &str) -> Result<ModelRule, ServiceError> {
    let object = expect_object(raw, path)?;
    Ok(ModelRule {
        provider_id: require_trimmed(object, "providerId", path)?,
        model_id: require_trimmed(object, "modelId", path)?,
        config: object
            .get("config")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid_envelope(format!("{path}.config: expected an object")))?
            .clone()
            .into(),
    })
}

/// ModelConfigRules on the TS side hangs the rules on the provider; Rust's personal store also stores model records
/// Under provider, so model rules that do not correspond to providerRule are lazy in Rust like TS
/// (no resolution path will use it), will not be silently promoted to a new provider.
fn build_provider_record(
    rule: &ProviderRule,
    smart: &[ModelRule],
    manual: &[ModelRule],
) -> ProviderRecord {
    let mut ids = model_ids_from_config(&rule.config);
    for model_id in smart
        .iter()
        .chain(manual.iter())
        .filter(|model| model.provider_id == rule.provider_id)
        .map(|model| model.model_id.clone())
    {
        if !ids.contains(&model_id) {
            ids.push(model_id);
        }
    }
    let models =
        ids.into_iter()
            .map(|model_id| {
                // Manual rules for the same (provider, model) take precedence over smart rules; if there are multiple rules with the same name, the last one will be used.
                let manual_rule = manual.iter().rev().find(|model| {
                    model.provider_id == rule.provider_id && model.model_id == model_id
                });
                let smart_rule = smart.iter().rev().find(|model| {
                    model.provider_id == rule.provider_id && model.model_id == model_id
                });
                let (config, use_recommended) = match (manual_rule, smart_rule) {
                    (Some(model), _) => (model.config.clone(), false),
                    (None, Some(model)) => (model.config.clone(), true),
                    (None, None) => (default_model_config(), true),
                };
                let enabled = config
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                ModelRecord {
                    model_id,
                    config,
                    enabled,
                    use_recommended,
                }
            })
            .collect();
    ProviderRecord {
        provider_name: rule
            .provider_name
            .clone()
            .unwrap_or_else(|| rule.provider_id.clone()),
        provider_id: rule.provider_id.clone(),
        template_id: rule.template_id.clone(),
        enabled: rule.enabled,
        config: rule.config.clone(),
        models,
    }
}

struct ProviderRule {
    provider_id: String,
    template_id: Option<String>,
    provider_name: Option<String>,
    enabled: bool,
    config: Value,
}

struct ModelRule {
    provider_id: String,
    model_id: String,
    config: Value,
}

/// `idSchema.nullable().optional()`.
fn optional_nullable_trimmed(
    object: &Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<Option<String>, ServiceError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => trimmed_string(value).map(Some).ok_or_else(|| {
            invalid_envelope(format!("{path}.{key}: expected a non-empty string or null"))
        }),
    }
}

/// `samePersonalConfig` (providerProvisioningTarget.ts:276-284): used by TS
/// `JSON.stringify(encodeProviderConfigFile(x))` does CAS comparison. Rust's personal provider file
/// The shape is determined by `StoreSnapshot`, so the fingerprint takes the "semantic content" projection, excluding the store's own revision
/// Token (this field does not exist in the TS file).
fn personal_fingerprint(snapshot: &StoreSnapshot) -> String {
    let projected = json!({
        "providers": snapshot.providers,
        "providerOrder": snapshot.provider_order,
        "defaultModelSelection": snapshot.default_model_selection,
    });
    serde_json::to_string(&projected).unwrap_or_default()
}

/// Rust peer check for `validateSelection`: the default model must be executable in the personal store after writing
/// (provider is enabled, no configuration issues, model exists and is enabled).
/// Model "executable" = effective configuration is readable (can read optionsSpecs and other specifications). "Available" determination of default model
/// (TS validateSelection.ok, providerProvisioningTarget.ts:113-120) must contain it: the configuration cannot be read
/// Even if the model is enabled, it cannot actually be loaded and is not considered supported.
fn model_is_executable(model: &ModelRecord) -> bool {
    !model_effective_config(model).is_null()
}
fn default_selection_supported(snapshot: &StoreSnapshot, selection: &Value) -> bool {
    let Some(provider_id) = selection.get("providerId").and_then(Value::as_str) else {
        return false;
    };
    let Some(model_id) = selection.get("modelId").and_then(Value::as_str) else {
        return false;
    };
    snapshot
        .provider(provider_id)
        .filter(|provider| provider.enabled)
        .filter(|provider| {
            provider_issues(&provider_effective_config(provider, snapshot)).is_empty()
        })
        .and_then(|provider| {
            provider.models.iter().find(|model| {
                model.model_id == model_id && model.enabled && model_is_executable(model)
            })
        })
        .is_some()
}

//--------------------------------------------------------------------------------
// Set up patches and projections
//--------------------------------------------------------------------------------

/// TS:85-89 —— `providerFamilyDomain ?? undefined`. ProxyChannel takes JSON,
/// `undefined` will be discarded, so when domain is null, the patch **does not contain the key** (maintains the current value),
/// Instead of clearing it.
fn settings_patch(account: &AccountSettings) -> Value {
    let mut patch = Map::new();
    if let Some(domain) = &account.provider_family_domain {
        patch.insert(
            "providerFamilyDomain".to_string(),
            Value::String(domain.clone()),
        );
    }
    patch.insert(
        "providerFamilyConnectionSelections".to_string(),
        account.provider_family_connection_selections.clone(),
    );
    Value::Object(patch)
}

/// TS:255-258 - When rolling back, write back the two items of before as they are; the default key is also not included (no additional clearing).
fn restore_settings_patch(settings: &Value) -> Value {
    let mut patch = Map::new();
    if let Some(domain) = settings.get("providerFamilyDomain") {
        patch.insert("providerFamilyDomain".to_string(), domain.clone());
    }
    patch.insert(
        "providerFamilyConnectionSelections".to_string(),
        settings
            .get("providerFamilyConnectionSelections")
            .cloned()
            .unwrap_or_else(|| json!({})),
    );
    Value::Object(patch)
}

/// `toProvisioningAccountSettings` (providerProvisioningTarget.ts:286-293).
fn account_projection(settings: &Value) -> Value {
    json!({
        "providerFamilyDomain": settings
            .get("providerFamilyDomain")
            .cloned()
            .unwrap_or(Value::Null),
        "providerFamilyConnectionSelections": settings
            .get("providerFamilyConnectionSelections")
            .cloned()
            .unwrap_or_else(|| json!({})),
    })
}

fn account_settings_json(account: &AccountSettings) -> Value {
    json!({
        "providerFamilyDomain": account
            .provider_family_domain
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
        "providerFamilyConnectionSelections": account.provider_family_connection_selections.clone(),
    })
}

//--------------------------------------------------------------------------------
// Idempotent state file (the record shape of schemaVersion:1 is exactly the same as TS)
//--------------------------------------------------------------------------------

/// `readStateFile`(providerProvisioningTarget.ts:314-337): Parsing failed / Version does not match /
/// Record shapes that do not match will be downgraded to "no history", but other legal records will never be discarded because of a bad record.
async fn read_state_file(file: &Path) -> Option<StateFile> {
    let raw = read_text_if_exists(file).await.ok()??;
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    if parsed.get("schemaVersion").and_then(Value::as_u64) != Some(PROVISIONING_SCHEMA_VERSION) {
        return None;
    }
    let raw_records: Vec<Value> = match parsed.get("records").and_then(Value::as_array) {
        Some(records) => records.clone(),
        // Compatible with older single record files (TS:322-324).
        None => match (parsed.get("syncId"), parsed.get("result")) {
            (Some(sync_id), Some(result)) => vec![json!({ "syncId": sync_id, "result": result })],
            _ => Vec::new(),
        },
    };
    let mut records = Vec::new();
    for record in raw_records {
        let Some(object) = record.as_object() else {
            continue;
        };
        let Some(sync_id) = object
            .get("syncId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
        else {
            continue;
        };
        let Some(result) = object.get("result").and_then(parse_result) else {
            continue;
        };
        records.push(StateRecord { sync_id, result });
    }
    Some(StateFile {
        schema_version: PROVISIONING_SCHEMA_VERSION,
        records,
    })
}

/// `providerProvisioningResultSchema.safeParse`: `.strict()`, the field name must be consistent with the TS bytes.
pub fn parse_result(value: &Value) -> Option<ProvisioningResult> {
    let object = value.as_object()?;
    for key in object.keys() {
        if ![
            "syncId",
            "status",
            "personalProviderCount",
            "credentialCount",
            "configRevision",
            "errorMessage",
            "rolledBack",
        ]
        .contains(&key.as_str())
        {
            return None;
        }
    }
    let status = match object.get("status")?.as_str()? {
        "applied" => ProvisioningStatus::Applied,
        "already-applied" => ProvisioningStatus::AlreadyApplied,
        "unsupported" => ProvisioningStatus::Unsupported,
        "failed" => ProvisioningStatus::Failed,
        "rollback_failed" => ProvisioningStatus::RollbackFailed,
        _ => return None,
    };
    Some(ProvisioningResult {
        sync_id: object.get("syncId").and_then(trimmed_string)?,
        status,
        personal_provider_count: nonnegative_int(object.get("personalProviderCount")?)?,
        credential_count: nonnegative_int(object.get("credentialCount")?)?,
        config_revision: match object.get("configRevision") {
            None => None,
            Some(value) => Some(trimmed_string(value)?),
        },
        error_message: match object.get("errorMessage") {
            None => None,
            Some(value) => Some(trimmed_string(value)?),
        },
        rolled_back: object.get("rolledBack").and_then(Value::as_bool)?,
    })
}

fn nonnegative_int(value: &Value) -> Option<usize> {
    value.as_u64().map(|number| number as usize)
}

/// `appendStateRecord`(providerProvisioningTarget.ts:339-351): Overwrite old record with same syncId.
fn append_state_record(previous: Option<StateFile>, result: &ProvisioningResult) -> StateFile {
    let records = previous
        .map(|state| {
            state
                .records
                .into_iter()
                .filter(|record| record.sync_id != result.sync_id)
                .collect()
        })
        .unwrap_or_default();
    let mut records: Vec<StateRecord> = records;
    records.push(StateRecord {
        sync_id: result.sync_id.clone(),
        result: result.clone(),
    });
    StateFile {
        schema_version: PROVISIONING_SCHEMA_VERSION,
        records,
    }
}

/// `writeStateFile`(providerProvisioningTarget.ts:353-355):/// `atomicWritePrivateTextFile(path, JSON.stringify(state, null, 2) + "\n")`.
async fn write_state_file(file: &Path, state: StateFile) -> Result<(), ServiceError> {
    let contents = format!(
        "{}\n",
        serde_json::to_string_pretty(&state)
            .map_err(|error| ServiceError::new(error.to_string()))?
    );
    atomic_write_text(file, &contents).await?;
    set_private_permissions(file).await;
    Ok(())
}

/// Consistent with TS `atomicWritePrivateTextFile(..., { mode: 0o600 })` (Windows inherits from ACL).
async fn set_private_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// `listProviderProvisioningCredentialKeys` (providerProvisioningSource.ts:166-183):
/// Only the physical keys in the allowlist are enumerated for the target end to implement replace-allowlist deletion semantics.
async fn list_provisioning_credential_keys(paths: &AppPaths) -> Result<Vec<String>, ServiceError> {
    let file = paths.app_config_dir().join(CREDENTIAL_FILE_NAME);
    let Some(raw) = read_text_if_exists(&file).await? else {
        return Ok(Vec::new());
    };
    let parsed: Value = serde_json::from_str(&raw)?;
    let object = parsed
        .as_object()
        .ok_or_else(|| ServiceError::new("Credential Store must be a JSON object"))?;
    Ok(object
        .keys()
        .filter(|key| {
            is_oauth_credential_key(key) || is_provider_provisioning_account_credential_key(key)
        })
        .cloned()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::EventBus;
    use nc_code_services_config::credential::CredentialService;
    use nc_code_services_config::setting::SettingService;
    fn temp_paths(tag: &str) -> AppPaths {
        let name = format!("zcode-prov-e2e-{}-{}", std::process::id(), tag);
        let root = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&root).expect("mkdir");
        AppPaths::new(root.join("home"), root.join("data"))
    }

    fn service(tag: &str) -> Arc<ProviderProvisioningTargetService> {
        let paths = temp_paths(tag);
        let store = ProviderStore::new(paths.clone());
        let notifier = ProviderSettingsService::new(Arc::clone(&store), EventBus::default());
        let settings = Arc::new(SettingService::new(paths.clone()));
        let credentials = Arc::new(CredentialService::new(paths.clone()));
        ProviderProvisioningTargetService::new(paths, store, settings, credentials, notifier)
    }

    fn ctx(mode: ClientMode) -> ChannelContext {
        ChannelContext::new("prov-e2e", mode)
    }

    #[tokio::test]
    async fn apply_requires_a_trusted_desktop_host() {
        let service = service("gate");
        let remote = ctx(ClientMode::WebRemoteReplayable);
        let args = vec![WireValue::Json(json!({}))];
        let error = service
            .call(&remote, "apply", args, CancelToken::new())
            .await
            .expect_err("untrusted host must be rejected");
        assert!(error.to_string().contains(TRUSTED_HOST_REQUIRED_MESSAGE));
    }

    #[tokio::test]
    async fn apply_rejects_invalid_envelope_fail_closed() {
        let service = service("envelope");
        let trusted = ctx(ClientMode::DesktopContinuous);
        let args = vec![WireValue::Json(json!({}))];
        let error = service
            .call(&trusted, "apply", args, CancelToken::new())
            .await
            .expect_err("invalid envelope must fail closed");
        assert!(!error.to_string().is_empty());
        let unknown = service
            .call(&trusted, "bogus", vec![], CancelToken::new())
            .await
            .expect_err("unknown method");
        assert!(unknown.to_string().contains("Method not found"));
    }
}
