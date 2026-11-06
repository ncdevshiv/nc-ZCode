//! Shared kit for service channels (the Rust counterpart of `@zcode/services`
//! cross-cutting helpers). No channel implementations live here.

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

pub use args::ServiceArgs;
pub use error::ServiceError;
pub use paths::AppPaths;
