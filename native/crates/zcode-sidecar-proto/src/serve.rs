//! sidecar 服务循环：行读入 → 解析 → 分发 → 响应/事件写出。
//!
//! 不变式（spec 规则 1–5）：
//! - 客户端任何字节都不会让循环 panic：非法 UTF-8 / 非法 JSON 一律转 `proto_error` 事件后继续服务；
//! - 版本不匹配 → 400 `PROTOCOL_VERSION_MISMATCH` 响应 + 返回 `Shutdown::ProtocolError`（exit 1）；
//! - `method:"shutdown"` → 成功响应 + `Shutdown::Graceful`（exit 0）；stdin EOF 同路径；
//! - handler 与事件共用一把 writer 锁（`Emitter` = `Arc<Mutex<W>>`），事件可任意穿插响应。

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::{json, Value};

use crate::frame::{
    ErrorBody, Frame, CODE_BAD_REQUEST, CODE_INTERNAL, CODE_NOT_FOUND, SIDECAR_PROTOCOL_VERSION,
    VERSION_MISMATCH_PREFIX,
};

/// 协议错误事件名。
pub const EVENT_PROTO_ERROR: &str = "proto_error";
/// 内建关闭方法。
pub const METHOD_SHUTDOWN: &str = "shutdown";
/// `proto_error.raw` 的最大字节数（超出按 char 边界截断）。
pub const RAW_TRUNCATE_BYTES: usize = 512;

/// 服务循环的返回信号：bin 层据此决定退出码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shutdown {
    /// 优雅退出（shutdown 或 stdin EOF）→ exit 0。
    Graceful,
    /// 协议违规（版本不匹配），fail closed → exit 1。
    ProtocolError,
}

impl Shutdown {
    /// 对应的进程退出码。
    pub fn exit_code(self) -> i32 {
        match self {
            Shutdown::Graceful => 0,
            Shutdown::ProtocolError => 1,
        }
    }
}

/// handler 失败体：直接成为响应的 `error` 字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandlerError {
    pub body: ErrorBody,
}

impl HandlerError {
    /// 任意错误码（400/404/409/500 由调用方选择）。
    pub fn new(code: u16, message: impl Into<String>) -> Self {
        Self {
            body: ErrorBody {
                code,
                message: message.into(),
            },
        }
    }

    /// 404 未知方法 / 未知 id。
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(crate::frame::CODE_NOT_FOUND, message)
    }

    /// 409 状态冲突（如重复 scanId）。
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(crate::frame::CODE_CONFLICT, message)
    }

    /// 500 内部错误。
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(CODE_INTERNAL, message)
    }
}

/// 方法处理器：拿到 params 与 Emitter 句柄（可克隆进其它线程异步发事件）。
pub type HandlerFn<W> = dyn Fn(&Value, &Emitter<W>) -> Result<Value, HandlerError> + Send + Sync;

/// 方法名 → 处理器注册表。
pub struct Registry<W> {
    handlers: HashMap<String, Box<HandlerFn<W>>>,
}

impl<W> Default for Registry<W> {
    fn default() -> Self {
        Self::new()
    }
}

impl<W> Registry<W> {
    /// 空注册表。
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }

    /// 注册方法处理器（后注册覆盖先注册）。
    pub fn register<F>(&mut self, method: impl Into<String>, handler: F)
    where
        F: Fn(&Value, &Emitter<W>) -> Result<Value, HandlerError> + Send + Sync + 'static,
    {
        self.handlers.insert(method.into(), Box::new(handler));
    }

    pub(crate) fn get(&self, method: &str) -> Option<&HandlerFn<W>> {
        self.handlers.get(method).map(|boxed| &**boxed)
    }
}

/// 写出端共享句柄：所有响应与事件经同一把 `Mutex<W>` 串行化，flush 到行边界。
pub struct Emitter<W> {
    inner: Arc<Mutex<W>>,
}

