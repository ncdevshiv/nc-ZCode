//! NDJSON 帧的编码与解析 —— spec: `specs/rust-native-crates.md` §Sidecar stdio protocol。
//!
//! 帧形状（key 顺序是线协议的一部分，`serde_json` `preserve_order` 保证按字面量顺序编码）：
//!
//! ```jsonc
//! {"v":1,"id":7,"method":"watch","params":{...}}              // request
//! {"v":1,"id":7,"ok":true,"result":{...}}                     // response 成功
//! {"v":1,"id":7,"ok":false,"error":{"code":404,"message":""}} // response 失败
//! {"v":1,"event":"fs_event","data":{...}}                     // 异步事件
//! ```

use std::fmt;

use serde_json::{json, Value};

/// sidecar 协议版本（帧字段 `v`）。
pub const SIDECAR_PROTOCOL_VERSION: u32 = 1;

/// 400 —— 协议/参数错误（含版本不匹配）。
pub const CODE_BAD_REQUEST: u16 = 400;
/// 404 —— 未知方法 / 未知 id。
pub const CODE_NOT_FOUND: u16 = 404;
/// 409 —— 状态冲突（如重复 scanId）。
pub const CODE_CONFLICT: u16 = 409;
/// 500 —— sidecar 内部错误。
pub const CODE_INTERNAL: u16 = 500;

/// 版本不匹配错误消息前缀（spec 要求携带 `PROTOCOL_VERSION_MISMATCH`）。
pub const VERSION_MISMATCH_PREFIX: &str = "PROTOCOL_VERSION_MISMATCH";

/// 响应错误体 `{"code":…,"message":…}`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorBody {
    pub code: u16,
    pub message: String,
}

/// 协议帧。`id` 用 `Value` 承载以便原样回显客户端的任意 id。
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Request {
        v: u32,
        id: Value,
        method: String,
        params: Value,
    },
    ResponseOk {
        v: u32,
        id: Value,
        result: Value,
    },
    ResponseErr {
        v: u32,
        id: Value,
        error: ErrorBody,
    },
    Event {
        v: u32,
        event: String,
        data: Value,
    },
}

impl Frame {
    /// 构造当前协议版本的 request。
    pub fn request(id: Value, method: impl Into<String>, params: Value) -> Self {
        Frame::Request {
            v: SIDECAR_PROTOCOL_VERSION,
            id,
            method: method.into(),
            params,
        }
    }

    /// 构造成功响应。
    pub fn response_ok(id: Value, result: Value) -> Self {
        Frame::ResponseOk {
            v: SIDECAR_PROTOCOL_VERSION,
            id,
            result,
        }
    }

    /// 构造失败响应。
    pub fn response_err(id: Value, code: u16, message: impl Into<String>) -> Self {
        Frame::ResponseErr {
            v: SIDECAR_PROTOCOL_VERSION,
            id,
            error: ErrorBody {
                code,
                message: message.into(),
            },
        }
    }

    /// 构造事件帧。
    pub fn event(event: impl Into<String>, data: Value) -> Self {
        Frame::Event {
            v: SIDECAR_PROTOCOL_VERSION,
            event: event.into(),
            data,
        }
    }

    /// 帧携带的协议版本。
    pub fn v(&self) -> u32 {
        match self {
            Frame::Request { v, .. }
            | Frame::ResponseOk { v, .. }
            | Frame::ResponseErr { v, .. }
            | Frame::Event { v, .. } => *v,
        }
    }

    /// 帧携带的关联 id；事件帧没有 id（版本检查时回 null）。
    pub fn id(&self) -> Option<&Value> {
        match self {
            Frame::Request { id, .. }
            | Frame::ResponseOk { id, .. }
            | Frame::ResponseErr { id, .. } => Some(id),
            Frame::Event { .. } => None,
        }
    }

    /// 编码为单行 JSON（不含换行）。
    pub fn encode(&self) -> String {
        match self {
            Frame::Request {
                v,
                id,
                method,
                params,
            } => json!({ "v": v, "id": id, "method": method, "params": params }).to_string(),
            Frame::ResponseOk { v, id, result } => {
                json!({ "v": v, "id": id, "ok": true, "result": result }).to_string()
            }
            Frame::ResponseErr { v, id, error } => json!({
                "v": v,
                "id": id,
                "ok": false,
                "error": { "code": error.code, "message": error.message },
            })
            .to_string(),
            Frame::Event { v, event, data } => {
                json!({ "v": v, "event": event, "data": data }).to_string()
            }
        }
    }

