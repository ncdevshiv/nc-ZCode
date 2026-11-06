//! The kernel NDJSON frame protocol — Rust ↔ kernel_driver.mjs inside the user's Node.
//!
//! Mirrors the two-sided shapes of `src/driver/kernel_driver.mjs` frame by frame:
//! - Outbound (Rust → kernel stdin): evaluate / cancel / reset / status / addModuleDir / shutdown
//!   (the `frame.op` branches of `handleFrame` at kernel_driver.mjs:795-871).
//! - Inbound (kernel stdout → Rust): the ready event, result / cancel / reset / status /
//!   moduleDir / shutdown replies, error and fatal (kernel_driver.mjs:795-928).
//!
//! stdout is a pure protocol channel (kernel_driver.mjs:47-51): the kernel side writes frames only through writeFrame,
//! so the Rust side treats an unparsable line as "impossible to be a reply" (the waiter has its own timeout fallback and never hangs),
//! and for `kind:"fatal"` (uncaughtException/unhandledRejection) it marks the kernel as dead.
//!
//! The protocol version is checked verbatim in the ready frame (the KERNEL_PROTOCOL constant); a mismatch is fail-closed:
//! the driver and the Rust side must be the same generation of frame shapes.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The protocol id announced by the kernel driver (kernel_driver.mjs:24).
pub const KERNEL_PROTOCOL: &str = "zcode-node-repl-kernel/1";

// ── Outbound frames (Rust → kernel) ──

/// One evaluate request (the fields read at kernel_driver.mjs:722-725).
/// The field names must be camelCase (syncTimeoutMs/requestMeta) — the driver reads exactly those two keys,
/// serializing them as snake_case would make the driver fall back to its defaults and silently drop the caller's timeout setting.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluateFrame {
    pub id: u64,
    pub op: &'static str,
    pub code: String,
    /// The vm synchronous execution budget; the driver clamps it to >=1 (kernel_driver.mjs:725).
    pub sync_timeout_ms: u64,
    /// The context passed to the sandbox's `nodeRepl.requestMeta` (kernel_driver.mjs:729).
    pub request_meta: Value,
}

impl EvaluateFrame {
    pub fn new(
        id: u64,
        code: impl Into<String>,
        sync_timeout_ms: u64,
        request_meta: Value,
    ) -> Self {
        Self {
            id,
            op: "evaluate",
            code: code.into(),
            sync_timeout_ms,
            request_meta,
        }
    }
}

/// The reason for a cancel: the driver preserves the TimeoutError / AbortError distinction
/// (kernel_driver.mjs:810-813, mirroring executors.ts:67-73).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    TimeoutError,
    AbortError,
}

impl CancelReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TimeoutError => "TimeoutError",
            Self::AbortError => "AbortError",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct CancelFrame {
    pub id: u64,
    pub op: &'static str,
    pub reason: &'static str,
    pub message: String,
}

impl CancelFrame {
    pub fn new(id: u64, reason: CancelReason, message: impl Into<String>) -> Self {
        Self {
            id,
            op: "cancel",
            reason: reason.as_str(),
            message: message.into(),
        }
    }
}

/// Single-operation frame (reset / status / shutdown): only id + op.
#[derive(Debug, Serialize)]
pub struct SimpleFrame {
    pub id: u64,
    pub op: &'static str,
}

impl SimpleFrame {
    pub fn reset(id: u64) -> Self {
        Self { id, op: "reset" }
    }

    pub fn status(id: u64) -> Self {
        Self { id, op: "status" }
    }

    pub fn shutdown(id: u64) -> Self {
        Self { id, op: "shutdown" }
    }
}

/// The addModuleDir frame (kernel_driver.mjs:832-857): injects the host-chosen node_modules
/// directories into the module resolution path (removed from the tool surface since 0.6.0, but the kernel capability is kept for the host).
#[derive(Debug, Serialize)]
pub struct AddModuleDirFrame {
    pub id: u64,
    pub op: &'static str,
    pub dir: String,
}

impl AddModuleDirFrame {
    pub fn new(id: u64, dir: impl Into<String>) -> Self {
        Self {
            id,
            op: "addModuleDir",
            dir: dir.into(),
        }
    }
}

// ── Inbound frames (kernel → Rust) ──

/// The ready event (kernel_driver.mjs:917-928): the handshake and the diagnostic facts.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadyEvent {
    pub event: String,
    pub protocol: String,
    pub pid: u32,
    pub node: String,
    pub exec_path: String,
    pub cwd: String,
    pub restrict_process: bool,
    pub generation: u64,
    pub module_dirs: Vec<String>,
}

