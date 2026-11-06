//! Client-facing service channels (Rust backend).
//!
//! Two TS channels keep their names and shapes but answer from local state rather than a
//! zcode cloud fetch (product policy: no zcode cloud connection except auth/token claim
//! and account/model setup — `specs/disabled-features.md`):
//! - `client-config` — public client config snapshot (`pluginStoreOrder` is always null).
//! - `client-scenes` — client scene catalog (always an empty catalog envelope).
//! - `window-controller` — window Controller projection over `nc_code_services_agent::TaskService`.
//!
//! [`ClientServices`] is shaped like `ConfigServices`: construct once, `register` exactly
//! the three channel names. A channel is registered only when its implementation is real —
//! there are no placeholder handlers in this crate.

pub mod client_config;
pub mod client_scenes;
pub mod controller_projection;
pub mod remote_registry;
pub mod remote_ws_facade;
pub mod window_controller;

use std::sync::Arc;

use nc_code_channels::{ChannelRegistry, DuplicateChannel};
use nc_code_services_agent::TaskService;

pub use client_config::ClientConfigService;
pub use client_scenes::ClientScenesService;
pub use remote_registry::RemoteConnectionRegistry;
pub use remote_ws_facade::{probe_tunnel_health, TcpForwarder, WsChannelClient, WsTaskService};
pub use window_controller::WindowControllerService;

pub struct ClientServices {
    pub client_config: Arc<ClientConfigService>,
    pub client_scenes: Arc<ClientScenesService>,
    pub window_controller: Arc<WindowControllerService>,
    pub remotes: Arc<RemoteConnectionRegistry>,
}

impl ClientServices {
    /// The window Controller does not own task state; it routes into `tasks`
    /// (local) or the window registry's remote facades (spec §2).
    pub fn new(tasks: Arc<TaskService>, remotes: Arc<RemoteConnectionRegistry>) -> Self {
        Self {
            client_config: ClientConfigService::new(),
            client_scenes: ClientScenesService::new(),
            window_controller: WindowControllerService::new(
                Arc::clone(&tasks),
                Arc::clone(&remotes),
            ),
            remotes,
        }
    }

    pub fn register(&self, registry: &ChannelRegistry) -> Result<(), DuplicateChannel> {
        registry.register("client-config", self.client_config.clone())?;
        registry.register("client-scenes", self.client_scenes.clone())?;
        registry.register("window-controller", self.window_controller.clone())?;
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
