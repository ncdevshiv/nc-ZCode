//! Composition root for the Rust backend.
//!
//! Builds the channel registry from real service implementations. Each service crate
//! owns its channels; this crate owns the assembly order and the shared paths. A
//! channel is registered only when its implementation is complete — the registry is
//! never populated with placeholders.

/// Handoff skeleton (pure data/types): serde persistence structs, port trait, Send/Sync markers.
pub mod handoff;

use std::sync::Arc;

use nc_code_channels::{ChannelRegistry, DuplicateChannel};
use nc_code_service_core::logger::ServiceLogger;
use nc_code_service_core::AppPaths;
use nc_code_services_agent::automation_dispatch::AutomationDispatcher;
use nc_code_services_agent::{AgentService, AgentServices};
use nc_code_services_client::ClientServices;
use nc_code_services_config::ConfigServices;
use nc_code_services_conversation_share::ConversationShareService;
use nc_code_services_extensions::ExtensionsServices;
use nc_code_services_feedback::FeedbackService;
use nc_code_services_git::GitServices;
use nc_code_services_git_checkpoint::GitCheckpointServices;
use nc_code_services_mcp_sync::McpSyncService;
use nc_code_services_offpeak::DisabledOffPeakTaskService;
use nc_code_services_plugin_sync::PluginSyncService;
use nc_code_services_plugins::PluginManagementService;
use nc_code_services_prompt_attachment_transfer::PromptAttachmentTransferService;
use nc_code_services_provider::ProviderServices;
use nc_code_services_settings_sync::SettingsSyncService;
use nc_code_services_skill_sync::SkillSyncService;
use nc_code_services_terminal::TerminalServices;
use nc_code_services_usage_stats::UsageStatsService;
use nc_code_services_workspace::WorkspaceServices;

pub struct Backend {
    pub registry: Arc<ChannelRegistry>,
    pub paths: AppPaths,
    pub config: ConfigServices,
    pub agent: AgentServices,
    pub provider: ProviderServices,
    pub workspace: WorkspaceServices,
    pub terminal: TerminalServices,
    pub extensions: ExtensionsServices,
    pub client: ClientServices,
    pub git: GitServices,
    pub git_checkpoint: GitCheckpointServices,
}

/// The scheduling loop for automation: every 20s it claims due items and dispatches them (matching the polling cadence of the TS desktop scheduler).
/// Both entry points (nc-code-server / nc-code-shell) share it, each spawning it on its own runtime.
pub async fn run_automation_scheduler(agent: Arc<AgentService>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(20));
    loop {
        interval.tick().await;
        let dispatcher = AutomationDispatcher::new(&agent);
        match dispatcher.tick_once(now_ms()).await {
            Ok(report) if report.claimed > 0 || report.skipped > 0 || report.failed > 0 => {
                ServiceLogger::new("nc-code-automation").info(
                    None,
                    format!(
                        "claimed={} dispatched={} skipped={} failed={}",
                        report.claimed, report.dispatched, report.skipped, report.failed
                    ),
                );
            }
            Ok(_) => {}
            Err(error) => ServiceLogger::new("nc-code-automation")
                .error(None, format!("tick failed: {error}")),
        }
    }
}

pub fn spawn_automation_scheduler(agent: Arc<AgentService>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run_automation_scheduler(agent))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

pub fn build(paths: AppPaths) -> Result<Backend, DuplicateChannel> {
    build_with_ports(paths, None, None)
}

/// Composition-root entry that injects the native browser-control port (the
/// Tauri shell's child-webview CDP bridge). The plain `build` entry keeps the
/// tool fail-closed, matching the "absent = explicit unavailable" contract.
pub fn build_with_browser_port(
    paths: AppPaths,
    browser_port: Option<Arc<dyn nc_code_host::tools::browser::BrowserControlPort>>,
) -> Result<Backend, DuplicateChannel> {
    build_with_ports(paths, browser_port, None)
}