/// The error object on the JS output side (strict {name, message, stack?};
/// contracts/src/tools/node-repl.ts:48-53 — code never enters the tool JSON, see error.rs).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct KernelJsError {
    pub name: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}

/// The output of a kernel result frame (kernel_driver.mjs:711-720 buildOutput).
/// The key names must stay camelCase (responseMeta/structuredResults) — the driver emits exactly these keys,
/// without the rename the metadata in the response would be dropped as unknown keys.
///
/// `structured_results` is the SDK content-block channel for the CUA/Browser bridge: they **do not enter**
/// the JsOutput strict schema (contracts' JsOutputSchema has no such field, and the core handler
/// node-repl.ts:265 does not pass them through either) — the host's formatModelContent uses them to build content blocks, while this layer
/// carries them verbatim and never drops them silently (the consumer lives in the host wiring, as noted in the crate docs).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KernelOutput {
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub logs: String,
    #[serde(default)]
    pub error: Option<KernelJsError>,
    #[serde(default)]
    pub images: Option<Vec<KernelImage>>,
    #[serde(default)]
    pub structured_results: Option<Vec<Value>>,
    #[serde(default)]
    pub response_meta: Option<Value>,
}

/// The images collected by nodeRepl.emitImage (the `{ base64, mimeType }` at kernel_driver.mjs:196).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KernelImage {
    pub base64: String,
    pub mime_type: String,
}

/// The status reply (kernel_driver.mjs:823-831).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusFrame {
    pub generation: u64,
    pub busy: bool,
    pub restrict_process: bool,
    pub module_dirs: Vec<String>,
}

/// The moduleDir reply (kernel_driver.mjs:833-857).
#[derive(Debug, Clone, Deserialize)]
pub struct ModuleDirFrame {
    pub ok: bool,
    #[serde(default)]
    pub dirs: Vec<String>,
    #[serde(default)]
    pub error: Option<KernelJsError>,
}

/// The classified inbound frame.
#[derive(Debug, Clone)]
pub enum InboundFrame {
    Ready(ReadyEvent),
    /// `kind:"result"`; the id correlates with the evaluate call.
    Result {
        id: u64,
        output: KernelOutput,
    },
    /// `kind:"cancel"`; acknowledged=false means that id is no longer in the active table.
    CancelAck {
        id: u64,
        acknowledged: bool,
    },
    ResetAck {
        id: u64,
        generation: u64,
    },
    Status {
        id: u64,
        status: StatusFrame,
    },
    ModuleDir {
        id: u64,
        frame: ModuleDirFrame,
    },
    ShutdownAck {
        id: u64,
    },
    /// `kind:"error"`: operation-level failure (may or may not carry an id — the bad-frame path).
    Error {
        id: Option<u64>,
        error: KernelJsError,
    },
    /// `kind:"fatal"`: uncaughtException/unhandledRejection, the kernel is about to exit.
    Fatal(KernelJsError),
}

