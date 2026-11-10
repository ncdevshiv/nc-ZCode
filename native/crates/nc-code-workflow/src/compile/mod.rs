//! Compile layer: workflow script compilation.
//!
//! The TS compiler API pipeline has been replaced with a native Rust implementation
//! (`ts_workflow`) that produces identical `CompiledScript` output (site table, schema
//! synthesis, lowering, diagnostics). The Node kernel bridge is no longer the default
//! execution path; `CompileKernel` uses the native compiler directly.
//! Transport, timeouts, crash isolation, caching and scriptHash remain in `kernel`.

pub mod analysis;
mod kernel;
mod native_impl;
pub mod ts_workflow;

pub use kernel::{
    default_dist_entry, default_node, script_hash, AskSpecWire, CompileDiagnostic, CompileError,
    CompileKernel, CompiledScript, KERNEL_SOURCE, PROTOCOL,
};
pub use ts_workflow::compile_ts_workflow;
