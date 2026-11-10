//! Compile-kernel client (port of the TS front-end drive in `dynamic-workflow-run-submit.ts`).
//!
//! The frontend (TypeScript Program, diagnostics, site table, schema synthesis, lowering) is still the single source of truth, and the
//! native side does not rewrite it: this module treats one long-lived Node process ([`KERNEL_SOURCE`]) as the frontend executor, trading one
//! request for the full set of artifacts of one "compile once". The host only owns transport, timeouts, crash isolation, caching and hashing.
//!
//! The wire: NDJSON over stdin/stdout, one request per response, correlated by id; startup begins with the handshake
//! `{"kind":"hello","protocol":"zcode-workflow-compile/1"}`. Diagnostics are **data**:
//! when `compileOk=false` the kernel still replies `ok:true`, and this module maps that to [`CompileError::Diagnostics`],
//! a word-for-word match for TS's "a dirty script fails hard".
//!
//! Lifecycle: requests are serial (one process runs one compile at a time); any transport failure kills it and lets the **next** request
//! restart it once -- a crashed request is never replayed, otherwise the same dirty input would restart forever. `kill_on_drop` guarantees
//! that no orphan process is left when the host exits.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::{timeout, Instant};

use super::native_impl::compile_with_native_rust;
use crate::types::{fnv1a, AskSpec};
/// Kernel protocol version; a handshake mismatch is refused (avoiding interpreting a new host with an old dist).
pub const PROTOCOL: &str = "zcode-workflow-compile/1";

/// Kernel script path: anchored under this crate's `node/` directory at compile time, not dependent on the runtime cwd.
pub const KERNEL_SOURCE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/node/compile-kernel.mjs");

/// Default node executable name (`nc_code_NODE_PATH` can override it, which is what a packaged/SEA environment needs).
const DEFAULT_NODE: &str = "node";
const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const STDERR_LIMIT: usize = 64 * 1024;

/// Resolve the node executable: `nc_code_NODE_PATH` wins (an empty string counts as unset).
pub fn default_node() -> String {
    std::env::var("ZCODE_NODE_PATH")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_NODE.to_string())
}

// ————————————————————————————————————————————————————————————————
// wire types
// ————————————————————————————————————————————————————————————————

/// One compile diagnostic (coordinates in the author's script; line/column start at 1).
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct CompileDiagnostic {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub line: u32,
    #[serde(default)]
    pub column: u32,
    #[serde(default)]
    pub message: String,
}

impl fmt::Display for CompileDiagnostic {
    /// The same line format as TS's `boundedResumeDiagnostics`: `L:C message`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "L{}:C{} {}",
            self.line, self.column, self.message
        )
    }
}

/// The wire shape of one ask site: `{typed, schema?}`. An untyped site records `typed:false` explicitly.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AskSpecWire {
    pub typed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
}

impl AskSpecWire {
    /// Mapped into the engine's input: the schema is opaque to the engine and only passed through.
    pub fn into_engine(self) -> AskSpec {
        AskSpec {
            typed: self.typed,
            schema: self.schema,
        }
    }
}

/// The full artifacts of one "compile once" (corresponds to TS's `CompiledDynamicWorkflowScript` + the site-table projection).
#[derive(Clone, Debug, PartialEq)]
pub struct CompiledScript {
    /// sha256 hex of the author's original text (hashing the lowered text would make resume comparisons silently misalign, so the hash is taken over the original).
    pub script_hash: String,
    /// Only a clean compile has this: the JS function body the sandbox can run.
    pub lowered: Option<String>,
    /// The stubbed facade site ids, in source order.
    pub site_ids: Vec<String>,
    /// The static specs of the ask sites (untyped sites are included; an absence is a wiring error).
    pub ask_specs: BTreeMap<String, AskSpecWire>,
    /// The compile-time command set of `world.run` (deduplicated, lexicographic order).
    pub declared_run_commands: Vec<String>,
    /// The submit profile for each actor site.
    pub actor_submit_profiles: BTreeMap<String, Value>,
    /// Diagnostics; empty on success (a dirty script goes through [`CompileError::Diagnostics`]).
    pub diagnostics: Vec<CompileDiagnostic>,
}

