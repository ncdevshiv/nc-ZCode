//! `plugin-management` channel — dispatch layer for the plugin engine.
//!
//! Members and result shapes come from `IPluginManagementService`
//! (`packages/services/src/plugins/pluginManagement.ts`) and its CLI protocol handlers
//! (`bootstrap/src/nc-code-protocol/plugins.ts` + `plugin-reference-catalog.ts`). The
//! engine itself lives in [`crate::orchestration`]; this module only parses parameters,
//! bridges cancellation and projects the results.
//!
//! Ownership notes:
//! - every request loads its own [`PluginContext`] (config layers + storage root) from
//!   `params.workspace.workspacePath`, so two workspaces never share merged config;
//! - long-running members register their cancellation flag under the client's
//!   `operationId` ([`OperationRegistry`]); `cancelPluginOperation` flips that same flag;
//! - the skill reference catalog delegates to the `skills` channel instead of
//!   re-implementing the scan policy (single owner for skill discovery).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure, EventSubscription};
use nc_code_rpc::WireValue;
use nc_code_service_core::{AppPaths, ServiceArgs, ServiceError};
use serde_json::{json, Map, Value};

use crate::diagnostics::{to_protocol_values, Severity};
use crate::fetch::Fetcher;
use crate::orchestration::{
    add_plugin_marketplace, configure_plugin, describe_plugin, install_plugin, list_plugins_value,
    overview_data, remove_plugin_marketplace, reset_plugin_config, restore_builtin_core,
    set_plugin_enabled_for_plugin, uninstall_plugin, update_plugin, update_plugin_marketplace,
    validate_plugin, InstallProtocolOutcome, PluginContext,
};
use crate::project::to_plugin_info;
use crate::reference;
use crate::types::OFFICIAL_MARKETPLACE_ID;

use crate::operations::{bridge_cancel, OperationRegistry};

/// `SUGGESTED_PLUGIN_MARKETPLACE_REFRESH_TIMEOUT_MS` (`plugin-reference-catalog.ts:63`).
const SUGGESTED_PLUGIN_REFRESH_TIMEOUT: Duration = Duration::from_millis(10_000);

const ERROR_PLUGIN_OPERATION_CANCELLED: &str = "plugin_operation_cancelled";
const MESSAGE_PLUGIN_OPERATION_CANCELLED: &str = "Plug-in operation cancelled";
const ERROR_MARKETPLACE_REFRESH_FAILED: &str = "marketplace_refresh_failed";
const MESSAGE_MARKETPLACE_REFRESH_TIMEOUT: &str =
    "Refresh zcode-plugins-official timeout (10000 ms)";
const ERROR_SUGGESTED_UNTRUSTED_SOURCE: &str = "plugin_suggested_reference_untrusted_source";
const MESSAGE_SUGGESTED_UNTRUSTED_SOURCE: &str =
    "Recommended plugins are not from the trusted official zcode-plugins-official source";
const ERROR_SUGGESTED_CONFLICT: &str = "plugin_suggested_reference_conflict";
const MESSAGE_SUGGESTED_CONFLICT: &str = "The recommended plug-in has a name conflict and cannot be automatically installed or referenced";
const ERROR_SUGGESTED_NOT_LISTED: &str = "plugin_suggested_reference_not_listed";
const MESSAGE_SUGGESTED_NOT_LISTED: &str =
    "The plug-in was not found in the refreshed official directory";

/// `validateZCodePlugin` compatibility projection (`plugins.ts:479-483`).
const COMPATIBILITY_RUNNABLE: [&str; 5] =
    ["skills", "commands", "hooks", "mcpServers", "userConfig"];
const COMPATIBILITY_DIAGNOSTIC_ONLY: [&str; 5] = [
    "agents",
    "lspServers",
    "outputStyles",
    "channels",
    "settings",
];
const COMPATIBILITY_UNSUPPORTED: [&str; 5] = ["mcpb", "dxt", "npm", "hostPattern", "pathPattern"];

