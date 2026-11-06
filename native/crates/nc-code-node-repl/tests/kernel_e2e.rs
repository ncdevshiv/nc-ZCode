//! kernel e2e: Start the kernel for the real Node and implement the complete NDJSON protocol.
//!
//! These tests are not mock probes - they spawn kernel_driver.mjs in the native Node,
//! Verify frame by frame evaluate/cancel/timeout/reset/status/module directory injection and process recycling,
//! is the only behavioral-level evidence of the Rust ↔ User Node link (the protocol single test only proves analysis).
//! These tests faithfully fail on machines where Node is missing/old (fail-closed is consistent with R2 acceptance),
//! No skipping, no forgery.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nc_code_node_repl::{
    capability, KernelOutput, KernelStatus, NodeKernel, NodeReplError, SpawnOptions,
    TOOL_MAX_TIMEOUT_MS,
};
use serde_json::{json, Value};

/// Detect the local Node; if it is not available, it will fail (do not skip - this machine should have Node for warehouse access control).
fn require_node() -> std::path::PathBuf {
    match capability::detect(None) {
        capability::NodeCapability::Available { exe, version, .. } => {
            println!("e2e kernel on {version} ({})", exe.display());
            exe
        }
        capability::NodeCapability::Unavailable(unavailable) => {
            panic!(
                "e2e requires a usable Node: {}",
                unavailable.to_model_text()
            );
        }
    }
}

fn spawn_kernel() -> NodeKernel {
    NodeKernel::spawn(&SpawnOptions::new(require_node())).expect("spawn kernel")
}

fn eval(kernel: &NodeKernel, code: &str) -> KernelOutput {
    kernel
        .evaluate(code, 5_000, Duration::from_secs(15), Value::Null, None)
        .expect("evaluate")
}

#[test]
fn evaluate_returns_the_completion_value_and_logs() {
    let kernel = spawn_kernel();
    // The completion value (end expression) and console log go to the two fields result/logs respectively.
    // (kernel_driver.mjs:711-720 buildOutput).
    // The final expression is converted to return and processed as the "last line" (kernel_driver.mjs:694-706), so
    // The completion value expression is on its own line (natural way of writing REPL).
    let output = eval(&kernel, "console.log('side effect')\n1 + 1");
    assert_eq!(
        output.result.as_deref(),
        Some("2"),
        "Numeric completion value without quotes"
    );
    assert_eq!(output.logs, "side effect");
    assert!(output.error.is_none());
    assert!(kernel.is_alive());
}

#[test]
fn top_level_declarations_persist_across_calls_in_one_kernel() {
    let kernel = spawn_kernel();
    // instrument.ts:98-206 Semantics: top-level declarations are copied to globalThis, spanning within the same kernel
    // evaluate is visible (the non-persistence of cross-calls in 0.6.0 fresh mode is guaranteed by tool layer spawn/shutdown).
    let first = eval(&kernel, "const base = 40\n'declared'");
    assert_eq!(
        first.result.as_deref(),
        Some("declared"),
        "The string result is returned unchanged"
    );
    let second = eval(&kernel, "base + 2");
    assert_eq!(
        second.result.as_deref(),
        Some("42"),
        "Binding must survive across calls"
    );
}

#[test]
fn js_error_is_a_result_not_a_transport_failure() {
    let kernel = spawn_kernel();
    // TS semantics: JS throw is the completed execution, error is entered into the result (node-repl-session.ts:438-446).
    let output = eval(&kernel, "throw new Error('boom')");
    let error = output.error.expect("error must be present");
    assert_eq!(error.name, "Error");
    assert_eq!(error.message, "boom");
    assert!(
        error.stack.unwrap_or_default().contains("boom"),
        "stack is going to be brought back"
    );
}

#[test]
fn sync_infinite_loop_is_broken_by_vm_budget_and_resets_bindings() {
    let kernel = spawn_kernel();
    // First drop a binding, verify the timeout and then clear the reset (node-repl-session.ts:428-437).
    eval(&kernel, "globalThis.keep = 'me'");
    // Synchronization infinite loop: vm budget (syncTimeoutMs=300) interrupts, driver reset + return RESET_SUFFIX.
    let output = kernel
        .evaluate(
            "while (true) {}",
            300,
            Duration::from_secs(10),
            Value::Null,
            None,
        )
        .expect("timeout arrives as an output error, not Err");
    let error = output.error.expect("timeout error");
    // The vm synchronization timeout is an Error thrown by the vm itself (name=Error, message contains "Script execution
    // timed out"); driver's mustReset hits message and appends RESET_SUFFIX// (kernel_driver.mjs:741-744), the same criteria as TS node-repl-session.ts:424-427.
    assert!(
        error.message.contains("Script execution timed out"),
        "vm budget message: {}",
        error.message
    );
    // RESET_SUFFIX verbatim (kernel_driver.mjs:28-29): The model must read reset semantics.
    assert!(
        error
            .message
            .contains("kernel reset, all previous bindings were cleared"),
        "message must carry the reset notice: {}",
        error.message
    );
    //The binding is cleared after reset.
    let after = eval(&kernel, "typeof keep");
    assert_eq!(
        after.result.as_deref(),
        Some("undefined"),
        "reset must clear the binding"
    );
    assert!(
        kernel.is_alive(),
        "kernel survives (not killed) after reset"
    );
}