    /// 从单行文本解析帧。只报类型化错误，绝不 panic。
    pub fn parse(line: &str) -> Result<Frame, ParseError> {
        let value: Value = serde_json::from_str(line).map_err(ParseError::InvalidJson)?;
        let obj = value.as_object().ok_or(ParseError::NotAnObject)?;

        let raw_v = obj.get("v").ok_or(ParseError::MissingField("v"))?;
        let v = u32::try_from(raw_v.as_u64().ok_or(ParseError::InvalidFieldType {
            field: "v",
            expected: "u32",
        })?)
        .map_err(|_| ParseError::InvalidFieldType {
            field: "v",
            expected: "u32",
        })?;

        // 判别顺序：method → ok → event（同时携带多个键时以 method 为准）。
        if let Some(method) = obj.get("method") {
            let method = method
                .as_str()
                .ok_or(ParseError::InvalidFieldType {
                    field: "method",
                    expected: "string",
                })?
                .to_string();
            let id = obj
                .get("id")
                .cloned()
                .ok_or(ParseError::MissingField("id"))?;
            // params 允许缺失，按 null 交给 handler 校验（→ 400）。
            let params = obj.get("params").cloned().unwrap_or(Value::Null);
            return Ok(Frame::Request {
                v,
                id,
                method,
                params,
            });
        }

        if let Some(ok) = obj.get("ok") {
            let ok = ok.as_bool().ok_or(ParseError::InvalidFieldType {
                field: "ok",
                expected: "boolean",
            })?;
            let id = obj
                .get("id")
                .cloned()
                .ok_or(ParseError::MissingField("id"))?;
            if ok {
                let result = obj.get("result").cloned().unwrap_or(Value::Null);
                return Ok(Frame::ResponseOk { v, id, result });
            }
            let error = obj
                .get("error")
                .ok_or(ParseError::MissingField("error"))?
                .as_object()
                .ok_or(ParseError::InvalidFieldType {
                    field: "error",
                    expected: "object",
                })?;
            let code = u16::try_from(
                error
                    .get("code")
                    .ok_or(ParseError::MissingField("error.code"))?
                    .as_u64()
                    .ok_or(ParseError::InvalidFieldType {
                        field: "error.code",
                        expected: "u16",
                    })?,
            )
            .map_err(|_| ParseError::InvalidFieldType {
                field: "error.code",
                expected: "u16",
            })?;
            let message = error
                .get("message")
                .ok_or(ParseError::MissingField("error.message"))?
                .as_str()
                .ok_or(ParseError::InvalidFieldType {
                    field: "error.message",
                    expected: "string",
                })?
                .to_string();
            return Ok(Frame::ResponseErr {
                v,
                id,
                error: ErrorBody { code, message },
            });
        }

        if let Some(event) = obj.get("event") {
            let event = event
                .as_str()
                .ok_or(ParseError::InvalidFieldType {
                    field: "event",
                    expected: "string",
                })?
                .to_string();
            let data = obj.get("data").cloned().unwrap_or(Value::Null);
            return Ok(Frame::Event { v, event, data });
        }

        Err(ParseError::UnknownShape)
    }
}

/// 帧解析的类型化错误。
#[derive(Debug)]
pub enum ParseError {
    InvalidJson(serde_json::Error),
    NotAnObject,
    UnknownShape,
    MissingField(&'static str),
    InvalidFieldType {
        field: &'static str,
        expected: &'static str,
    },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::InvalidJson(err) => write!(f, "invalid json: {err}"),
            ParseError::NotAnObject => write!(f, "frame must be a json object"),
            ParseError::UnknownShape => write!(f, "unrecognized frame shape"),
            ParseError::MissingField(field) => write!(f, "missing field: {field}"),
            ParseError::InvalidFieldType { field, expected } => {
                write!(f, "field {field} must be {expected}")
            }
        }
    }
}