impl<W> Clone for Emitter<W> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<W: Write> Emitter<W> {
    /// 包装一个 writer。
    pub fn new(writer: W) -> Self {
        Self {
            inner: Arc::new(Mutex::new(writer)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, W> {
        // handler panic 可能留下毒锁；写路径只做 append+flush，取内层值继续即可。
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 写出一帧（单行 JSON + `\n`）并 flush。
    pub fn write_frame(&self, frame: &Frame) -> io::Result<()> {
        let mut writer = self.lock();
        writer.write_all(frame.encode().as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()
    }

    /// 发出一个异步事件帧。
    pub fn emit(&self, event: &str, data: Value) -> io::Result<()> {
        self.write_frame(&Frame::event(event, data))
    }
}

impl<W: Write + AsRef<[u8]>> Emitter<W> {
    /// 读出当前已写入字节（测试与诊断用）。
    pub fn contents(&self) -> Vec<u8> {
        self.lock().as_ref().to_vec()
    }
}

/// 服务一条行协议连接，直到 EOF / shutdown / 版本不匹配。
pub fn serve<R: BufRead, W: Write>(
    mut reader: R,
    emitter: &Emitter<W>,
    registry: &Registry<W>,
) -> io::Result<Shutdown> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => return Ok(Shutdown::Graceful),
            Ok(_) => {}
            Err(err) => return Err(err),
        }
        // 客户端字节按 lossy 解码：非 UTF-8 行同样只触发 proto_error，绝不 panic。
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end_matches(&['\n', '\r'][..]);
        if let Some(shutdown) = handle_line(line, emitter, registry)? {
            return Ok(shutdown);
        }
    }
}

fn handle_line<W: Write>(
    line: &str,
    emitter: &Emitter<W>,
    registry: &Registry<W>,
) -> io::Result<Option<Shutdown>> {
    let frame = match Frame::parse(line) {
        Ok(frame) => frame,
        Err(err) => {
            emit_proto_error(emitter, line, &err.to_string())?;
            return Ok(None);
        }
    };

    // spec 规则 1：任何帧的 v 都必须等于当前协议版本，否则 fail closed。
    if frame.v() != SIDECAR_PROTOCOL_VERSION {
        let id = frame.id().cloned().unwrap_or(Value::Null);
        emitter.write_frame(&Frame::response_err(
            id,
            CODE_BAD_REQUEST,
            format!(
                "{VERSION_MISMATCH_PREFIX}: expected {SIDECAR_PROTOCOL_VERSION}, got {}",
                frame.v()
            ),
        ))?;
        return Ok(Some(Shutdown::ProtocolError));
    }

    match frame {
        Frame::Request {
            id, method, params, ..
        } => {
            // spec 规则 3：内建 shutdown，成功响应后优雅退出。
            if method == METHOD_SHUTDOWN {
                emitter.write_frame(&Frame::response_ok(id, json!({})))?;
                return Ok(Some(Shutdown::Graceful));
            }
            let response = match registry.get(&method) {
                None => {
                    Frame::response_err(id, CODE_NOT_FOUND, format!("unknown method: {method}"))
                }
                Some(handler) => {
                    // handler panic 属内部错误：映射 500 后继续服务，不击穿循环。
                    match panic::catch_unwind(AssertUnwindSafe(|| handler(&params, emitter))) {
                        Ok(Ok(result)) => Frame::response_ok(id, result),
                        Ok(Err(err)) => Frame::response_err(id, err.body.code, err.body.message),
                        Err(_) => Frame::response_err(
                            id,
                            CODE_INTERNAL,
                            "internal error: handler panicked",
                        ),
                    }
                }
            };
            emitter.write_frame(&response)?;
            Ok(None)
        }
        // 客户端不应发送响应/事件帧：按协议错误上报（spec 规则 2 的形状扩展）并继续服务。
        Frame::ResponseOk { .. } | Frame::ResponseErr { .. } | Frame::Event { .. } => {
            emit_proto_error(emitter, line, "unexpected response/event frame from client")?;
            Ok(None)
        }
    }
}

fn emit_proto_error<W: Write>(emitter: &Emitter<W>, line: &str, reason: &str) -> io::Result<()> {
    emitter.emit(
        EVENT_PROTO_ERROR,
        json!({ "raw": truncate_raw(line), "reason": reason }),
    )
}

/// 原始行截断到 `RAW_TRUNCATE_BYTES` 字节内，向前对齐 char 边界（保证是合法 UTF-8）。
pub fn truncate_raw(line: &str) -> &str {
    if line.len() <= RAW_TRUNCATE_BYTES {
        return line;
    }
    let mut end = RAW_TRUNCATE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    &line[..end]
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::mpsc;

    use super::*;
    use crate::frame::ParseError;
    use crate::params::bad_request;

    type TestIo = (io::Result<Shutdown>, String);

    fn serve_input(input: &[u8], build: impl FnOnce(&mut Registry<Vec<u8>>)) -> TestIo {
        let reader = Cursor::new(input.to_vec());
        let emitter = Emitter::new(Vec::new());
        let mut registry = Registry::new();
        build(&mut registry);
        let result = serve(reader, &emitter, &registry);
        let out = String::from_utf8_lossy(&emitter.contents()).into_owned();
        (result, out)
    }

    fn lines(output: &str) -> Vec<&str> {
        let trimmed = output.trim_end_matches('\n');
        if trimmed.is_empty() {
            Vec::new()
        } else {
            trimmed.split('\n').collect()
        }
    }

    fn error_code(line: &str) -> u16 {
        let value: Value = serde_json::from_str(line).expect("response line is json");
        assert_eq!(value["ok"], json!(false), "expected error response: {line}");
        value["error"]["code"].as_u64().expect("code") as u16
    }

    #[test]
    fn dispatch_success_and_eof_is_graceful() {
        let input = br#"{"v":1,"id":1,"method":"echo","params":{"a":1}}
"#;
        let (result, out) = serve_input(input, |registry| {
            registry.register("echo", |params, _emitter| Ok(params.clone()));
        });
        assert_eq!(result.expect("serve ok"), Shutdown::Graceful);
        assert_eq!(
            lines(&out),
            vec![r#"{"v":1,"id":1,"ok":true,"result":{"a":1}}"#],
        );
    }

    #[test]
    fn empty_input_returns_graceful_without_output() {
        let (result, out) = serve_input(b"", |_| {});
        assert_eq!(result.expect("serve ok"), Shutdown::Graceful);
        assert_eq!(out, "");
    }

    #[test]
    fn unknown_method_maps_to_404_and_keeps_serving() {
        let input = br#"{"v":1,"id":2,"method":"nope","params":{}}
{"v":1,"id":3,"method":"echo","params":{}}
"#;
        let (result, out) = serve_input(input, |registry| {
            registry.register("echo", |params, _| Ok(params.clone()));
        });
        assert_eq!(result.unwrap(), Shutdown::Graceful);
        let out = lines(&out);
        assert_eq!(error_code(out[0]), crate::frame::CODE_NOT_FOUND);
        assert!(out[0].contains("unknown method: nope"));
        assert_eq!(out[1], r#"{"v":1,"id":3,"ok":true,"result":{}}"#);
    }

    #[test]
    fn handler_error_codes_map_to_400_409_and_panic_to_500() {
        let input = br#"{"v":1,"id":1,"method":"bad","params":{}}
{"v":1,"id":2,"method":"conflict","params":{}}
{"v":1,"id":3,"method":"boom","params":{}}
{"v":1,"id":4,"method":"echo","params":{}}
"#;
        let (result, out) = serve_input(input, |registry| {
            registry.register("bad", |_, _| Err(bad_request("params broken")));
            registry.register("conflict", |_, _| {
                Err(HandlerError::conflict("already running"))
            });
            registry.register("boom", |_, _| panic!("handler exploded"));
            registry.register("echo", |params, _| Ok(params.clone()));
        });
        // panic 被 catch_unwind 吸收后映射 500，循环继续服务第 4 个请求。
        assert_eq!(result.unwrap(), Shutdown::Graceful);
        let out = lines(&out);
        assert_eq!(out.len(), 4);
        assert_eq!(error_code(out[0]), CODE_BAD_REQUEST);
        assert_eq!(error_code(out[1]), crate::frame::CODE_CONFLICT);
        assert_eq!(error_code(out[2]), CODE_INTERNAL);
        assert!(out[2].contains("handler panicked"));
        assert_eq!(out[3], r#"{"v":1,"id":4,"ok":true,"result":{}}"#);
    }

    #[test]
    fn malformed_line_emits_proto_error_and_keeps_serving() {
        let input = b"not json at all\n{\"v\":1,\"id\":9,\"method\":\"echo\",\"params\":{}}\n";
        let (result, out) = serve_input(input, |registry| {
            registry.register("echo", |params, _| Ok(params.clone()));
        });
        assert_eq!(result.unwrap(), Shutdown::Graceful);
        let out = lines(&out);
        let first: Value = serde_json::from_str(out[0]).expect("proto_error json");
        assert_eq!(first["event"], json!(EVENT_PROTO_ERROR));
        assert_eq!(first["data"]["raw"], json!("not json at all"));
        assert!(first["data"]["reason"]
            .as_str()
            .expect("reason")
            .starts_with("invalid json"));
        assert_eq!(out[1], r#"{"v":1,"id":9,"ok":true,"result":{}}"#);
    }

    #[test]
    fn proto_error_raw_is_truncated_to_512_bytes_at_char_boundary() {
        assert_eq!(truncate_raw("short"), "short");

        let long_ascii = "x".repeat(RAW_TRUNCATE_BYTES + 84);
        assert_eq!(truncate_raw(&long_ascii).len(), RAW_TRUNCATE_BYTES);

        // 3 字节字符：截断必须向前对齐，保证结果仍是合法 UTF-8。
        let long_cjk = "你".repeat(200); // 600 字节
        let cut = truncate_raw(&long_cjk);
        assert!(cut.len() <= RAW_TRUNCATE_BYTES);
        assert_eq!(cut.len() % 3, 0);
        assert!(cut.chars().all(|c| c == '你'));

        // 走 serve 路径验证 raw 字段同样截断且循环不中断。
        let mut input = long_cjk.into_bytes();
        input.push(b'\n');
        input.extend_from_slice(br#"{"v":1,"id":5,"method":"echo","params":{}}"#);
        input.push(b'\n');
        let (result, out) = serve_input(&input, |registry| {
            registry.register("echo", |params, _| Ok(params.clone()));
        });
        assert_eq!(result.unwrap(), Shutdown::Graceful);
        let out = lines(&out);
        let first: Value = serde_json::from_str(out[0]).expect("proto_error json");
        let raw = first["data"]["raw"].as_str().expect("raw");
        assert_eq!(raw.len(), 510);
        assert_eq!(out.len(), 2, "截断后仍必须继续服务");
    }

    #[test]
    fn non_utf8_line_becomes_proto_error_without_panic() {
        let mut input = vec![0xff, 0xfe, b'\n'];
        input.extend_from_slice(br#"{"v":1,"id":1,"method":"echo","params":{}}"#);
        input.push(b'\n');
        let (result, out) = serve_input(&input, |registry| {
            registry.register("echo", |params, _| Ok(params.clone()));
        });
        assert_eq!(result.unwrap(), Shutdown::Graceful);
        let out = lines(&out);
        let first: Value = serde_json::from_str(out[0]).expect("proto_error json");
        assert_eq!(first["event"], json!(EVENT_PROTO_ERROR));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn version_mismatch_fails_closed_with_error_frame() {
        let input = br#"{"v":2,"id":7,"method":"watch","params":{}}
{"v":1,"id":8,"method":"echo","params":{}}
"#;
        let (result, out) = serve_input(input, |registry| {
            registry.register("echo", |params, _| Ok(params.clone()));
        });
        let shutdown = result.expect("serve ok");
        assert_eq!(shutdown, Shutdown::ProtocolError);
        assert_eq!(shutdown.exit_code(), 1);
        let out = lines(&out);
        assert_eq!(out.len(), 1, "版本不匹配后不得再处理后续输入");
        let value: Value = serde_json::from_str(out[0]).expect("error json");
        assert_eq!(value["ok"], json!(false));
        assert_eq!(value["error"]["code"], json!(CODE_BAD_REQUEST));
        assert!(value["error"]["message"]
            .as_str()
            .expect("message")
            .starts_with(VERSION_MISMATCH_PREFIX));
        assert_eq!(value["id"], json!(7), "id 必须回显");
    }

    #[test]
    fn shutdown_is_success_then_graceful_and_idempotent() {
        // 两条 shutdown：只回第一条成功响应即退出（进程即将 exit(0)，与 TS 语义一致）。
        let input = br#"{"v":1,"id":1,"method":"shutdown","params":{}}
{"v":1,"id":2,"method":"shutdown","params":{}}
"#;
        let (result, out) = serve_input(input, |_| {});
        let shutdown = result.expect("serve ok");
        assert_eq!(shutdown, Shutdown::Graceful);
        assert_eq!(shutdown.exit_code(), 0);
        assert_eq!(
            lines(&out),
            vec![r#"{"v":1,"id":1,"ok":true,"result":{}}"#],
            "shutdown 响应必须是成功帧且只有一条",
        );

        // 幂等：重复调用 serve 处理 shutdown 均返回 Graceful + 成功响应。
        for _ in 0..2 {
            let (again, out) = serve_input(br#"{"v":1,"id":42,"method":"shutdown"}"#, |_| {});
            assert_eq!(again.unwrap(), Shutdown::Graceful);
            assert!(lines(&out)[0].contains(r#""ok":true"#));
        }
    }

    #[test]
    fn client_sent_event_or_response_is_proto_error_and_loop_survives() {
        let input = br#"{"v":1,"event":"bogus","data":{}}
{"v":1,"id":3,"ok":true,"result":{}}
{"v":1,"id":4,"method":"echo","params":{}}
"#;
        let (result, out) = serve_input(input, |registry| {
            registry.register("echo", |params, _| Ok(params.clone()));
        });
        assert_eq!(result.unwrap(), Shutdown::Graceful);
        let out = lines(&out);
        assert_eq!(out.len(), 3);
        for line in &out[..2] {
            let value: Value = serde_json::from_str(line).expect("proto_error json");
            assert_eq!(value["event"], json!(EVENT_PROTO_ERROR));
            assert_eq!(
                value["data"]["reason"],
                json!("unexpected response/event frame from client")
            );
        }
        assert_eq!(out[2], r#"{"v":1,"id":4,"ok":true,"result":{}}"#);
    }

    #[test]
    fn handler_emitted_event_interleaves_before_response() {
        let input = br#"{"v":1,"id":1,"method":"watch","params":{}}
{"v":1,"id":2,"method":"ping","params":{}}
"#;
        let (result, out) = serve_input(input, |registry| {
            registry.register("watch", |_params, emitter| {
                // io 错误转 500：handler 的失败类型是 HandlerError（测试 writer 不会失败）。
                emitter
                    .emit(
                        "fs_event",
                        json!({"watchId": "0", "path": "a", "kind": "create"}),
                    )
                    .map_err(|err| HandlerError::internal(err.to_string()))?;
                Ok(json!({"watchId": "0"}))
            });
            registry.register("ping", |_, _| Ok(json!("pong")));
        });
        assert_eq!(result.unwrap(), Shutdown::Graceful);
        assert_eq!(
            lines(&out),
            vec![
                r#"{"v":1,"event":"fs_event","data":{"watchId":"0","path":"a","kind":"create"}}"#,
                r#"{"v":1,"id":1,"ok":true,"result":{"watchId":"0"}}"#,
                r#"{"v":1,"id":2,"ok":true,"result":"pong"}"#,
            ],
            "事件必须先于对应响应写出，且后续响应不受影响",
        );
    }

    #[test]
    fn handler_spawned_thread_can_emit_through_emitter() {
        let input = br#"{"v":1,"id":1,"method":"scan","params":{}}
"#;
        let (result, out) = serve_input(input, |registry| {
            registry.register("scan", |_params, emitter| {
                let emitter = emitter.clone();
                // 汇合通道保证事件线程写完后 handler 才返回 → 输出顺序确定。
                let (done_tx, done_rx) = mpsc::sync_channel(0);
                std::thread::spawn(move || {
                    emitter
                        .emit("scan_done", json!({"scanId": "s", "count": 0}))
                        .expect("emit from thread");
                    let _ = done_tx.send(());
                });
                done_rx.recv().expect("emit thread finished");
                Ok(json!({"scanId": "s"}))
            });
        });
        assert_eq!(result.unwrap(), Shutdown::Graceful);
        assert_eq!(
            lines(&out),
            vec![
                r#"{"v":1,"event":"scan_done","data":{"scanId":"s","count":0}}"#,
                r#"{"v":1,"id":1,"ok":true,"result":{"scanId":"s"}}"#,
            ],
        );
    }

    #[test]
    fn parse_error_display_is_used_as_proto_reason() {
        // reason 依赖 ParseError 的 Display 覆盖：proto_error 的可读诊断。
        let err = Frame::parse("").expect_err("empty line must fail");
        assert!(matches!(err, ParseError::InvalidJson(_)));
        assert!(err.to_string().starts_with("invalid json"));
        let err = Frame::parse(r#"{"v":1}"#).expect_err("unknown shape");
        assert_eq!(err.to_string(), "unrecognized frame shape");
    }
}