pub struct PluginManagementService {
    paths: AppPaths,
    fetcher: Fetcher,
    /// The `skills` channel: the single owner of skill discovery.
    skills: Arc<dyn Channel>,
    operations: OperationRegistry,
}

impl PluginManagementService {
    pub fn new(paths: AppPaths, skills: Arc<dyn Channel>) -> Arc<Self> {
        Arc::new(Self {
            paths,
            fetcher: Fetcher::new(),
            skills,
            operations: OperationRegistry::default(),
        })
    }

    /// Load the per-request engine context (`resolvePluginContext`).
    async fn context(&self, params: &Map<String, Value>) -> Result<PluginContext, ServiceError> {
        let workspace_path = workspace_path(params)?;
        let config_scope = config_scope(params)?;
        PluginContext::load(&self.paths, &workspace_path, config_scope.as_deref()).await
    }

    async fn list_plugins(&self, params: &Map<String, Value>) -> Result<Value, ServiceError> {
        let ctx = self.context(params).await?;
        let outcome = ctx.discover().await;
        Ok(list_plugins_value(&ctx, &outcome))
    }

    async fn plugins_overview(&self, params: &Map<String, Value>) -> Result<Value, ServiceError> {
        let ctx = self.context(params).await?;
        let overview = overview_data(&ctx).await;
        Ok(json!({
            "marketplaces": overview.marketplaces,
            "availablePlugins": overview.available,
            "installedPlugins": overview.installed,
            "restorableBuiltins": overview.restorable,
            "diagnostics": to_protocol_values(&overview.diagnostics),
            "capability": { "supported": true },
        }))
    }

    async fn add_marketplace(
        &self,
        params: &Map<String, Value>,
        cancel: &CancelToken,
    ) -> Result<Value, ServiceError> {
        let ctx = self.context(params).await?;
        let source = required_string(params, "source")?;
        let dry_run = optional_bool(params, "dryRun")?.unwrap_or(false);
        let ticket = self
            .operations
            .begin(optional_string(params, "operationId")?.as_deref());
        let _bridge = bridge_cancel(cancel, &ticket);
        let result =
            add_plugin_marketplace(&self.fetcher, &ctx, &source, dry_run, &ticket.flag()).await;
        self.operations.finish(&ticket);
        let marketplace = result?;
        Ok(json!({ "marketplace": marketplace, "diagnostics": [] }))
    }

    async fn remove_marketplace(&self, params: &Map<String, Value>) -> Result<Value, ServiceError> {
        let ctx = self.context(params).await?;
        let marketplace = required_string(params, "marketplace")?;
        remove_plugin_marketplace(&ctx, &marketplace).await?;
        Ok(json!({ "diagnostics": [] }))
    }

    async fn update_marketplace(
        &self,
        params: &Map<String, Value>,
        cancel: &CancelToken,
    ) -> Result<Value, ServiceError> {
        let ctx = self.context(params).await?;
        let marketplace = optional_string(params, "marketplace")?;
        let ticket = self
            .operations
            .begin(optional_string(params, "operationId")?.as_deref());
        let _bridge = bridge_cancel(cancel, &ticket);
        let result =
            update_plugin_marketplace(&self.fetcher, &ctx, marketplace.as_deref(), &ticket.flag())
                .await;
        self.operations.finish(&ticket);
        let (marketplaces, diagnostics) = result?;
        Ok(json!({
            "marketplaces": marketplaces,
            "diagnostics": to_protocol_values(&diagnostics),
        }))
    }

    async fn install(
        &self,
        params: &Map<String, Value>,
        cancel: &CancelToken,
    ) -> Result<Value, ServiceError> {
        let ctx = self.context(params).await?;
        let plugin_name = required_string(params, "pluginName")?;
        let marketplace = required_string(params, "marketplace")?;
        let dry_run = optional_bool(params, "dryRun")?.unwrap_or(false);
        let ticket = self
            .operations
            .begin(optional_string(params, "operationId")?.as_deref());
        let _bridge = bridge_cancel(cancel, &ticket);
        let result = install_plugin(
            &self.fetcher,
            &ctx,
            &marketplace,
            &plugin_name,
            dry_run,
            &ticket.flag(),
        )
        .await;
        self.operations.finish(&ticket);
        Ok(install_result_value(&result?))
    }

