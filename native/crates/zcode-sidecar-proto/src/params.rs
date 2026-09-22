//! 请求 params 的校验辅助：统一把参数问题映射为 400（spec 规则 5：400 协议/参数）。

use serde_json::{Map, Value};

use crate::frame::ErrorBody;
use crate::serve::HandlerError;

/// params 必须是 JSON object，否则 400。
pub fn require_object(params: &Value) -> Result<&Map<String, Value>, HandlerError> {
    params
        .as_object()
        .ok_or_else(|| bad_request("params must be an object"))
}

/// 取非空字符串字段；缺失或类型不符 → 400。
pub fn require_string(obj: &Map<String, Value>, key: &str) -> Result<String, HandlerError> {
    let value = obj
        .get(key)
        .ok_or_else(|| bad_request(format!("params.{key} is required")))?;
    let text = value
        .as_str()
        .ok_or_else(|| bad_request(format!("params.{key} must be a string")))?;
    if text.is_empty() {
        return Err(bad_request(format!("params.{key} must not be empty")));
    }
    Ok(text.to_string())
}

/// 取非空字符串数组字段；缺失或类型不符 → 400。
pub fn require_string_array(
    obj: &Map<String, Value>,
    key: &str,
) -> Result<Vec<String>, HandlerError> {
    let value = obj
        .get(key)
        .ok_or_else(|| bad_request(format!("params.{key} is required")))?;
    let items = value
        .as_array()
        .ok_or_else(|| bad_request(format!("params.{key} must be an array")))?;
    if items.is_empty() {
        return Err(bad_request(format!("params.{key} must not be empty")));
    }
    items
        .iter()
        .map(|item| {
            item.as_str()
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .ok_or_else(|| bad_request(format!("params.{key} must contain non-empty strings")))
        })
        .collect()
}

/// 可选 bool 字段；缺失取默认值，类型不符 → 400。
pub fn optional_bool(
    obj: &Map<String, Value>,
    key: &str,
    default: bool,
) -> Result<bool, HandlerError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| bad_request(format!("params.{key} must be a boolean"))),
    }
}

/// 可选 u64（非负整数）字段；缺失取默认值，浮点/类型不符 → 400。
pub fn optional_u64(
    obj: &Map<String, Value>,
    key: &str,
    default: u64,
) -> Result<u64, HandlerError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .ok_or_else(|| bad_request(format!("params.{key} must be a non-negative integer"))),
    }
}

/// 可选 u32 字段（如 maxDepth），超出范围 → 400。
pub fn optional_u32(
    obj: &Map<String, Value>,
    key: &str,
    default: Option<u32>,
) -> Result<Option<u32>, HandlerError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => {
            let raw = value.as_u64().ok_or_else(|| {
                bad_request(format!("params.{key} must be a non-negative integer"))
            })?;
            u32::try_from(raw)
                .map(Some)
                .map_err(|_| bad_request(format!("params.{key} must be a non-negative integer")))
        }
    }
}

/// 构造 400 HandlerError。
pub fn bad_request(message: impl Into<String>) -> HandlerError {
    HandlerError {
        body: ErrorBody {
            code: crate::frame::CODE_BAD_REQUEST,
            message: message.into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::frame::CODE_BAD_REQUEST;

    #[test]
    fn require_object_rejects_non_object_params() {
        let err = require_object(&json!(null)).expect_err("null params must fail");
        assert_eq!(err.body.code, CODE_BAD_REQUEST);
        assert!(require_object(&json!({"a":1})).is_ok());
    }

    #[test]
    fn require_string_covers_missing_wrong_type_and_empty() {
        let obj = json!({"path": 7});
        assert_eq!(
            require_string(obj.as_object().unwrap(), "path")
                .unwrap_err()
                .body
                .code,
            CODE_BAD_REQUEST
        );
        let obj = json!({"path": ""});
        assert!(require_string(obj.as_object().unwrap(), "path").is_err());
        let obj = json!({});
        assert!(require_string(obj.as_object().unwrap(), "path").is_err());
        let obj = json!({"path": "x"});
        assert_eq!(
            require_string(obj.as_object().unwrap(), "path").unwrap(),
            "x"
        );
    }

    #[test]
    fn optional_helpers_validate_types() {
        let obj = json!({"recursive": "yes", "debounceMs": -1, "maxDepth": 3.5});
        let m = obj.as_object().unwrap();
        assert_eq!(
            optional_bool(m, "recursive", false).unwrap_err().body.code,
            400
        );
        assert_eq!(
            optional_u64(m, "debounceMs", 150).unwrap_err().body.code,
            400
        );
        assert_eq!(
            optional_u32(m, "maxDepth", None).unwrap_err().body.code,
            400
        );

        let empty = json!({});
        let m = empty.as_object().unwrap();
        // 缺失字段 → 返回默认值（recursive 默认 false）。
        assert!(!optional_bool(m, "recursive", false).unwrap());
        assert_eq!(optional_u64(m, "debounceMs", 150).unwrap(), 150);
        assert_eq!(optional_u32(m, "maxDepth", None).unwrap(), None);
    }
}
