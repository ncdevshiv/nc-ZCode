//! Z.ai service disabled (ZCode services removed).

use std::sync::Arc;

use nc_code_channels::{ChannelRegistry, DuplicateChannel};

#[derive(Clone, Debug)]
pub struct ZaiConfig;

impl Default for ZaiConfig {
    fn default() -> Self {
        Self
    }
}

impl ZaiConfig {
    pub fn from_env() -> Self {
        Self
    }
}

pub const ZAI_PROVIDER_ID: &str = "zai";
pub const ZCODE_JWT_TOKEN_KEY: &str = "zcode_jwt_token";

pub struct ZaiOAuthService;
pub struct ZaiClaimService;
pub struct ZaiHttp;

impl ZaiOAuthService {
    pub fn new<C>(_config: ZaiConfig, _credentials: Arc<C>, _http: ZaiHttp) -> Self {
        Self
    }
    pub async fn refresh_token(
        &self,
        _token: Option<String>,
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Err("Zai services disabled".into())
    }
}

impl ZaiClaimService {
    pub fn new<C>(_config: ZaiConfig, _credentials: Arc<C>, _http: ZaiHttp) -> Self {
        Self
    }
}

impl ZaiHttp {
    pub fn new() -> Self {
        Self
    }
}

pub struct ZaiServices;

impl ZaiServices {
    pub fn new<C>(_config: ZaiConfig, _credentials: C) -> Self {
        Self
    }

    pub fn register(&self, _registry: &ChannelRegistry) -> Result<(), DuplicateChannel> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {
        assert!(true);
    }
}
