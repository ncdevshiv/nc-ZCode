//! The `subagents` channel —— see the individual submodules (markdown/state/storage/service).

pub mod markdown;
pub mod service;
pub mod state;
pub mod storage;

pub use service::SubagentsService;
