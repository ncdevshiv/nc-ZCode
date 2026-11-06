use crate::compile::kernel::{CompileError, CompiledScript};
use crate::compile::ts_workflow::compile_ts_workflow;

/// Native Rust compiler entry point (replaces Node kernel bridge to TS compiler).
/// Produces identical `CompiledScript` output: parsing, diagnostics, site analysis,
/// schema synthesis, lowering, and actor submit profiles.
pub fn compile_with_native_rust(script_text: &str) -> Result<CompiledScript, CompileError> {
    compile_ts_workflow(script_text)
}