    async fn uninstall(&self, params: &Map<String, Value>) -> Result<Value, ServiceError> {
        let ctx = self.context(params).await?;
        let plugin_id = optional_string(params, "pluginId")?;
        let plugin_name = optional_string(params, "pluginName")?;
        let marketplace = optional_string(params, "marketplace")?;
        let remove_cache = optional_bool(params, "removeCache")?;
        let removed = uninstall_plugin(
            &ctx,
            plugin_id.as_deref(),
            plugin_name.as_deref(),
            marketplace.as_deref(),
            remove_cache,
        )
        .await?;
        Ok(match removed {
            Some(removed) => json!({ "removedPlugin": removed, "diagnostics": [] }),
            None => json!({ "diagnostics": [] }),
        })
    }

    async fn update(
        &self,
        params: &Map<String, Value>,
        cancel: &CancelToken,
    ) -> Result<Value, ServiceError> {
        let ctx = self.context(params).await?;
        let plugin_id = optional_string(params, "pluginId")?;
        let marketplace = optional_string(params, "marketplace")?;
        let ticket = self.operations.begin(None);
        let _bridge = bridge_cancel(cancel, &ticket);
        let result = update_plugin(
            &self.fetcher,
            &ctx,
            plugin_id.as_deref(),
            marketplace.as_deref(),
            &ticket.flag(),
        )
        .await;
        self.operations.finish(&ticket);
        Ok(install_result_value(&result?))
    }

    async fn restore_builtin(&self, params: &Map<String, Value>) -> Result<Value, ServiceError> {
        let ctx = self.context(params).await?;
        let plugin_id = required_string(params, "pluginId")?;
        restore_builtin_core(&ctx, &plugin_id).await?;
        Ok(json!({ "pluginId": plugin_id, "diagnostics": [] }))
    }

    async fn configure(&self, params: &Map<String, Value>) -> Result<Value, ServiceError> {
        let ctx = self.context(params).await?;
        let plugin_id = required_string(params, "pluginId")?;
        let options = required_object(params, "options")?;
        let clear_option_keys = optional_string_array(params, "clearOptionKeys")?;
        let scope = config_scope(params)?;
        let dry_run = optional_bool(params, "dryRun")?.unwrap_or(false);
        configure_plugin(
            &ctx,
            &plugin_id,
            &options,
            clear_option_keys.as_ref(),
            scope.as_deref(),
            dry_run,
        )
        .await?;
        Ok(json!({ "pluginId": plugin_id, "diagnostics": [] }))
    }

    async fn reset_config(&self, params: &Map<String, Value>) -> Result<Value, ServiceError> {
        let ctx = self.context(params).await?;
        let plugin_id = required_string(params, "pluginId")?;
        let scope = config_scope(params)?;
        reset_plugin_config(&ctx, &plugin_id, scope.as_deref()).await?;
        Ok(json!({ "pluginId": plugin_id, "diagnostics": [] }))
    }

    async fn validate(
        &self,
        params: &Map<String, Value>,
        cancel: &CancelToken,
    ) -> Result<Value, ServiceError> {
        let ctx = self.context(params).await?;
        let marketplace = optional_string(params, "marketplace")?;
        let plugin_name = optional_string(params, "pluginName")?;
        let source = optional_string(params, "source")?;
        let ticket = self.operations.begin(None);
        let _bridge = bridge_cancel(cancel, &ticket);
        let diagnostics = validate_plugin(
            &self.fetcher,
            &ctx,
            marketplace.as_deref(),
            plugin_name.as_deref(),
            source.as_deref(),
            &ticket.flag(),
        )
        .await;
        self.operations.finish(&ticket);
        let ok = diagnostics
            .iter()
            .all(|diagnostic| diagnostic.severity != Severity::Error);
        Ok(json!({
            "ok": ok,
            "diagnostics": to_protocol_values(&diagnostics),
            "compatibility": {
                "runnable": COMPATIBILITY_RUNNABLE,
                "diagnosticOnly": COMPATIBILITY_DIAGNOSTIC_ONLY,
                "unsupported": COMPATIBILITY_UNSUPPORTED,
            },
        }))
    }