#[test]
fn await_hang_is_aborted_by_overall_timeout_with_reset_notice() {
    let kernel = spawn_kernel();
    // await hangs forever: vm synchronization budget cannot be managed (only interrupts synchronization execution), rely on the outer total time to cancel
    // (Rust equivalent of server.ts:216-228 AbortSignal.timeout).
    let output = kernel
        .evaluate(
            "await new Promise(() => {})",
            5_000,
            Duration::from_millis(500),
            Value::Null,
            None,
        )
        .expect("await hang settles as output error");
    // The total duration timeout is a TimeoutError reason (kernel.rs evaluate), the driver uses this name
    // reject raceAbort (kernel_driver.mjs:811-813), keep the error name as it is.
    let error = output.error.expect("timeout abort error");
    assert_eq!(error.name, "TimeoutError");
    assert!(error.message.contains("JavaScript execution timed out"));
    assert!(error.message.contains("kernel reset"));
}

#[test]
fn cancel_flag_aborts_a_running_evaluation() {
    let kernel = spawn_kernel();
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    // The other thread will set the bit and cancel it after 200ms (the actual usage form of the blocking API: set when the host turn ends).
    let flag = std::sync::Arc::clone(&cancel);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        flag.store(true, Ordering::SeqCst);
    });
    let output = kernel
        .evaluate(
            "await new Promise(() => {})",
            5_000,
            Duration::from_secs(30),
            Value::Null,
            Some(&cancel),
        )
        .expect("cancel settles as output error");
    let error = output.error.expect("cancel error");
    assert_eq!(error.name, "AbortError");
    assert!(error.message.contains("JavaScript execution was canceled"));
}

#[test]
fn status_reset_and_generation_track_the_driver() {
    let kernel = spawn_kernel();
    let status: KernelStatus = kernel.status().expect("status");
    assert!(status.pid > 0);
    assert!(status.node_version.starts_with('v'));
    assert!(!status.busy, "busy=false when idle");
    assert!(status.restrict_process, "default restrict-process");
    // reset → generation+1 (kernel_driver.mjs:789-793).
    let generation = kernel.reset().expect("reset");
    assert!(generation >= 1);
    let after = kernel.status().expect("status after reset");
    assert_eq!(after.generation, generation);
}

#[test]
fn request_meta_reaches_the_sandbox_api() {
    let kernel = spawn_kernel();
    let output = kernel
        .evaluate(
            "nodeRepl.write(nodeRepl.requestMeta.session_id ?? 'none')",
            5_000,
            Duration::from_secs(10),
            json!({"session_id": "s-42"}),
            None,
        )
        .expect("evaluate");
    assert_eq!(
        output.logs, "s-42",
        "requestMeta must arrive in the sandbox verbatim"
    );
}

#[test]
fn restricted_process_has_no_exit_method() {
    let kernel = spawn_kernel();
    // The process under restrict-process is frozen facade (kernel_driver.mjs:54-73):// exit does not exist → JS TypeError (it is the error in the result, not the kernel death).
    let output = eval(&kernel, "process.exit(0)");
    let error = output.error.expect("facade must reject exit");
    assert_eq!(error.name, "TypeError");
    assert!(kernel.is_alive(), "Restricted facade keeps kernel alive");
}

#[test]
fn emit_image_reaches_the_result() {
    let kernel = spawn_kernel();
    let output = eval(
        &kernel,
        "nodeRepl.emitImage({base64: 'AA==', mimeType: 'image/png'}); 'ok'",
    );
    let images = output.images.expect("images must be present");
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].base64, "AA==");
    assert_eq!(images[0].mime_type, "image/png");
}

#[test]
fn add_module_dir_extends_resolution_and_reports_back() {
    let kernel = spawn_kernel();
    let extra = std::env::temp_dir().join(format!("zcode-nr-mods-{}", std::process::id()));
    std::fs::create_dir_all(&extra).expect("mkdir");
    let dirs = kernel.add_module_dir(&extra).expect("addModuleDir");
    assert!(
        dirs.iter().any(|dir| dir.contains("zcode-nr-mods")),
        "The injection directory must appear in the driver's parsed path: {dirs:?}"
    );
    let _ = std::fs::remove_dir_all(&extra);
}

#[test]
fn shutdown_reaps_the_process_and_the_next_call_fails_cleanly() {
    let kernel = spawn_kernel();
    let pid = kernel.pid();
    assert!(pid > 0);
    kernel.shutdown();
    assert!(!kernel.is_alive());
    let err = kernel
        .evaluate("1", 1_000, Duration::from_secs(1), Value::Null, None)
        .expect_err("dead kernel must fail closed");
    assert!(matches!(err, NodeReplError::KernelDied(_)));
    // The process is indeed gone (nc-code-proc's pid_alive exit code probe: dead/non-existent are considered recycled).
    let still_listed = nc_code_proc::list_tree(pid);
    assert!(
        !still_listed.iter().any(|info| info.pid == pid),
        "The kernel process must not be in the process table after shutdown"
    );
}
#[test]
fn large_timeout_budget_is_accepted_end_to_end() {
    //Contract anchor point: The maximum of the input parameter schema and the upper limit accepted by the execution layer must be the same number.
    // Otherwise, the model passing values according to the schema will be rejected by the execution layer (drift is a bug).
    assert_eq!(TOOL_MAX_TIMEOUT_MS, 120_000);
    let kernel = spawn_kernel();
    let output = kernel
        .evaluate(
            "'quick'",
            TOOL_MAX_TIMEOUT_MS,
            Duration::from_secs(10),
            Value::Null,
            None,
        )
        .expect("large budget is accepted");
    assert_eq!(output.result.as_deref(), Some("quick"));
}
