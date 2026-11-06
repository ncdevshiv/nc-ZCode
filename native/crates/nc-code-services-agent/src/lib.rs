//! `nc-code-agent` service channel: in-process nc-code-host runtime per workspace.

pub mod automation_channel;
pub mod automation_dispatch;
pub mod automation_tool_port;
pub mod runtime;
pub mod saved_workflows;
pub mod service;
pub mod session;
pub mod snapshot;
pub mod task;
pub mod task_snapshot;
pub mod task_store;

use std::sync::Arc;

use nc_code_channels::{ChannelRegistry, DuplicateChannel};
use nc_code_service_core::AppPaths;
use nc_code_services_provider::ProviderStore;

pub use service::AgentService;
pub use session::SessionService;
pub use task::TaskService;

pub struct AgentServices {
    pub service: Arc<AgentService>,
    pub sessions: Arc<SessionService>,
    pub tasks: Arc<TaskService>,
}

impl AgentServices {
    pub fn new(paths: AppPaths, store: Arc<ProviderStore>) -> Self {
        Self::new_with_browser_port(paths, store, None)
    }

    pub fn new_with_browser_port(
        paths: AppPaths,
        store: Arc<ProviderStore>,
        browser_port: Option<Arc<dyn nc_code_host::tools::browser::BrowserControlPort>>,
    ) -> Self {
        Self::new_with_ports(paths, store, browser_port, None)
    }

    pub fn new_with_ports(
        paths: AppPaths,
        store: Arc<ProviderStore>,
        browser_port: Option<Arc<dyn nc_code_host::tools::browser::BrowserControlPort>>,
        computer_port: Option<Arc<dyn nc_code_host::tools::computer::ComputerControlPort>>,
    ) -> Self {
        let service =
            AgentService::new_with_ports(paths.clone(), store, browser_port, computer_port);
        let sessions = SessionService::new(Arc::clone(&service));
        let tasks = TaskService::new(paths, Arc::clone(&service));
        Self {
            service,
            sessions,
            tasks,
        }
    }

    pub fn register(&self, registry: &ChannelRegistry) -> Result<(), DuplicateChannel> {
        registry.register("zcode-agent", self.service.clone())?;
        registry.register("zcode-session", self.sessions.clone())?;
        registry.register("zcode-task", self.tasks.clone())?;
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
