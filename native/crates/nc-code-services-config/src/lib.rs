//! Configuration/account service channels (Rust backend).
//!
//! Each module owns one channel and can be registered independently; [`ConfigServices`]
//! is the composition used by `nc-code-app`. No module contains another channel's logic.

pub mod broadcast;
pub mod credential;
pub mod cua_permission;
pub mod cua_pip_session;
pub mod onboarding;
pub mod setting;

use std::sync::Arc;

use nc_code_channels::{ChannelRegistry, DuplicateChannel};
use nc_code_service_core::AppPaths;

pub struct ConfigServices {
    pub paths: AppPaths,
    pub account: onboarding::AccountState,
    pub setting: Arc<setting::SettingService>,
    pub broadcast: Arc<broadcast::BroadcastService>,
    pub onboarding: Arc<onboarding::OnboardingRecordService>,
    pub credential: Arc<credential::CredentialService>,
    pub cua_permission: Arc<cua_permission::CuaPermissionService>,
    pub cua_pip_session: Arc<cua_pip_session::CuaPipSessionService>,
}

impl ConfigServices {
    pub fn new(paths: AppPaths) -> Self {
        let account = onboarding::AccountState::new();
        Self {
            setting: Arc::new(setting::SettingService::new(paths.clone())),
            broadcast: Arc::new(broadcast::BroadcastService::new()),
            onboarding: Arc::new(onboarding::OnboardingRecordService::new(
                paths.clone(),
                account.clone(),
            )),
            credential: Arc::new(credential::CredentialService::new(paths.clone())),
            cua_permission: cua_permission::CuaPermissionService::new(),
            cua_pip_session: cua_pip_session::CuaPipSessionService::new(),
            account,
            paths,
        }
    }

    pub fn register(&self, registry: &ChannelRegistry) -> Result<(), DuplicateChannel> {
        registry.register("setting", self.setting.clone())?;
        registry.register("broadcast", self.broadcast.clone())?;
        registry.register("onboarding-record", self.onboarding.clone())?;
        registry.register("credential", self.credential.clone())?;
        registry.register("cua-permission", self.cua_permission.clone())?;
        registry.register("cua-pip-session", self.cua_pip_session.clone())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_service_core::AppPaths;
    use tempfile::TempDir;

    #[test]
    fn config_services_constructs() {
        let tmp = TempDir::new().unwrap();
        let paths = AppPaths::new(tmp.path().to_path_buf(), tmp.path().to_path_buf());
        let config = ConfigServices::new(paths);
        assert!(config
            .paths
            .app_config_dir()
            .to_string_lossy()
            .contains(".zcode"));
    }
}