    async fn describe(&self, params: &Map<String, Value>) -> Result<Value, ServiceError> {
        let ctx = self.context(params).await?;
        let marketplace = required_string(params, "marketplace")?;
        let plugin_name = required_string(params, "pluginName")?;
        let ticket = self.operations.begin(None);
        let result = describe_plugin(
            &self.fetcher,
            &ctx,
            &marketplace,
            &plugin_name,
            &ticket.flag(),
        )
        .await;
        self.operations.finish(&ticket);
        let diagnostics = to_protocol_values(&result.diagnostics);
        let mut value = Map::new();
        value.insert("components".into(), Value::Array(result.components));
        // TS spreads optional keys only when present (`plugins.ts:508-513`).
        if !diagnostics.is_empty() {
            value.insert("diagnostics".into(), Value::Array(diagnostics));
        }
        if let Some(metadata) = result.metadata {
            value.insert("metadata".into(), metadata);
        }
        Ok(Value::Object(value))
    }

    async fn set_enabled(
        &self,
        params: &Map<String, Value>,
        cancel: &CancelToken,
    ) -> Result<Value, ServiceError> {
        let ctx = self.context(params).await?;
        let plugin_selector = required_string(params, "pluginId")?;
        let enabled = required_bool(params, "enabled")?;
        let scope = config_scope(params)?;
        let ticket = self
            .operations
            .begin(optional_string(params, "operationId")?.as_deref());
        let _bridge = bridge_cancel(cancel, &ticket);
        let result =
            set_plugin_enabled_for_plugin(&ctx, &plugin_selector, enabled, scope.as_deref()).await;
        self.operations.finish(&ticket);
        let (plugin, enabled) = result?;
        let mut info = to_plugin_info(&plugin, &ctx.view, &ctx.paths);
        if let Some(object) = info.as_object_mut() {
            // `setPluginEnabled` projects the plugin without a config view, then writes
            // `enabledSource: params.scope ?? "user"` (plugins.ts:244-250).
            object.remove("optionSources");
            object.remove("rootSource");
            object.insert("enabled".into(), Value::Bool(enabled));
            object.insert(
                "enabledSource".into(),
                Value::String(scope.unwrap_or_else(|| "user".to_string())),
            );
        }
        Ok(json!({ "plugin": info, "enabled": enabled }))
    }

    async fn cancel_operation(&self, params: &Map<String, Value>) -> Result<Value, ServiceError> {
        // No workspace in this schema: cancellation is keyed by operation id alone.
        let operation_id = required_string(params, "operationId")?;
        let cancelled = self.operations.cancel(&operation_id);
        Ok(json!({ "operationId": operation_id, "cancelled": cancelled }))
    }

    async fn plugin_reference_catalog(
        &self,
        params: &Map<String, Value>,
    ) -> Result<Value, ServiceError> {
        let ctx = self.context(params).await?;
        let outcome = ctx.discover().await;
        let overview = overview_data(&ctx).await;
        // The plugin-management consumer requests `plugins/referenceCatalogWithCategory`
        // first (pluginReferenceCatalogRequest.ts), so `category` is part of this surface.
        Ok(reference::reference_catalog_value(
            &overview,
            &outcome.plugins,
            true,
        ))
    }

