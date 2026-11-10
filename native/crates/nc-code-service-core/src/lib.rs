//! Shared kit for service channels (the Rust counterpart of `@zcode/services`
//! cross-cutting helpers). No channel implementations live here.

/// Handoff skeleton (pure data/types): serde persistence structs, port trait, Send/Sync markers.
pub mod handoff;

pub mod args;
pub mod atomic;
pub mod bigmodel;
pub mod conversation_telemetry;
pub mod device_mid;
pub mod error;
pub mod fs_fault_injection;
pub mod logger;
pub mod memory_diagnostics;
pub mod paths;
pub mod remote_sync;
pub mod usage_aggregate;
pub mod usage_store;

pub use args::ServiceArgs;
pub use error::ServiceError;
pub use paths::AppPaths;
pub use usage_aggregate::{build_app_usage_snapshot, UsageFact, UsageToolCall};
pub use usage_store::{scan_agent_usage, UsageScan};
