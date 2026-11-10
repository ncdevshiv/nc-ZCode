//! `nc-code-orchestrator` - NC-AG, the autonomous multi-agent orchestration platform.
//!
//!
//! Spec: `specs/nc-ag-orchestration.md`. Backend-only Rust (TypeScript stays front-end).
//! The crate owns orchestration state; the axum server mounts `api::router` and the
//! desktop/web shell renders `packages/ui/src/nc-ag/*` against that API.

#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::too_many_arguments)]

pub mod agent;
pub mod api;
pub mod budget;
pub mod config;
pub mod dag;
pub mod db;
pub mod decisions;
pub mod error;
pub mod events;
pub mod git;
pub mod integration;
pub mod leases;
pub mod main_support;
pub mod messaging;
pub mod orchestrator;
pub mod planner;
pub mod profiles;
pub mod provider;
pub mod recovery;
pub mod repo;
pub mod repo_misc;
pub mod repo_tasks;
pub mod review;
pub mod security;
pub mod state;
pub mod supervisor;
pub mod tools;
pub mod types;
pub mod verification;

pub use config::NcagConfig;
pub use db::Store;
pub use error::{NcagError, Result};
pub use events::EventHub;
pub use orchestrator::Orchestrator;
pub use supervisor::Supervisor;

/// Everything a host needs to run NC-AG in-process (server, desktop shell, standalone bin).
pub struct NcagRuntime {
    pub store: std::sync::Arc<Store>,
    pub hub: std::sync::Arc<EventHub>,
    pub orchestrator: std::sync::Arc<Orchestrator>,
    pub supervisor: std::sync::Arc<Supervisor>,
    pub config: NcagConfig,
}

/// The shutdown flag shared between a host and the maintenance loop.
pub fn new_shutdown_flag() -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))
}

impl NcagRuntime {
    /// Open the store, compose the orchestrator/supervisor and run startup recovery.
    /// The provider store is optional: without it agents fail closed with
    /// `provider_unavailable` (never a fake model).
    pub fn open(
        config: NcagConfig,
        provider_store: Option<std::sync::Arc<nc_code_services_provider::store::ProviderStore>>,
    ) -> Result<Self> {
        let store = std::sync::Arc::new(Store::open(&config.database_path)?);
        let hub = std::sync::Arc::new(EventHub::new());
        let orchestrator = std::sync::Arc::new(Orchestrator::new(
            std::sync::Arc::clone(&store),
            std::sync::Arc::clone(&hub),
            config.clone(),
            provider_store,
        ));
        let supervisor = std::sync::Arc::new(Supervisor::new(
            std::sync::Arc::clone(&orchestrator),
            std::sync::Arc::clone(&store),
            std::sync::Arc::clone(&hub),
        ));
        // Recovery is best-effort at startup: a corrupt row must not stop the service.
        let _ = supervisor.startup();
        Ok(Self {
            store,
            hub,
            orchestrator,
            supervisor,
            config,
        })
    }

    pub fn api_state(&self) -> api::ApiState {
        api::ApiState {
            orchestrator: std::sync::Arc::clone(&self.orchestrator),
            store: std::sync::Arc::clone(&self.store),
            hub: std::sync::Arc::clone(&self.hub),
            token: self.config.api_token.clone(),
        }
    }

    /// Spawn the maintenance loop on the caller's Tokio runtime; returns the shutdown flag.
    ///
    /// Callers that already run inside a Tokio runtime (the server binary) can use this
    /// directly. Callers on a non-Tokio thread (the Tauri setup hook) must use
    /// `NcagRuntime::run_background` through their own runtime handle instead - a bare
    /// `tokio::spawn` there panics with "there is no reactor running", which used to abort
    /// the whole desktop app at startup.
    pub fn spawn_background(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        let shutdown = new_shutdown_flag();
        tokio::spawn(std::sync::Arc::clone(&self.supervisor).run(
            supervisor::SWEEP_INTERVAL_MS,
            std::sync::Arc::clone(&shutdown),
        ));
        shutdown
    }

    /// Body of the maintenance loop, awaited by whichever runtime owns the thread.
    /// Returns when the shutdown flag is set (graceful stop; state is already durable).
    pub async fn run_background(
        self: std::sync::Arc<Self>,
        shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        std::sync::Arc::clone(&self.supervisor)
            .run(supervisor::SWEEP_INTERVAL_MS, shutdown)
            .await;
    }
}