    /// Skill discovery is owned by the `skills` channel; this member only re-projects its
    /// rows into the identifiers-only reference shape (`skill-reference-catalog.ts`).
    async fn skill_reference_catalog(
        &self,
        ctx: &ChannelContext,
        params: &Map<String, Value>,
        cancel: &CancelToken,
    ) -> Result<Value, ServiceError> {
        let workspace_path = workspace_path(params)?;
        let args = vec![WireValue::Json(json!({ "workspacePath": workspace_path }))];
        let listed = self
            .skills
            .call(ctx, "list", args, cancel.clone())
            .await
            .map_err(service_error_from_failure)?;
        let WireValue::Json(value) = listed else {
            return Err(ServiceError::new("skills list returned a non-JSON payload"));
        };
        Ok(reference::skill_catalog_value(&value))
    }

    async fn resolve_suggested_reference(
        &self,
        params: &Map<String, Value>,
        cancel: &CancelToken,
    ) -> Result<Value, ServiceError> {
        let ctx = self.context(params).await?;
        let stable_id = required_string(params, "stableId")?.trim().to_string();
        let operation_id = required_string(params, "operationId")?;
        let ticket = self.operations.begin(Some(&operation_id));
        let _bridge = bridge_cancel(cancel, &ticket);
        let result = self
            .resolve_suggested(&ctx, &stable_id, &operation_id, &ticket.flag())
            .await;
        self.operations.finish(&ticket);
        result
    }

    /// `resolveSuggestedPluginReference` (`plugin-reference-catalog.ts:66`): missing rows
    /// must refresh the official marketplace before they may be offered for install.
    async fn resolve_suggested(
        &self,
        ctx: &PluginContext,
        stable_id: &str,
        operation_id: &str,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<Value, ServiceError> {
        let (plugin_name, marketplace) = reference::split_suggested_id(stable_id);
        if plugin_name.is_empty()
            || marketplace != OFFICIAL_MARKETPLACE_ID
            || !reference::is_trusted_suggested_id(stable_id)
        {
            return Ok(unavailable_suggested(
                stable_id,
                ERROR_SUGGESTED_UNTRUSTED_SOURCE,
                MESSAGE_SUGGESTED_UNTRUSTED_SOURCE,
            ));
        }
        if let Some(entry) = read_reference_entry(ctx, stable_id).await {
            return Ok(self
                .suggested_reference_value(ctx, stable_id, &plugin_name, &marketplace, &entry)
                .await);
        }

        // The old process will only return missing after the official Marketplace refresh is completed; the progress will be sent first during the network waiting period.
        // Let the UI of the same operation enter loading (TS notify "refreshing" here).
        self.operations.publish_progress(operation_id, "refreshing");

        let refresh = tokio::time::timeout(
            SUGGESTED_PLUGIN_REFRESH_TIMEOUT,
            update_plugin_marketplace(&self.fetcher, ctx, Some(OFFICIAL_MARKETPLACE_ID), cancelled),
        )
        .await;
        match refresh {
            Err(_) => {
                return Ok(unavailable_suggested(
                    stable_id,
                    ERROR_MARKETPLACE_REFRESH_FAILED,
                    MESSAGE_MARKETPLACE_REFRESH_TIMEOUT,
                ))
            }
            Ok(Err(error)) => {
                return Ok(self.refresh_failure_value(stable_id, cancelled, &error.message))
            }
            Ok(Ok((_, diagnostics))) => {
                if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                    return Ok(unavailable_suggested(
                        stable_id,
                        ERROR_PLUGIN_OPERATION_CANCELLED,
                        MESSAGE_PLUGIN_OPERATION_CANCELLED,
                    ));
                }
                if let Some(failure) = diagnostics.iter().find(|diagnostic| {
                    diagnostic.plugin_id.as_deref() == Some(OFFICIAL_MARKETPLACE_ID)
                }) {
                    return Ok(self.refresh_failure_value(stable_id, cancelled, &failure.message));
                }
            }
        }
        if let Some(entry) = read_reference_entry(ctx, stable_id).await {
            return Ok(self
                .suggested_reference_value(ctx, stable_id, &plugin_name, &marketplace, &entry)
                .await);
        }
        let overview = overview_data(ctx).await;
        let candidate = overview
            .available
            .iter()
            .find(|item| item.get("id").and_then(Value::as_str) == Some(stable_id));
        let Some(candidate) = candidate else {
            return Ok(unavailable_suggested(
                stable_id,
                ERROR_SUGGESTED_NOT_LISTED,
                MESSAGE_SUGGESTED_NOT_LISTED,
            ));
        };
        if candidate.get("name").and_then(Value::as_str) != Some(plugin_name.as_str())
            || candidate.get("marketplace").and_then(Value::as_str) != Some(OFFICIAL_MARKETPLACE_ID)
        {
            return Ok(unavailable_suggested(
                stable_id,
                ERROR_SUGGESTED_NOT_LISTED,
                MESSAGE_SUGGESTED_NOT_LISTED,
            ));
        }
        let listing = candidate.get("listing").cloned();
        let icon = listing
            .as_ref()
            .and_then(|listing| listing.get("icon"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|icon| !icon.is_empty())
            .map(str::to_string);
        let mut value = Map::new();
        value.insert("stableId".into(), Value::String(stable_id.to_string()));
        value.insert("status".into(), Value::String("missing".into()));
        value.insert(
            "marketplace".into(),
            Value::String(OFFICIAL_MARKETPLACE_ID.to_string()),
        );
        value.insert("pluginName".into(), Value::String(plugin_name));
        value.insert("sourceTrust".into(), Value::String("official".into()));
        if let Some(icon) = icon {
            value.insert("icon".into(), Value::String(icon));
        }
        if let Some(listing) = listing {
            value.insert("listing".into(), listing);
        }
        value.insert("diagnostics".into(), Value::Array(Vec::new()));
        Ok(Value::Object(value))
    }

    fn refresh_failure_value(
        &self,
        stable_id: &str,
        cancelled: &std::sync::atomic::AtomicBool,
        message: &str,
    ) -> Value {
        if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            unavailable_suggested(
                stable_id,
                ERROR_PLUGIN_OPERATION_CANCELLED,
                MESSAGE_PLUGIN_OPERATION_CANCELLED,
            )
        } else {
            unavailable_suggested(stable_id, ERROR_MARKETPLACE_REFRESH_FAILED, message)
        }
    }