/// Composition-root entry that injects both native desktop ports: browser
/// control (child-webview CDP) and computer use (screen capture + input).
pub fn build_with_ports(
    paths: AppPaths,
    browser_port: Option<Arc<dyn nc_code_host::tools::browser::BrowserControlPort>>,
    computer_port: Option<Arc<dyn nc_code_host::tools::computer::ComputerControlPort>>,
) -> Result<Backend, DuplicateChannel> {
    let registry = Arc::new(ChannelRegistry::new());
    let config = ConfigServices::new(paths.clone());
    config.register(&registry)?;
    let provider = ProviderServices::new(paths.clone());
    provider.register(&registry)?;
    // provider-provisioning-target: the receiving end of cross Environment Provisioning (1-member apply).
    // It needs the provider's store/settings plus config's setting/credential (a cross-service transaction, see provisioning.rs).
    let provisioning =
        nc_code_services_provider::provisioning::ProviderProvisioningTargetService::new(
            paths.clone(),
            Arc::clone(&provider.store),
            Arc::clone(&config.setting),
            Arc::clone(&config.credential),
            Arc::clone(&provider.settings),
        );
    registry.register("provider-provisioning-target", provisioning)?;
    let agent = AgentServices::new_with_ports(
        paths.clone(),
        Arc::clone(&provider.store),
        browser_port,
        computer_port,
    );
    agent.register(&registry)?;
    let workspace = WorkspaceServices::new(paths.clone());
    workspace.register(&registry)?;
    let terminal = TerminalServices::new(paths.clone());
    terminal.register(&registry)?;
    // The plugins channel is retired in ZCode Agent mode (mirrored entry by entry from the TS pluginsService.ts: getOverview returns an
    // empty list + capability.supported=false, while the market/install/enable-disable members throw the same error).
    registry.register("plugins", nc_code_services_plugins::PluginsService::new())?;
    let extensions = ExtensionsServices::new(paths.clone());
    extensions.register(&registry)?;
    // The protocol slash-command directory is owned by the `commands` channel; the agent's
    // `readWorkspacePresentation` consumes it through this port (one discovery path).
    struct CommandsSlashDirectory(Arc<nc_code_services_extensions::commands::CommandsService>);
    #[async_trait::async_trait]
    impl nc_code_services_agent::service::SlashCommandDirectory for CommandsSlashDirectory {
        async fn protocol_slash_commands(
            &self,
            workspace_path: &str,
        ) -> Result<serde_json::Value, nc_code_service_core::ServiceError> {
            self.0.protocol_slash_commands(workspace_path).await
        }
    }
    agent
        .service
        .attach_slash_command_directory(Arc::new(CommandsSlashDirectory(Arc::clone(
            &extensions.commands,
        ))));
    // plugin-management: the plugin-management protocol surface (install/market/enable-disable/reference catalog). The skill-reference catalog
    // reuses the same process's skills channel (the single owner of skill discovery), so it must be constructed after extensions.
    let plugin_management = PluginManagementService::new(paths.clone(), extensions.skills.clone());
    // nc-code-agent's 18 plugin members are mirrored to the same instance (one implementation, two dispatch points).
    agent.service.attach_plugin_management(
        Arc::clone(&plugin_management) as Arc<dyn nc_code_channels::Channel>
    );
    registry.register("plugin-management", plugin_management)?;
    // client's window-controller is only a router: the source of task truth is the agent's tasks facade,
    // so it must be constructed after agent (the AGENTS.md single-owner rule).
    // Remote scopes resolve through the window registry (spec §2): one registry
    // per window host, fed by remote connects (SSH/WSL/Docker forwards).
    let remotes = nc_code_services_client::RemoteConnectionRegistry::new();
    let client = ClientServices::new(Arc::clone(&agent.tasks), Arc::clone(&remotes));
    client.register(&registry)?;
    // The commit-message generator and the agent share the same provider store (single source of truth).
    let git = GitServices::new(Arc::clone(&provider.store));
    git.register(&registry)?;
    // The checkpoint store/repo share the same AppPaths (manifest directory + temporary index root).
    let git_checkpoint = GitCheckpointServices::new(paths.clone());
    git_checkpoint.register(&registry)?;
    // system service: owned by WorkspaceServices (same 3 methods); registering
    // the standalone SystemService here double-registered "system" and broke
    // composition. Single owner per the AGENTS.md rule.
    registry.register("conversation-share", ConversationShareService::new())?;
    // off-peak-task: cloud scheduling is disabled per user policy (specs/disabled-features.md §2.1).
    // The channel registers in disabled form; the frontend hides the entry based on feature_disabled from getCodingPlanSupport.
    registry.register("off-peak-task", DisabledOffPeakTaskService::new())?;
    registry.register(
        "prompt-attachment-transfer",
        PromptAttachmentTransferService::new(),
    )?;
    registry.register("usage-stats", UsageStatsService::new())?;
    registry.register("mcp-sync", McpSyncService::new(paths.clone()))?;
    registry.register("plugin-sync", PluginSyncService::new(paths.clone()))?;
    registry.register("skill-sync", SkillSyncService::new(paths.clone()))?;
    registry.register("feedback", FeedbackService::new())?;
    registry.register("settings-sync", SettingsSyncService::new())?;
    // file/file-watcher/media-preview + system are owned by WorkspaceServices
    // (spec §2.1); the standalone crates for the same channels are NOT
    // re-registered here (\"channel already registered\" broke composition).
    Ok(Backend {
        registry,
        paths,
        config,
        agent,
        provider,
        workspace,
        terminal,
        extensions,
        client,
        git,
        git_checkpoint,
    })
}

#[cfg(test)]
mod tests {
    /// The composition root must assemble the live channel families: a missing
    /// registration here is a dead frontend surface, not a compile error.
    #[test]
    fn build_assembles_the_channel_families() {
        let root = std::env::temp_dir().join(format!("zcode-app-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("data dir");
        let backend = super::build(nc_code_service_core::AppPaths::new(
            root.clone(),
            root.clone(),
        ))
        .expect("build");
        for channel in [
            "zcode-agent",
            "zcode-session",
            "zcode-task",
            "file",
            "file-watcher",
            "media-preview",
            "terminal",
            "commands",
            "hooks",
            "memory",
            "skills",
            "plugins",
            "git",
            "provider-settings",
            "model-selection",
        ] {
            assert!(
                backend.registry.get(channel).is_some(),
                "channel {channel} must be registered"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