/// Parse one stdout line into an inbound frame.
///
/// Returns `Ok(None)`: an empty line / an unparsable line / an unknown kind — none of these can be the reply
/// any waiter is awaiting (a reply always carries kind); each waiter falls back on its own timeout, so a bad line never hangs it and never panics.
pub fn parse_inbound(line: &str) -> Option<InboundFrame> {
    let value: Value = serde_json::from_str(line).ok()?;
    let object = value.as_object()?;
    let kind = object.get("kind").and_then(Value::as_str)?;
    let id = object.get("id").and_then(Value::as_u64);
    match kind {
        "event" => {
            let ready: ReadyEvent = serde_json::from_value(value).ok()?;
            (ready.event == "ready").then_some(InboundFrame::Ready(ready))
        }
        "result" => {
            let id = id?;
            let output: KernelOutput =
                serde_json::from_value(object.get("output")?.clone()).ok()?;
            Some(InboundFrame::Result { id, output })
        }
        "cancel" => Some(InboundFrame::CancelAck {
            id: id?,
            acknowledged: object
                .get("acknowledged")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        "reset" => Some(InboundFrame::ResetAck {
            id: id?,
            generation: object
                .get("generation")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        }),
        "status" => {
            let status: StatusFrame = serde_json::from_value(value).ok()?;
            Some(InboundFrame::Status { id: id?, status })
        }
        "moduleDir" => {
            let frame: ModuleDirFrame = serde_json::from_value(value).ok()?;
            Some(InboundFrame::ModuleDir { id: id?, frame })
        }
        "shutdown" => Some(InboundFrame::ShutdownAck { id: id? }),
        "error" => {
            let error: KernelJsError = serde_json::from_value(object.get("error")?.clone()).ok()?;
            Some(InboundFrame::Error { id, error })
        }
        "fatal" => {
            let error: KernelJsError = serde_json::from_value(object.get("error")?.clone()).ok()?;
            Some(InboundFrame::Fatal(error))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn evaluate_frame_shape_matches_driver_reader() {
        let frame = EvaluateFrame::new(7, "1+1", 5_000, json!({"session_id": "s"}));
        let value = serde_json::to_value(&frame).unwrap();
        // kernel_driver.mjs:722-729 reads op/code/syncTimeoutMs/requestMeta/id.
        assert_eq!(value["op"], "evaluate");
        assert_eq!(value["code"], "1+1");
        assert_eq!(value["syncTimeoutMs"], 5_000);
        assert_eq!(value["requestMeta"]["session_id"], "s");
        assert_eq!(value["id"], 7);
    }

    #[test]
    fn cancel_reason_strings_match_driver_branch() {
        // kernel_driver.mjs:811: frame.reason === "TimeoutError" ? "TimeoutError" : "AbortError".
        assert_eq!(CancelReason::TimeoutError.as_str(), "TimeoutError");
        assert_eq!(CancelReason::AbortError.as_str(), "AbortError");
    }

    #[test]
    fn ready_event_parses_and_protocol_is_verbatim() {
        let line = r#"{"kind":"event","event":"ready","protocol":"zcode-node-repl-kernel/1","pid":42,"node":"v24.0.0","execPath":"C:/node/node.exe","cwd":"C:/w","restrictProcess":true,"generation":0,"moduleDirs":[]}"#;
        let Some(InboundFrame::Ready(ready)) = parse_inbound(line) else {
            panic!("ready frame must parse");
        };
        assert_eq!(ready.protocol, KERNEL_PROTOCOL);
        assert_eq!(ready.pid, 42);
        assert!(ready.restrict_process);
    }

    #[test]
    fn result_frame_parses_driver_output_shape() {
        // The fields of kernel_driver.mjs:711-720 buildOutput (result may be omitted).
        let line = r#"{"id":3,"kind":"result","output":{"result":"2","logs":"","images":[{"base64":"AA==","mimeType":"image/png"}],"responseMeta":{"a":1}}}"#;
        let Some(InboundFrame::Result { id, output }) = parse_inbound(line) else {
            panic!("result frame must parse");
        };
        assert_eq!(id, 3);
        assert_eq!(output.result.as_deref(), Some("2"));
        assert_eq!(output.images.as_ref().unwrap().len(), 1);
        assert_eq!(output.response_meta.as_ref().unwrap()["a"], 1);
    }

    #[test]
    fn result_error_frame_carries_reset_suffix_message() {
        // After a cancel/timeout the error with RESET_SUFFIX appended by the driver must reach Rust verbatim
        // (kernel_driver.mjs:744-747).
        let line = r#"{"id":9,"kind":"result","output":{"logs":"","error":{"name":"TimeoutError","message":"x; kernel reset, all previous bindings were cleared; reinitialize browser/tab bindings before rerunning"}}}"#;
        let Some(InboundFrame::Result { output, .. }) = parse_inbound(line) else {
            panic!("error result must parse");
        };
        let error = output.error.unwrap();
        assert_eq!(error.name, "TimeoutError");
        assert!(error.message.contains("kernel reset"));
    }

    #[test]
    fn cancel_ack_and_status_and_fatal_parse() {
        let Some(InboundFrame::CancelAck { id, acknowledged }) =
            parse_inbound(r#"{"id":2,"kind":"cancel","acknowledged":true}"#)
        else {
            panic!("cancel ack");
        };
        assert_eq!(id, 2);
        assert!(acknowledged);

        let Some(InboundFrame::Status { id, status }) = parse_inbound(
            r#"{"id":5,"kind":"status","generation":2,"busy":false,"restrictProcess":true,"moduleDirs":["/a"]}"#,
        ) else {
            panic!("status frame");
        };
        assert_eq!(id, 5);
        assert_eq!(status.generation, 2);
        assert!(!status.busy);

        let Some(InboundFrame::Fatal(error)) =
            parse_inbound(r#"{"kind":"fatal","error":{"name":"Error","message":"boom"}}"#)
        else {
            panic!("fatal frame");
        };
        assert_eq!(error.message, "boom");
    }

    #[test]
    fn garbage_and_unknown_kind_yield_none_never_panic() {
        assert!(parse_inbound("").is_none());
        assert!(parse_inbound("not json").is_none());
        assert!(parse_inbound(r#"{"kind":"mystery"}"#).is_none());
        // When non-protocol text is mixed into stdout (user code writing past the sink), drop it as a bad line.
        assert!(parse_inbound("plain user text").is_none());
    }
}
