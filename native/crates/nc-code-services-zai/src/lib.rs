//! Z.ai service disabled (ZCode services removed).

/// Handoff skeleton (pure data/types): serde persistence structs, port trait, Send/Sync markers.
pub mod handoff;

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
#[derive(Default)]
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
    use nc_code_channels::ChannelRegistry;

    /// Every ZCode cloud service is removed, so the family registers no channel at
    /// all; registration succeeding with an untouched registry is the contract.
    #[test]
    fn the_removed_family_registers_no_channel() {
        let registry = ChannelRegistry::new();
        let before = registry.len();
        super::ZaiServices::register(&super::ZaiServices, &registry).expect("register");
        assert_eq!(registry.len(), before, "no channel may be registered");
        assert_eq!(super::ZAI_PROVIDER_ID, "zai");
    }
}
