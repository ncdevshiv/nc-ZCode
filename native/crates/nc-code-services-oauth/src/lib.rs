//! OAuth channel — matches the disabled TS implementation
//! (`packages/services/src/oauth/oauthService.ts`).
//!
//! OAuth is currently disabled in the TS host service: provider/session reads return
//! signed-out/empty values, start/refresh fail with `OAuth disabled`, and
//! logout/cancel are no-ops. This channel mirrors that contract; it does not perform
//! network flows or fabricate tokens.

use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::ServiceArgs;
use serde_json::json;

const OAUTH_DISABLED: &str = "OAuth disabled";

pub struct OAuthService;

impl OAuthService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl Default for OAuthService {
    fn default() -> Self {
        Self
    }
}

fn disabled() -> ChannelFailure {
    ChannelFailure::error(OAUTH_DISABLED)
}

#[async_trait]
impl Channel for OAuthService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let _args = ServiceArgs::new(args);
        match command {
            "getProviders" => Ok(WireValue::Json(json!([]))),
            "getActiveProvider"
            | "restoreCachedSession"
            | "restoreSession"
            | "pollPendingOAuth"
            | "handleCallback" => Ok(WireValue::Json(serde_json::Value::Null)),
            "restoreCachedSessionState" => Ok(WireValue::Json(json!({ "status": "signed-out" }))),
            "startOAuth" | "startOAuthWithPolling" | "refreshToken" => Err(disabled()),
            "logout" | "logoutAll" | "cancelPending" | "revoke" => Ok(WireValue::Undefined),
            // Back-compat names from earlier stub; keep explicit behavior.
            "getStatus" => Ok(WireValue::Json(
                json!({ "authenticated": false, "disabled": true }),
            )),
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;

    fn ctx() -> ChannelContext {
        ChannelContext::new("oauth", ClientMode::DesktopContinuous)
    }

    async fn call(command: &str) -> Result<WireValue, ChannelFailure> {
        OAuthService::new()
            .call(&ctx(), command, vec![], CancelToken::new())
            .await
    }

    fn json(value: WireValue) -> serde_json::Value {
        match value {
            WireValue::Json(value) => value,
            other => panic!("expected json, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reads_match_disabled_ts_contract() {
        assert_eq!(
            json(call("getProviders").await.expect("providers")),
            json!([])
        );
        assert!(matches!(
            call("getActiveProvider").await.expect("active"),
            WireValue::Json(serde_json::Value::Null)
        ));
        assert_eq!(
            json(call("restoreCachedSessionState").await.expect("state"))["status"],
            json!("signed-out")
        );
        assert!(matches!(
            call("logout").await.expect("logout"),
            WireValue::Undefined
        ));
    }

    #[tokio::test]
    async fn start_and_refresh_fail_closed() {
        for command in ["startOAuth", "startOAuthWithPolling", "refreshToken"] {
            let err = call(command).await.expect_err(command);
            assert!(err.to_string().contains(OAUTH_DISABLED), "{command}: {err}");
        }
    }
}
