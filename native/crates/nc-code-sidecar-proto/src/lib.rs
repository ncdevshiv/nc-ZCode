//! sidecar stdio 协议（NDJSON 单行帧）—— 全部 S1 island 共享的传输层。
//!
//! Spec: `specs/rust-native-crates.md` §Sidecar stdio protocol（协议版本 `1`）。
//!
//! - [`Frame`]：request / response / event 三类帧的编码与类型化解析；
//! - [`serve`]：`Read+Write` 泛型的行循环（方法注册表 + 内建 `shutdown` + fail-closed 版本检查）;
//! - [`Emitter`]：`Arc<Mutex<W>>` 写出句柄，handler 可克隆进其它线程异步发事件；
//! - [`params`]：参数校验辅助，统一映射 400。
//!
//! 错误码约定（spec 规则 5）：400 协议/参数、404 未知方法、409 状态冲突、500 内部错误。
//! 客户端输入永远不触发 panic：非法行 → `proto_error` 事件后继续服务（spec 规则 2）。

mod frame;
mod params;
mod serve;

pub use frame::{
    ErrorBody, Frame, ParseError, CODE_BAD_REQUEST, CODE_CONFLICT, CODE_INTERNAL, CODE_NOT_FOUND,
    SIDECAR_PROTOCOL_VERSION, VERSION_MISMATCH_PREFIX,
};
pub use params::{
    bad_request, optional_bool, optional_u32, optional_u64, require_object, require_string,
    require_string_array,
};
pub use serve::{
    serve, truncate_raw, Emitter, HandlerError, HandlerFn, Registry, Shutdown, EVENT_PROTO_ERROR,
    METHOD_SHUTDOWN, RAW_TRUNCATE_BYTES,
};