impl std::error::Error for ParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip_is_byte_exact() {
        let line = r#"{"v":1,"id":7,"method":"watch","params":{}}"#;
        let frame = Frame::parse(line).expect("parse request");
        assert_eq!(
            frame,
            Frame::Request {
                v: 1,
                id: json!(7),
                method: "watch".into(),
                params: json!({}),
            }
        );
        assert_eq!(frame.encode(), line, "重编码必须与规范示例字节一致");
    }

    #[test]
    fn response_frames_roundtrip_byte_exact() {
        let ok = r#"{"v":1,"id":7,"ok":true,"result":{}}"#;
        let parsed = Frame::parse(ok).expect("parse ok");
        assert_eq!(parsed, Frame::response_ok(json!(7), json!({})));
        assert_eq!(parsed.encode(), ok);

        let err =
            r#"{"v":1,"id":7,"ok":false,"error":{"code":404,"message":"unknown method: nope"}}"#;
        let parsed = Frame::parse(err).expect("parse err");
        assert_eq!(
            parsed,
            Frame::response_err(json!(7), 404, "unknown method: nope")
        );
        assert_eq!(parsed.encode(), err);
    }

    #[test]
    fn event_frame_roundtrip_byte_exact() {
        let line = r#"{"v":1,"event":"ready","data":{"sidecar":"zcode-watch","proto":1}}"#;
        let frame = Frame::parse(line).expect("parse event");
        assert_eq!(
            frame,
            Frame::Event {
                v: 1,
                event: "ready".into(),
                data: json!({"sidecar": "zcode-watch", "proto": 1}),
            }
        );
        assert_eq!(frame.encode(), line);
        assert_eq!(frame.id(), None);
    }

    #[test]
    fn string_and_unicode_ids_echo_verbatim() {
        let line = r#"{"v":1,"id":"req-α","method":"list","params":null}"#;
        let frame = Frame::parse(line).expect("parse");
        assert_eq!(frame.id(), Some(&json!("req-α")));
        assert_eq!(frame.encode(), line);
    }

    #[test]
    fn missing_params_defaults_to_null() {
        let frame = Frame::parse(r#"{"v":1,"id":1,"method":"m"}"#).expect("parse");
        match frame {
            Frame::Request { params, .. } => assert_eq!(params, Value::Null),
            other => panic!("expected request, got {other:?}"),
        }
    }

    /// 用例表条目：(输入行, 错误分类谓词) —— 别名压平 type_complexity。
    type ErrorCase = (&'static str, fn(&ParseError) -> bool);

    #[test]
    fn malformed_inputs_return_typed_errors() {
        let cases: Vec<ErrorCase> = vec![
            ("not json", |e| matches!(e, ParseError::InvalidJson(_))),
            ("[1,2]", |e| matches!(e, ParseError::NotAnObject)),
            ("{}", |e| matches!(e, ParseError::MissingField("v"))),
            (r#"{"v":"1","id":1,"method":"m"}"#, |e| {
                matches!(e, ParseError::InvalidFieldType { field: "v", .. })
            }),
            (r#"{"v":1,"method":"m","params":{}}"#, |e| {
                matches!(e, ParseError::MissingField("id"))
            }),
            (r#"{"v":1,"id":1,"method":7,"params":{}}"#, |e| {
                matches!(
                    e,
                    ParseError::InvalidFieldType {
                        field: "method",
                        ..
                    }
                )
            }),
            (r#"{"v":1,"id":1,"ok":"yes"}"#, |e| {
                matches!(e, ParseError::InvalidFieldType { field: "ok", .. })
            }),
            (r#"{"v":1,"id":1,"ok":false}"#, |e| {
                matches!(e, ParseError::MissingField("error"))
            }),
            (
                r#"{"v":1,"id":1,"ok":false,"error":{"message":"x"}}"#,
                |e| matches!(e, ParseError::MissingField("error.code")),
            ),
            (
                r#"{"v":1,"id":1,"ok":false,"error":{"code":99999,"message":"x"}}"#,
                |e| {
                    matches!(
                        e,
                        ParseError::InvalidFieldType {
                            field: "error.code",
                            ..
                        }
                    )
                },
            ),
            (r#"{"v":1,"foo":1}"#, |e| {
                matches!(e, ParseError::UnknownShape)
            }),
        ];
        for (input, check) in cases {
            let err = Frame::parse(input)
                .err()
                .unwrap_or_else(|| panic!("expected error for {input}"));
            assert!(check(&err), "input {input}: unexpected error kind {err:?}");
            // Display 覆盖所有分支，proto_error 的 reason 依赖它。
            assert!(!err.to_string().is_empty());
        }
    }
}