    /// `toResult` for a catalog entry: conflict beats enabled/disabled, and the icon comes
    /// from the target host's cached official listing only.
    async fn suggested_reference_value(
        &self,
        ctx: &PluginContext,
        stable_id: &str,
        plugin_name: &str,
        marketplace: &str,
        entry: &reference::CatalogEntry,
    ) -> Value {
        let overview = overview_data(ctx).await;
        let display_by_id = reference::listing_display_by_plugin_id(&overview);
        let status = if !entry.conflicting_plugin_ids.is_empty() {
            "conflict"
        } else if entry.enabled {
            "ready"
        } else {
            "disabled"
        };
        let mut value = Map::new();
        value.insert("stableId".into(), Value::String(stable_id.to_string()));
        value.insert("status".into(), Value::String(status.to_string()));
        value.insert("marketplace".into(), Value::String(marketplace.to_string()));
        value.insert("pluginName".into(), Value::String(plugin_name.to_string()));
        value.insert("sourceTrust".into(), Value::String("official".into()));
        if let Some(icon) = display_by_id
            .get(stable_id)
            .and_then(|display| display.icon.clone())
        {
            value.insert("icon".into(), Value::String(icon));
        }
        let diagnostics = if entry.conflicting_plugin_ids.is_empty() {
            Vec::new()
        } else {
            vec![reference::suggested_reference_diagnostic(
                stable_id,
                ERROR_SUGGESTED_CONFLICT,
                MESSAGE_SUGGESTED_CONFLICT,
            )]
        };
        value.insert("diagnostics".into(), Value::Array(diagnostics));
        Value::Object(value)
    }

