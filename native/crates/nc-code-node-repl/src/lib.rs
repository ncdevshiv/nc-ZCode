//! nc-code-node-repl — the R2 `node_repl` island (specs/rust-pure-native-roadmap.md §7 R2).
//!
//! The decisions themselves are not redesigned by this crate:
//! - **No JS engine is embedded** (no quickjs/boa, no deno_core/V8).
//! - At startup it probes **the user's own Node**; when the probe fails it reports **unavailable**,
//!   in the same shape as `nc-code-cua`'s fail-closed (`packages/nc-code-cua/broker.js:7-22`:
//!   typed error `{name, code, message, details?}`).
//! - When Node is detected the semantics are real Node: `require` / `npm` / `fetch` / `fs` are the same as today,
//!   because the kernel is just a child process running inside the user's Node (`src/driver/kernel_driver.mjs`).
//!
//! Layered dependency direction: this crate depends only on `nc-code-proc` (process-tree reclamation) and has no
//! compile-time coupling with `nc-code-host` — the host just wraps one layer with its own `Tool` trait (see README "Host wiring").
//!
//! Threading model (RUST-PORT-LOG ENTRY 017): entry points such as `evaluate` perform **blocking** IO
//! (write the child's stdin, wait for the NDJSON reply frame), so they must be called from a blocking thread pool; on the
//! host side this corresponds to the `spawn_blocking` at `native/crates/nc-code-host/src/turn.rs:1021`. Inside the kernel
//! there are two background threads, reader/stderr, and asynchrony shows up at the process boundary rather than in async tasks,
//! so this crate does not introduce tokio.

pub mod capability;
mod driver;
mod error;
mod kernel;
mod output;
mod protocol;
pub mod session;
mod tool;
mod version;

pub use capability::{
    detect, probe, DiscoveryConfig, DiscoverySource, NodeCapability, MIN_SUPPORTED_NODE,
};
pub use driver::{materialize_driver, DRIVER_FILE_NAME, KERNEL_DRIVER_SOURCE};
pub use error::{NodeReplError, NodeReplUnavailable, UnavailableCode};
pub use kernel::{
    KernelHandle, KernelRegistry, KernelStatus, NodeKernel, SpawnOptions, KERNEL_CANCEL_GRACE_MS,
    NODE_REPL_UNSCOPED_KEY,
};
pub use output::{
    cap_model_output, CappedOutput, JsError, JsImage, JsOutput, OutputBudget,
    MAX_INLINE_OUTPUT_BYTES, MAX_MODEL_OUTPUT_BYTES, NODE_REPL_DEFAULT_TIMEOUT_MS,
    NODE_REPL_SERVER_VERSION, TOOL_MAX_TIMEOUT_MS, TOOL_SYNC_TIMEOUT_CAP_MS,
};
pub use protocol::{
    parse_inbound, InboundFrame, KernelImage, KernelJsError, KernelOutput, ReadyEvent, StatusFrame,
    KERNEL_PROTOCOL,
};
pub use tool::{
    KernelMode, NodeReplTool, ToolCall, ToolContractDescriptor, JS_TOOL_NAME, NODE_REPL_SESSIONS,
    SCOPE_SYSTEM,
};
pub use version::NodeVersion;

/// Model-facing failure prefix for the `js` tool (same `[code] message` shape as the host's
/// `ToolError::to_model_text`, native/crates/nc-code-host/src/tools/mod.rs:93-95).
pub fn model_error_text(code: &str, message: &str) -> String {
    format!("[{code}] {message}")
}