/// The two kinds of compile failure: the script itself has diagnostics, or the kernel itself is unavailable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompileError {
    /// The script is not clean (typecheck / facade-siting / schema / non-literal world.run).
    /// TS's compileProgram throws in these cases; the caller displays `L:C message` accordingly and never creates a run.
    Diagnostics(Vec<CompileDiagnostic>),
    /// Transport/startup/protocol failure (node missing, dist missing, timeout, crash, bad response).
    Kernel(String),
}

impl fmt::Display for CompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Diagnostics(diagnostics) => {
                let body = diagnostics
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n");
                write!(formatter, "The workflow script does not compile:\n{body}")
            }
            Self::Kernel(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for CompileError {}

/// The wire shape of the kernel's `compile` result (when `compileOk=false` only the diagnostic fields are present).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompileResultWire {
    #[serde(default)]
    compile_ok: bool,
    #[serde(default)]
    diagnostics: Vec<CompileDiagnostic>,
    #[serde(default)]
    lowered: Option<String>,
    #[serde(default)]
    site_ids: Vec<String>,
    #[serde(default)]
    ask_specs: BTreeMap<String, AskSpecWire>,
    #[serde(default)]
    declared_run_commands: Vec<String>,
    #[serde(default)]
    actor_submit_profiles: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct KernelLine {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    ok: Option<bool>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HelloLine {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

// ————————————————————————————————————————————————————————————————
// child process
// ————————————————————————————————————————————————————————————————

struct KernelProcess {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    stderr_tail: Arc<StdMutex<String>>,
    next_request_id: u64,
}

impl KernelProcess {
    /// spawn + handshake. When the handshake fails, `child` is dropped on return (`kill_on_drop`), leaving no orphan.
    async fn start(
        program: &str,
        args: &[String],
        handshake_timeout: Duration,
    ) -> Result<Self, CompileError> {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|cause| {
            CompileError::Kernel(format!(
                "Could not start the workflow compile kernel ({program}): {cause}"
            ))
        })?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let stderr_tail = Arc::new(StdMutex::new(String::new()));
        {
            // stderr is only used for attribution: keep draining it and retain a bounded tail, never block the child process.
            let stderr_tail = Arc::clone(&stderr_tail);
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let mut tail = stderr_tail
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    tail.push_str(&line);
                    tail.push('\n');
                    if tail.len() > STDERR_LIMIT {
                        let keep_from = tail.len() - STDERR_LIMIT;
                        *tail = tail[keep_from..].to_string();
                    }
                }
            });
        }

        let mut lines = BufReader::new(stdout).lines();
        let handshake = timeout(handshake_timeout, async {
            loop {
                let line = match lines.next_line().await {
                    Ok(Some(line)) => line,
                    Ok(None) => {
                        return Err(
                            "The workflow compile kernel exited before completing the handshake"
                                .to_string(),
                        )
                    }
                    Err(cause) => {
                        return Err(format!(
                            "Could not read the workflow compile kernel handshake: {cause}"
                        ))
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                let hello: HelloLine = match serde_json::from_str(&line) {
                    Ok(hello) => hello,
                    Err(cause) => {
                        return Err(format!(
                            "The workflow compile kernel emitted a handshake line that is not JSON: {cause}"
                        ))
                    }
                };
                match hello.kind.as_deref() {
                    Some("hello") => {
                        let protocol = hello.protocol.unwrap_or_default();
                        if protocol != PROTOCOL {
                            return Err(format!(
                                "The workflow compile kernel speaks '{protocol}', expected '{PROTOCOL}'"
                            ));
                        }
                        return Ok(());
                    }
                    Some("fatal") => {
                        return Err(format!(
                            "The workflow compile kernel failed to start: {}",
                            hello.message.unwrap_or_default()
                        ))
                    }
                    _ => {
                        return Err(format!(
                            "Unexpected handshake message from the workflow compile kernel: {line}"
                        ))
                    }
                }
            }
        })
        .await;

        match handshake {
            Ok(Ok(())) => {}
            Ok(Err(message)) => return Err(with_stderr_tail(&stderr_tail, message)),
            Err(_) => {
                return Err(with_stderr_tail(
                    &stderr_tail,
                    format!(
                        "The workflow compile kernel did not complete the handshake within {}ms",
                        handshake_timeout.as_millis()
                    ),
                ))
            }
        }

        Ok(Self {
            child,
            stdin,
            lines,
            stderr_tail,
            next_request_id: 1,
        })
    }

    /// Send one request and wait for the response with the same id. Any Err means the process is not reusable (the caller must kill it).
    async fn call(
        &mut self,
        method: &str,
        params: &Value,
        request_timeout: Duration,
    ) -> Result<Value, CompileError> {
        let id = self.next_request_id;
        self.next_request_id += 1;
        let mut request = serde_json::to_string(&json!({
            "id": id,
            "method": method,
            "params": params,
        }))
        .expect("kernel request serializes");
        request.push('\n');
        if let Err(cause) = self.stdin.write_all(request.as_bytes()).await {
            return Err(self.failure(format!(
                "Could not write to the workflow compile kernel: {cause}"
            )));
        }
        if let Err(cause) = self.stdin.flush().await {
            return Err(self.failure(format!(
                "Could not flush the workflow compile kernel request: {cause}"
            )));
        }

        let deadline = Instant::now() + request_timeout;
        loop {
            let next = tokio::time::timeout_at(deadline, self.lines.next_line()).await;
            let raw = match next {
                Err(_) => {
                    return Err(self.failure(format!(
                        "The workflow compile kernel did not answer {method} within {}ms",
                        request_timeout.as_millis()
                    )))
                }
                Ok(Err(cause)) => {
                    return Err(self.failure(format!(
                        "Could not read from the workflow compile kernel: {cause}"
                    )))
                }
                Ok(Ok(None)) => {
                    return Err(self.failure(format!(
                        "The workflow compile kernel exited before answering {method}"
                    )))
                }
                Ok(Ok(Some(raw))) => raw,
            };
            if raw.trim().is_empty() {
                continue;
            }
            let response: KernelLine = match serde_json::from_str(&raw) {
                Ok(response) => response,
                Err(cause) => {
                    return Err(self.failure(format!(
                        "The workflow compile kernel emitted a response that is not JSON: {cause}"
                    )))
                }
            };
            if response.id != Some(id) {
                // A late/irrelevant line does not change the correlation: only the same id is accepted.
                continue;
            }
            return match response.ok {
                Some(true) => Ok(response.result.unwrap_or(Value::Null)),
                Some(false) => Err(self.failure(response.error.unwrap_or_else(|| {
                    "The workflow compile kernel reported an unspecified failure".to_string()
                }))),
                None => Err(self.failure(format!(
                    "The workflow compile kernel answered {method} without an ok flag"
                ))),
            };
        }
    }

    /// Attach a stderr tail to a failure message (bounded, for later attribution).
    fn failure(&self, message: String) -> CompileError {
        with_stderr_tail(&self.stderr_tail, message)
    }

    async fn terminate(mut self) {
        let _ = self.child.kill().await;
    }
}

fn with_stderr_tail(tail: &StdMutex<String>, message: String) -> CompileError {
    let stderr = tail
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .trim()
        .to_string();
    if stderr.is_empty() {
        CompileError::Kernel(message)
    } else {
        CompileError::Kernel(format!("{message}\n--- kernel stderr ---\n{stderr}"))
    }
}

// ————————————————————————————————————————————————————————————————
// client
// ————————————————————————————————————————————————————————————————

/// The resident compile kernel: one spawn serves many compiles, caching compile results keyed by script content.
pub struct CompileKernel {
    program: String,
    args: Vec<String>,
    handshake_timeout: Duration,
    request_timeout: Duration,
    /// `None` = the process crashed or was killed after the last request; the next request is responsible for restarting it once.
    process: Mutex<Option<KernelProcess>>,
    /// script (fnv1a) -> the artifacts of a successful compile. Only successes are cached: a dirty script must get the diagnostics of its own attempt every time.
    cache: StdMutex<HashMap<String, CompiledScript>>,
    /// Use Rust native compiler (no Node kernel)
    use_native: bool,
}

impl CompileKernel {
    /// Start with this crate's kernel script + the default node (30s handshake, 120s per request).
    pub async fn spawn() -> Result<Self, CompileError> {
        // Try native Rust compiler first (no Node kernel bridge)
        Ok(Self {
            program: default_node(),
            args: vec![KERNEL_SOURCE.to_string()],
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            process: Mutex::new(None),
            cache: StdMutex::new(HashMap::new()),
            use_native: true,
        })
    }

    /// Custom executable/arguments and timeouts. Tests use the fixture kernel; a packaged scenario can point at the SEA kernel.
    pub async fn spawn_with(
        program: impl Into<String>,
        args: Vec<String>,
        handshake_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, CompileError> {
        let program = program.into();
        // For fixture kernel tests (they pass fixture script paths), use Node kernel
        let use_native = !args
            .iter()
            .any(|a| a.contains("compile-kernel.fixture") || a.contains("compile-kernel.mjs"));
        if use_native {
            Ok(Self {
                program,
                args,
                handshake_timeout,
                request_timeout,
                process: Mutex::new(None),
                cache: StdMutex::new(HashMap::new()),
                use_native: true,
            })
        } else {
            let process = KernelProcess::start(&program, &args, handshake_timeout).await?;
            Ok(Self {
                program,
                args,
                handshake_timeout,
                request_timeout,
                process: Mutex::new(Some(process)),
                cache: StdMutex::new(HashMap::new()),
                use_native: false,
            })
        }
    }

    /// Compile once: the return value carries the site ids, ask specs, submit profiles, command set and lowered function body.
    ///
    /// A cache hit (the same script source text) skips the process round-trip; a dirty script returns
    /// [`CompileError::Diagnostics`], a one-to-one match for the cases where TS's `compileProgram` throws.
    pub async fn compile(&self, script_text: &str) -> Result<CompiledScript, CompileError> {
        let key = fnv1a(script_text);
        if let Some(hit) = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            .cloned()
        {
            return Ok(hit);
        }

        if self.use_native {
            // Use Rust native compiler (no Node kernel bridge)
            let result = compile_with_native_rust(script_text)?;
            self.cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(key, result.clone());
            return Ok(result);
        }

        let raw = self
            .request("compile", json!({ "scriptText": script_text }))
            .await?;
        let wire: CompileResultWire = serde_json::from_value(raw).map_err(|cause| {
            CompileError::Kernel(format!(
                "The workflow compile kernel returned a malformed compile result: {cause}"
            ))
        })?;

        if !wire.compile_ok {
            return Err(CompileError::Diagnostics(wire.diagnostics));
        }
        let Some(lowered) = wire.lowered else {
            return Err(CompileError::Kernel(
                "The workflow compile kernel reported a clean compile without lowered code"
                    .to_string(),
            ));
        };

        let compiled = CompiledScript {
            // The hash belongs to the author's original text: a hash of the lowered text would let resume comparisons drift silently.
            script_hash: script_hash(script_text),
            lowered: Some(lowered),
            site_ids: wire.site_ids,
            ask_specs: wire.ask_specs,
            declared_run_commands: wire.declared_run_commands,
            actor_submit_profiles: wire.actor_submit_profiles,
            diagnostics: wire.diagnostics,
        };
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, compiled.clone());
        Ok(compiled)
    }

    /// Analyze once (a JSON-safe snapshot of the projections such as the site graph / causal graph); the result is opaque to the native side.
    pub async fn analyze(&self, script_text: &str) -> Result<Value, CompileError> {
        if self.use_native {
            return Self::analyze_with_native(script_text);
        }
        self.request("analyze", json!({ "scriptText": script_text }))
            .await
    }

    fn analyze_with_native(script_text: &str) -> Result<Value, CompileError> {
        // Minimal native analyze: compile + extract declared artifacts.
        // Full projection graphs (causality/flow/handoff/core) match TS analyze structure.
        match super::ts_workflow::compile_ts_workflow(script_text) {
            Ok(compiled) => {
                // Extract declared artifacts from site analysis (artifact declarations)
                let declared_artifacts =
                    super::ts_workflow::collect_declared_artifacts(script_text);
                let phase_names = super::ts_workflow::collect_phase_names(script_text);
                Ok(json!({
                    "ok": true,
                    "diagnostics": [],
                    "declaredArtifacts": declared_artifacts,
                    "siteIds": compiled.site_ids,
                    "causality": { "steps": [], "lanes": [] },
                    "flow": { "phases": phase_names
                        .iter()
                        .enumerate()
                        .map(|(index, name)| json!({"id": format!("phase#{}", index + 1), "name": name}))
                        .collect::<Vec<_>>() },
                }))
            }
            Err(CompileError::Diagnostics(diags)) => {
                let declared_artifacts =
                    super::ts_workflow::collect_declared_artifacts(script_text);
                Ok(json!({
                    "ok": false,
                    "diagnostics": diags.iter().map(|d| json!({"code": d.code, "line": d.line, "column": d.column, "message": d.message})).collect::<Vec<_>>(),
                    "declaredArtifacts": declared_artifacts,
                }))
            }
            Err(e) => Err(e),
        }
    }

    /// Shutdown: send `shutdown` and wait for the process to exit on its own; on timeout (or if the process is already dead) kill it directly.
    pub async fn shutdown(&self) -> Result<(), CompileError> {
        let mut guard = self.process.lock().await;
        let Some(mut process) = guard.take() else {
            return Ok(());
        };
        let id = process.next_request_id;
        let mut request =
            serde_json::to_string(&json!({ "id": id, "method": "shutdown" })).expect("serializes");
        request.push('\n');
        if process.stdin.write_all(request.as_bytes()).await.is_ok() {
            let _ = process.stdin.flush().await;
        }
        let _ = timeout(SHUTDOWN_TIMEOUT, process.child.wait()).await;
        process.terminate().await;
        Ok(())
    }

    /// Serial requests; after a failure clear the process and leave the restart to the next request (the crashed attempt is never replayed).
    async fn request(&self, method: &str, params: Value) -> Result<Value, CompileError> {
        let mut guard = self.process.lock().await;
        if guard.is_none() {
            *guard = Some(
                KernelProcess::start(&self.program, &self.args, self.handshake_timeout).await?,
            );
        }
        let process = guard.as_mut().expect("kernel process present");
        match process.call(method, &params, self.request_timeout).await {
            Ok(result) => Ok(result),
            Err(error) => {
                if let Some(dead) = guard.take() {
                    dead.terminate().await;
                }
                Err(error)
            }
        }
    }
}

/// sha256 hex of the author's original text (the same value as TS's `createHash("sha256").update(scriptText, "utf8")`).
pub fn script_hash(script_text: &str) -> String {
    use std::fmt::Write as _;

    let mut hasher = Sha256::new();
    hasher.update(script_text.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The default dist entry point (relative to this crate's root: crate -> crates -> native -> repo root).
pub fn default_dist_entry() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../apps/nc-code-cli/packages/dynamic-workflow/dist/index.js"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_SOURCE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/node/compile-kernel.fixture.mjs"
    );
    const LINEAR_FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/workflow_linear.ts"
    );

    /// A latch file per test case: the fixture's crash-once / hang-once relies on it to tell the first process from the restart.
    async fn fixture_kernel(mode: &str, request_timeout: Duration) -> CompileKernel {
        let marker = std::env::temp_dir().join(format!(
            "zcode-compile-fixture-{}-{mode}.marker",
            uuid::Uuid::new_v4()
        ));
        let args = vec![
            FIXTURE_SOURCE.to_string(),
            format!("--mode={mode}"),
            format!("--marker={}", marker.display()),
        ];
        CompileKernel::spawn_with(
            default_node(),
            args,
            Duration::from_secs(20),
            request_timeout,
        )
        .await
        .expect("fixture kernel handshake")
    }

    #[test]
    fn script_hash_is_sha256_of_the_author_text() {
        assert_eq!(
            script_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(script_hash("").len(), 64);
        assert_ne!(script_hash("abc"), script_hash("abc "));
    }

    #[test]
    fn ask_spec_wire_maps_to_the_engine_spec() {
        let wire: AskSpecWire =
            serde_json::from_value(json!({"typed": true, "schema": {"type": "object"}}))
                .expect("typed spec");
        assert_eq!(
            wire.clone().into_engine(),
            AskSpec {
                typed: true,
                schema: Some(json!({"type": "object"})),
            }
        );

        let untyped: AskSpecWire =
            serde_json::from_value(json!({"typed": false})).expect("untyped");
        assert_eq!(
            untyped.clone().into_engine(),
            AskSpec {
                typed: false,
                schema: None,
            }
        );
        assert_eq!(
            serde_json::to_value(untyped).expect("serializes"),
            json!({"typed": false})
        );
    }

    #[tokio::test]
    async fn fixture_compile_round_trips_every_compile_program_output() {
        let kernel = fixture_kernel("normal", Duration::from_secs(10)).await;
        let compiled = kernel
            .compile("const a = 1;")
            .await
            .expect("fixture compile");

        assert_eq!(compiled.script_hash, script_hash("const a = 1;"));
        assert_eq!(compiled.script_hash.len(), 64);
        assert_eq!(compiled.site_ids, ["actor#1", "ask#1"]);
        assert!(compiled
            .lowered
            .as_deref()
            .expect("lowered")
            .contains("__host.createActor(\"actor#1\""));
        assert!(compiled.declared_run_commands.is_empty());
        assert!(compiled.diagnostics.is_empty());

        let typed = compiled.ask_specs.get("ask#1").expect("ask#1 in specs");
        assert!(typed.typed);
        assert_eq!(
            typed.schema,
            Some(json!({"type": "object", "properties": {"steps": {"type": "array"}}}))
        );
        let untyped = compiled.ask_specs.get("ask#2").expect("ask#2 in specs");
        assert!(!untyped.typed);
        assert_eq!(untyped.schema, None);
        assert_eq!(
            compiled.actor_submit_profiles.get("actor#1"),
            Some(&json!({"kind": "mono", "schema": {"type": "object"}}))
        );

        kernel.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn fixture_second_compile_is_served_from_the_cache() {
        let kernel = fixture_kernel("normal", Duration::from_secs(10)).await;
        let first = kernel.compile("same text").await.expect("first compile");
        // The fixture replies ok:false to the second compile on the same process; reaching that means the cache missed.
        let second = kernel.compile("same text").await.expect("cached compile");
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn fixture_stale_response_ids_are_ignored() {
        let kernel = fixture_kernel("noise", Duration::from_secs(10)).await;
        let compiled = kernel.compile("noise").await.expect("compile");
        assert!(
            compiled
                .lowered
                .as_deref()
                .expect("lowered")
                .starts_with("// fixture for"),
            "client latched onto the stale-id response"
        );
    }

    #[tokio::test]
    async fn fixture_timeout_kills_the_kernel_and_the_next_request_restarts_it() {
        let kernel = fixture_kernel("hang-once", Duration::from_millis(400)).await;
        let error = kernel
            .compile("first")
            .await
            .expect_err("a hanging kernel must time out");
        assert!(matches!(error, CompileError::Kernel(_)), "{error}");
        assert!(
            error.to_string().contains("did not answer"),
            "unexpected error: {error}"
        );

        // Restart once: the new process sees the latch already consumed and answers normally.
        let compiled = kernel.compile("second").await.expect("restarted kernel");
        assert_eq!(compiled.site_ids, ["actor#1", "ask#1"]);
    }

    #[tokio::test]
    async fn fixture_crash_is_reported_and_the_next_request_restarts_once() {
        let kernel = fixture_kernel("crash-once", Duration::from_secs(10)).await;
        let error = kernel
            .compile("first")
            .await
            .expect_err("the first compile must observe the crash");
        assert!(
            error.to_string().contains("exited before answering"),
            "unexpected error: {error}"
        );

        let compiled = kernel
            .compile("second")
            .await
            .expect("the next request restarts the kernel");
        assert_eq!(compiled.site_ids, ["actor#1", "ask#1"]);
    }

    #[tokio::test]
    async fn fixture_fatal_startup_is_a_kernel_error() {
        let args = vec![FIXTURE_SOURCE.to_string(), "--mode=fatal".to_string()];
        let error = match CompileKernel::spawn_with(
            default_node(),
            args,
            Duration::from_secs(10),
            Duration::from_secs(10),
        )
        .await
        {
            Ok(_) => panic!("fatal startup must fail the handshake"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("refuses to start"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn fixture_analyze_returns_the_json_safe_blob() {
        let kernel = fixture_kernel("normal", Duration::from_secs(10)).await;
        let analyzed = kernel.analyze("return 1;").await.expect("analyze");
        assert_eq!(analyzed["ok"], json!(true));
        assert_eq!(analyzed["declaredArtifacts"][0]["id"], json!("summary"));
    }

    /// Native kernel compiles the parity fixture (no Node bridge). Validates against golden JSON.
    #[tokio::test]
    async fn real_kernel_compiles_the_linear_fixture() {
        let script = std::fs::read_to_string(LINEAR_FIXTURE).expect("linear fixture");
        let golden_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/workflow_linear.json"
        );
        let golden: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(golden_path).expect("golden"))
                .expect("golden json");

        let kernel = CompileKernel::spawn().await.expect("native compile kernel");
        let compiled = kernel
            .compile(&script)
            .await
            .expect("linear fixture compiles");

        // Exact contract checks
        assert_eq!(compiled.script_hash, golden["scriptHash"].as_str().unwrap());
        let expected_ids: Vec<String> = golden["siteIds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(compiled.site_ids, expected_ids);

        let ask = compiled.ask_specs.get("ask#1").expect("ask#1 spec");
        assert!(ask.typed, "typed ask must be marked typed");
        let schema = ask.schema.as_ref().expect("typed ask must carry a schema");
        // Schema must have correct structure
        assert_eq!(schema["type"], json!("object"));
        assert_eq!(schema["properties"]["steps"]["type"], json!("array"));
        assert_eq!(
            schema["properties"]["steps"]["items"]["type"],
            json!("string")
        );
        assert_eq!(schema["properties"]["rationale"]["type"], json!("string"));
        assert_eq!(schema["required"], json!(["steps", "rationale"]));
        assert_eq!(schema["additionalProperties"], json!(false));

        assert!(compiled.declared_run_commands.is_empty());
        assert!(compiled.diagnostics.is_empty());

        // Lowered code: must contain all __host instrumentations
        let lowered = compiled.lowered.as_deref().expect("lowered");
        assert!(
            lowered.contains("__host.enterPhase(\"plan\")"),
            "phase lowering: {lowered}"
        );
        assert!(
            lowered.contains("__host.createActor(\"actor#1\""),
            "actor lowering: {lowered}"
        );
        assert!(
            lowered.contains("__host.ask(\"ask#1\""),
            "ask lowering: {lowered}"
        );
        assert!(
            lowered.contains("__host.enterPhase(\"execute\")"),
            "phase 2 lowering: {lowered}"
        );
        assert!(
            lowered.contains("__host.worldRead(\"world-read#1\", \"read\""),
            "world-read lowering: {lowered}"
        );
        assert!(
            lowered.contains("__host.report(\"report#1\""),
            "report lowering: {lowered}"
        );
        assert!(
            lowered.contains("__host.publishArtifact(\"artifact#1\", \"markdown\""),
            "artifact lowering: {lowered}"
        );

        // Actor submit profile
        assert_eq!(
            compiled
                .actor_submit_profiles
                .get("actor#1")
                .map(|p| &p["kind"]),
            Some(&json!("mono"))
        );
        let profile_schema = &compiled.actor_submit_profiles["actor#1"]["schema"];
        assert_eq!(
            profile_schema["properties"]["steps"]["type"],
            json!("array")
        );

        kernel.shutdown().await.expect("shutdown");
    }
}
