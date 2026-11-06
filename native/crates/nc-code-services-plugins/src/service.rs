//! `plugins` channel - Rust equivalent implementation of TS `packages/services/src/plugins/pluginsService.ts`.
//!
//! TS This channel is **retired** in ZCode Agent mode (pluginsService.ts:8-86): `getOverview` always returns
//! Empty list + `capability.supported=false`, the remaining 6 market/installation/start/stop members will be thrown away
//! "Legacy plugin management has been retired in ZCode Agent mode". The source of truth has been moved to the nc-code-cli process;
//! The active protocol side for plugin management is the `plugin-management` channel (thin agent to plugin method of nc-code-agent).
//! This file is an item-by-item image of the retirement semantics, not a placeholder - the TS specification itself is "return null + throw error".

use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use serde_json::{json, Value};

/// Error text for TS throwRetiredPluginManagement (pluginsService.ts:17-19).
const RETIRED_MESSAGE: &str = "Legacy plugin management has been retired in ZCode Agent mode";

pub struct PluginsService;

impl PluginsService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }

    /// TS createRetiredOverview(pluginsService.ts:8-15): empty list + not supported.
    fn retired_overview() -> Value {
        json!({
            "marketplaces": [],
            "availablePlugins": [],
            "installedPlugins": [],
            "capability": { "supported": false },
        })
    }
}

impl Default for PluginsService {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl Channel for PluginsService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        _args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        match command {
            "getOverview" => Ok(WireValue::Json(Self::retired_overview())),
            //The remaining 6 market/installation/start/stop members: retirement semantics, all throw the same error (consistent with TS throw).
            "addMarketplace" | "removeMarketplace" | "updateMarketplace" | "installPlugin"
            | "uninstallPlugin" | "setPluginEnabled" => Err(ChannelFailure::error(RETIRED_MESSAGE)),
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;

    fn ctx() -> ChannelContext {
        ChannelContext::new("plugins-test", ClientMode::DesktopContinuous)
    }

    #[tokio::test]
    async fn get_overview_returns_retired_empty_capability() {
        let service = PluginsService::new();
        let result = service
            .call(&ctx(), "getOverview", vec![], CancelToken::new())
            .await
            .expect("getOverview");
        let WireValue::Json(value) = result else {
            panic!("getOverview must return JSON");
        };
        assert_eq!(value["marketplaces"], json!([]));
        assert_eq!(value["availablePlugins"], json!([]));
        assert_eq!(value["installedPlugins"], json!([]));
        assert_eq!(value["capability"]["supported"], json!(false));
    }

    #[tokio::test]
    async fn mutations_throw_retired_and_unknown_is_method_not_found() {
        let service = PluginsService::new();
        for command in [
            "addMarketplace",
            "removeMarketplace",
            "updateMarketplace",
            "installPlugin",
            "uninstallPlugin",
            "setPluginEnabled",
        ] {
            let error = service
                .call(&ctx(), command, vec![], CancelToken::new())
                .await
                .expect_err("retired mutation");
            assert!(error.to_string().contains(RETIRED_MESSAGE), "{command}");
        }
        let unknown = service
            .call(&ctx(), "bogus", vec![], CancelToken::new())
            .await
            .expect_err("unknown method");
        assert!(unknown.to_string().contains("Method not found"));
    }
}
