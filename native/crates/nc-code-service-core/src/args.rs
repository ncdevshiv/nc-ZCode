//! Positional argument decoding.
//!
//! The frontend's `ProxyChannel.toService` sends `[arg0, arg1, …]`; every service
//! method receives that array. These helpers turn wire values into JSON/typed values
//! and produce `ZodError`-shaped failures for bad input.

use nc_code_rpc::serialization::nested_uint8array_bytes;
use nc_code_rpc::WireValue;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::ServiceError;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServiceArgs {
    values: Vec<WireValue>,
}

impl ServiceArgs {
    pub fn new(values: Vec<WireValue>) -> Self {
        Self { values }
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&WireValue> {
        self.values.get(index)
    }

    pub fn raw(&self) -> &[WireValue] {
        &self.values
    }

    pub fn json(&self, index: usize) -> Result<Option<Value>, ServiceError> {
        match self.get(index) {
            None | Some(WireValue::Undefined) => Ok(None),
            Some(value) => wire_to_json(value, index).map(Some),
        }
    }

    pub fn required_json(&self, index: usize, label: &str) -> Result<Value, ServiceError> {
        self.json(index)?.ok_or_else(|| {
            ServiceError::invalid_params(format!("args[{index}] ({label}) is required"))
        })
    }

    pub fn string(&self, index: usize) -> Result<Option<String>, ServiceError> {
        match self.get(index) {
            None | Some(WireValue::Undefined) => Ok(None),
            Some(WireValue::String(value)) => Ok(Some(value.clone())),
            Some(other) => Err(expected(index, "string", other)),
        }
    }

    pub fn required_string(&self, index: usize, label: &str) -> Result<String, ServiceError> {
        self.string(index)?.ok_or_else(|| {
            ServiceError::invalid_params(format!("args[{index}] ({label}) is required"))
        })
    }

    pub fn bool(&self, index: usize) -> Result<Option<bool>, ServiceError> {
        match self.get(index) {
            None | Some(WireValue::Undefined) => Ok(None),
            Some(WireValue::Json(Value::Bool(value))) => Ok(Some(*value)),
            Some(other) => Err(expected(index, "boolean", other)),
        }
    }

    pub fn integer(&self, index: usize) -> Result<Option<i64>, ServiceError> {
        match self.get(index) {
            None | Some(WireValue::Undefined) => Ok(None),
            Some(WireValue::Int(value)) => Ok(Some(i64::from(*value))),
            Some(WireValue::Json(Value::Number(number))) => {
                number.as_i64().map(Some).ok_or_else(|| {
                    ServiceError::invalid_params(format!("args[{index}] must be an integer"))
                })
            }
            Some(other) => Err(expected(index, "integer", other)),
        }
    }

    pub fn typed<T: DeserializeOwned>(&self, index: usize) -> Result<Option<T>, ServiceError> {
        let Some(value) = self.json(index)? else {
            return Ok(None);
        };
        serde_json::from_value(value)
            .map(Some)
            .map_err(|error| ServiceError::invalid_params(format!("args[{index}]: {error}")))
    }

    pub fn required_typed<T: DeserializeOwned>(
        &self,
        index: usize,
        label: &str,
    ) -> Result<T, ServiceError> {
        self.typed(index)?.ok_or_else(|| {
            ServiceError::invalid_params(format!("args[{index}] ({label}) is required"))
        })
    }

    pub fn json_array(&self, index: usize) -> Result<Option<Vec<Value>>, ServiceError> {
        match self.get(index) {
            None | Some(WireValue::Undefined) => Ok(None),
            Some(WireValue::Array(items)) => items
                .iter()
                .enumerate()
                .map(|(position, item)| wire_to_json(item, position))
                .collect::<Result<Vec<_>, _>>()
                .map(Some),
            Some(other) => Err(expected(index, "array", other)),
        }
    }

    /// Binary argument: accepts `Uint8Array`/VSBuffer tags and the nested-bytes marker.
    pub fn bytes(&self, index: usize) -> Result<Option<Vec<u8>>, ServiceError> {
        match self.get(index) {
            None | Some(WireValue::Undefined) => Ok(None),
            Some(WireValue::Bytes(bytes)) | Some(WireValue::VsBuffer(bytes)) => {
                Ok(Some(bytes.clone()))
            }
            Some(WireValue::Json(value)) => nested_uint8array_bytes(value)
                .map(Some)
                .ok_or_else(|| expected(index, "binary", &WireValue::Json(value.clone()))),
            Some(other) => Err(expected(index, "binary", other)),
        }
    }
}

pub fn wire_to_json(value: &WireValue, index: usize) -> Result<Value, ServiceError> {
    match value {
        WireValue::Undefined => Ok(Value::Null),
        WireValue::String(text) => Ok(Value::String(text.clone())),
        WireValue::Int(number) => Ok(Value::from(*number)),
        WireValue::Json(json) => Ok(json.clone()),
        WireValue::Array(items) => items
            .iter()
            .enumerate()
            .map(|(position, item)| wire_to_json(item, position))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        WireValue::Bytes(_) | WireValue::VsBuffer(_) => Err(ServiceError::invalid_params(format!(
            "args[{index}] is binary; use ServiceArgs::bytes"
        ))),
    }
}

fn expected(index: usize, expected: &str, actual: &WireValue) -> ServiceError {
    ServiceError::invalid_params(format!(
        "args[{index}] must be {expected}, got {}",
        wire_kind(actual)
    ))
}

fn wire_kind(value: &WireValue) -> &'static str {
    match value {
        WireValue::Undefined => "undefined",
        WireValue::String(_) => "string",
        WireValue::Bytes(_) => "Uint8Array",
        WireValue::VsBuffer(_) => "VSBuffer",
        WireValue::Int(_) => "integer",
        WireValue::Array(_) => "array",
        WireValue::Json(inner) => match inner {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_json_strings_and_integers() {
        let args = ServiceArgs::new(vec![
            WireValue::Json(json!({"path": "G:/a"})),
            WireValue::String("x".into()),
            WireValue::Int(7),
            WireValue::Undefined,
        ]);
        assert_eq!(args.json(0).unwrap(), Some(json!({"path": "G:/a"})));
        assert_eq!(args.string(1).unwrap(), Some("x".to_string()));
        assert_eq!(args.integer(2).unwrap(), Some(7));
        assert_eq!(args.json(3).unwrap(), None);
        assert_eq!(args.json(9).unwrap(), None);
    }

    #[test]
    fn typed_decoding_reports_invalid_params() {
        #[derive(serde::Deserialize, PartialEq, Debug)]
        struct Params {
            path: String,
        }
        let args = ServiceArgs::new(vec![WireValue::Json(json!({"path": "G:/a"}))]);
        assert_eq!(
            args.required_typed::<Params>(0, "params").unwrap(),
            Params {
                path: "G:/a".to_string()
            }
        );
        let bad = ServiceArgs::new(vec![WireValue::Json(json!({"path": 1}))]);
        let error = bad.required_typed::<Params>(0, "params").unwrap_err();
        assert_eq!(error.name, "ZodError");
    }

    #[test]
    fn binary_arguments_accept_buffer_tags_and_nested_markers() {
        let marker = nc_code_rpc::serialization::nested_uint8array_json(&[1, 2]);
        let args = ServiceArgs::new(vec![
            WireValue::Bytes(vec![0, 255]),
            WireValue::VsBuffer(vec![3]),
            WireValue::Json(marker),
        ]);
        assert_eq!(args.bytes(0).unwrap(), Some(vec![0, 255]));
        assert_eq!(args.bytes(1).unwrap(), Some(vec![3]));
        assert_eq!(args.bytes(2).unwrap(), Some(vec![1, 2]));
        assert!(args.json(0).is_err());
    }
}