    /// One dispatch arm per member; the argument object is already decoded.
    async fn dispatch(
        &self,
        ctx: &ChannelContext,
        command: &str,
        params: &Map<String, Value>,
        cancel: &CancelToken,
    ) -> Result<Value, ChannelFailure> {
        match command {
            "listPlugins" => self.list_plugins(params).await.map_err(Into::into),
            "getPluginsOverview" => self.plugins_overview(params).await.map_err(Into::into),
            "addPluginMarketplace" => self
                .add_marketplace(params, cancel)
                .await
                .map_err(Into::into),
            "removePluginMarketplace" => self.remove_marketplace(params).await.map_err(Into::into),
            "updatePluginMarketplace" => self
                .update_marketplace(params, cancel)
                .await
                .map_err(Into::into),
            "installPlugin" => self.install(params, cancel).await.map_err(Into::into),
            "uninstallPlugin" => self.uninstall(params).await.map_err(Into::into),
            "updatePlugin" => self.update(params, cancel).await.map_err(Into::into),
            "restoreBuiltinPlugin" => self.restore_builtin(params).await.map_err(Into::into),
            "configurePlugin" => self.configure(params).await.map_err(Into::into),
            "resetPluginConfig" => self.reset_config(params).await.map_err(Into::into),
            "validatePlugin" => self.validate(params, cancel).await.map_err(Into::into),
            "describePlugin" => self.describe(params).await.map_err(Into::into),
            "setPluginEnabled" => self.set_enabled(params, cancel).await.map_err(Into::into),
            "cancelPluginOperation" => self.cancel_operation(params).await.map_err(Into::into),
            "getPluginReferenceCatalog" => self
                .plugin_reference_catalog(params)
                .await
                .map_err(Into::into),
            "getSkillReferenceCatalog" => self
                .skill_reference_catalog(ctx, params, cancel)
                .await
                .map_err(Into::into),
            "resolveSuggestedPluginReference" => self
                .resolve_suggested_reference(params, cancel)
                .await
                .map_err(Into::into),
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

#[async_trait]
impl Channel for PluginManagementService {
    async fn call(
        &self,
        ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        let params = params_object(&args)?;
        let value = self.dispatch(ctx, command, &params, &cancel).await?;
        Ok(WireValue::Json(value))
    }

    /// `onDynamicPluginOperationProgress(operationId)` (`pluginManagement.ts:50`).
    async fn listen(
        &self,
        _ctx: &ChannelContext,
        event: &str,
        arg: WireValue,
    ) -> Result<EventSubscription, ChannelFailure> {
        match event {
            // `onDynamicPluginOperationProgress(operationId)` (`pluginManagement.ts:50`);
            // the dynamic-event argument is the operation id itself.
            "onDynamicPluginOperationProgress" => {
                let Some(operation_id) = wire_string(&arg) else {
                    return Err(ChannelFailure::from(ServiceError::invalid_params(
                        "operationId is required",
                    )));
                };
                Ok(self.operations.progress_subscription(&operation_id))
            }
            other => Err(ChannelFailure::event_not_found(other)),
        }
    }
}

fn install_result_value(outcome: &InstallProtocolOutcome) -> Value {
    json!({
        "dependencyClosure": outcome.closure,
        "installedPlugins": outcome.installed,
        "diagnostics": to_protocol_values(&outcome.diagnostics),
    })
}

/// `buildPluginReferenceCatalog(outcome.plugins)` entry for `stable_id` over one
/// workspace discovery pass (`readState()` in the CLI handler).
async fn read_reference_entry(
    ctx: &PluginContext,
    stable_id: &str,
) -> Option<reference::CatalogEntry> {
    let outcome = ctx.discover().await;
    reference::build_catalog(&outcome.plugins)
        .into_iter()
        .find(|entry| entry.plugin_id == stable_id)
}

fn unavailable_suggested(stable_id: &str, code: &str, message: &str) -> Value {
    json!({
        "stableId": stable_id,
        "status": "unavailable",
        "diagnostics": [reference::suggested_reference_diagnostic(stable_id, code, message)],
    })
}

fn params_object(args: &ServiceArgs) -> Result<Map<String, Value>, ServiceError> {
    let value = args.required_json(0, "params")?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(ServiceError::invalid_params("params must be an object")),
    }
}

fn workspace_path(params: &Map<String, Value>) -> Result<String, ServiceError> {
    // Two callers reach this channel:
    // 1. `ProxyChannel.toService` (the product channel client) sends the flat params object.
    // 2. The legacy CLI protocol wraps the workspace ref under `params.workspace`.
    // Both must resolve to the same workspace or the settings page fails with a red
    // "workspacePath must be a non-empty string" banner (observed in the Tauri app).
    let nested = params
        .get("workspace")
        .and_then(Value::as_object)
        .and_then(|workspace| workspace.get("workspacePath"))
        .and_then(Value::as_str);
    let flat = params.get("workspacePath").and_then(Value::as_str);
    nested
        .or(flat)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            ServiceError::invalid_params("params.workspacePath must be a non-empty string")
        })
}

