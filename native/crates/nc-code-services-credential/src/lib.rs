//! `credential` channel — key-value credential storage.

/// Handoff skeleton (pure data/types): serde persistence structs, port trait, Send/Sync markers.
pub mod handoff;

pub mod cipher;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::{ServiceArgs, ServiceError};
use serde_json::{json, Value};

pub struct CredentialService {
    path: PathBuf,
    cache: Mutex<Option<HashMap<String, String>>>,
}

impl CredentialService {
    pub fn new() -> Arc<Self> {
        let path = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir())
            .join(".zcode-credentials.json");
        Arc::new(Self {
            path,
            cache: Mutex::new(None),
        })
    }

    fn read_all(&self) -> Result<HashMap<String, String>, ServiceError> {
        let mut cache = self.cache.lock().unwrap();
        if let Some(ref data) = *cache {
            return Ok(data.clone());
        }
        let data = match std::fs::read_to_string(&self.path) {
            Ok(s) => serde_json::from_str::<HashMap<String, String>>(&s).unwrap_or_default(),
            Err(_) => HashMap::new(),
        };
        *cache = Some(data.clone());
        Ok(data)
    }

    fn write_all(&self, data: &HashMap<String, String>) -> Result<(), ServiceError> {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let content =
            serde_json::to_string_pretty(data).map_err(|e| ServiceError::message(e.to_string()))?;
        std::fs::write(&self.path, content).map_err(|e| ServiceError::message(e.to_string()))?;
        let mut cache = self.cache.lock().unwrap();
        *cache = Some(data.clone());
        Ok(())
    }
}

impl Default for CredentialService {
    fn default() -> Self {
        let path = std::env::temp_dir().join(".zcode-credentials.json");
        Self {
            path,
            cache: Mutex::new(None),
        }
    }
}

#[async_trait]
impl Channel for CredentialService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        match command {
            "load" => {
                let key = args
                    .required_string(0, "key")
                    .map_err(ChannelFailure::from)?;
                let data = self.read_all().map_err(ChannelFailure::from)?;
                let value = data.get(&key).cloned();
                Ok(WireValue::Json(
                    value.map(Value::String).unwrap_or(Value::Null),
                ))
            }
            "save" => {
                let key = args
                    .required_string(0, "key")
                    .map_err(ChannelFailure::from)?;
                let value = args
                    .required_string(1, "value")
                    .map_err(ChannelFailure::from)?;
                let mut data = self.read_all().map_err(ChannelFailure::from)?;
                data.insert(key, value);
                self.write_all(&data).map_err(ChannelFailure::from)?;
                Ok(WireValue::Json(json!(null)))
            }
            "delete" => {
                let key = args
                    .required_string(0, "key")
                    .map_err(ChannelFailure::from)?;
                let mut data = self.read_all().map_err(ChannelFailure::from)?;
                data.remove(&key);
                self.write_all(&data).map_err(ChannelFailure::from)?;
                Ok(WireValue::Json(json!(null)))
            }
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}