fn config_scope(params: &Map<String, Value>) -> Result<Option<String>, ServiceError> {
    match params.get("configScope").or_else(|| params.get("scope")) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(scope)) if scope == "user" || scope == "workspace" => {
            Ok(Some(scope.clone()))
        }
        Some(_) => Err(ServiceError::invalid_params(
            "params.scope must be \"user\" or \"workspace\"",
        )),
    }
}

fn required_string(params: &Map<String, Value>, key: &str) -> Result<String, ServiceError> {
    match params.get(key) {
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        _ => Err(ServiceError::invalid_params(format!(
            "params.{key} must be a non-empty string"
        ))),
    }
}

fn optional_string(params: &Map<String, Value>, key: &str) -> Result<Option<String>, ServiceError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ServiceError::invalid_params(format!(
            "params.{key} must be a string"
        ))),
    }
}

fn required_bool(params: &Map<String, Value>, key: &str) -> Result<bool, ServiceError> {
    match params.get(key) {
        Some(Value::Bool(value)) => Ok(*value),
        _ => Err(ServiceError::invalid_params(format!(
            "params.{key} must be a boolean"
        ))),
    }
}

fn optional_bool(params: &Map<String, Value>, key: &str) -> Result<Option<bool>, ServiceError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(ServiceError::invalid_params(format!(
            "params.{key} must be a boolean"
        ))),
    }
}

fn required_object(
    params: &Map<String, Value>,
    key: &str,
) -> Result<Map<String, Value>, ServiceError> {
    match params.get(key) {
        Some(Value::Object(value)) => Ok(value.clone()),
        _ => Err(ServiceError::invalid_params(format!(
            "params.{key} must be an object"
        ))),
    }
}

fn optional_string_array(
    params: &Map<String, Value>,
    key: &str,
) -> Result<Option<Vec<String>>, ServiceError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_string).ok_or_else(|| {
                    ServiceError::invalid_params(format!(
                        "params.{key} must be an array of strings"
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(ServiceError::invalid_params(format!(
            "params.{key} must be an array of strings"
        ))),
    }
}

fn wire_string(value: &WireValue) -> Option<String> {
    match value {
        WireValue::String(text) => Some(text.clone()),
        WireValue::Json(Value::String(text)) => Some(text.clone()),
        _ => None,
    }
}

fn service_error_from_failure(failure: ChannelFailure) -> ServiceError {
    match failure {
        ChannelFailure::Error(payload) => ServiceError {
            name: payload.name,
            message: payload.message,
            code: payload.code.map(Box::new),
            data: payload.data.map(Box::new),
        },
        ChannelFailure::Thrown(value) => {
            ServiceError::new(format!("skills channel rejected the call: {value:?}"))
        }
    }
}
