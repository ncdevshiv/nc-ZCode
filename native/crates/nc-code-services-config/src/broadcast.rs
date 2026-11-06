//! `broadcast` channel — cross-window state sync + opaque claim leases.
//!
//! Server mode (`parentPort === null` in TS) semantics: messages fan out to local
//! subscribers, and claims live in this process with a 5s reservation TTL. The Tauri
//! app has a single window, but the renderer still uses claims to dedupe startup work.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use nc_code_channels::{
    CancelToken, Channel, ChannelContext, ChannelFailure, EventBus, EventSubscription,
};
use nc_code_rpc::WireValue;
use nc_code_service_core::{ServiceArgs, ServiceError};
use serde_json::{json, Value};

const RESERVATION_TTL_MS: u64 = 5_000;
const CLAIM_RETRY_MS: u64 = 250;
const MAX_LOCAL_CLAIMS: usize = 1_024;

static TOKEN_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimStatus {
    Reserved,
    Committed,
}

#[derive(Debug, Clone)]
struct ClaimRecord {
    token: String,
    status: ClaimStatus,
    expires_at: Option<u64>,
}

pub struct BroadcastService {
    bus: EventBus,
    claims: Mutex<HashMap<String, ClaimRecord>>,
}

impl BroadcastService {
    pub fn new() -> Self {
        Self {
            bus: EventBus::default(),
            claims: Mutex::new(HashMap::new()),
        }
    }

    fn send(&self, message: Value) -> Result<(), ServiceError> {
        let Some(object) = message.as_object() else {
            return Err(ServiceError::invalid_params(
                "broadcast message must be an object",
            ));
        };
        match object.get("channel").and_then(Value::as_str) {
            Some(channel) if !channel.trim().is_empty() => {}
            _ => {
                return Err(ServiceError::invalid_params(
                    "broadcast message.channel must be a non-empty string",
                ))
            }
        }
        if let Some(source) = object.get("sourceWindowId") {
            if !source.is_null() && !source.is_u64() && !source.is_i64() {
                return Err(ServiceError::invalid_params(
                    "broadcast message.sourceWindowId must be a number",
                ));
            }
        }
        self.bus.send(WireValue::Json(message));
        Ok(())
    }

    fn acquire_claim(&self, key: &str) -> Value {
        let normalized = key.trim();
        if normalized.is_empty() {
            return json!({ "status": "unavailable" });
        }
        let now = now_ms();
        let mut claims = self.lock();
        prune(&mut claims, now);
        if let Some(existing) = claims.get(normalized) {
            if existing.status == ClaimStatus::Committed {
                return json!({ "status": "committed" });
            }
            let retry_after_ms = existing
                .expires_at
                .map(|expires_at| expires_at.saturating_sub(now).min(CLAIM_RETRY_MS))
                .unwrap_or(0);
            return json!({ "status": "busy", "retryAfterMs": retry_after_ms });
        }
        let token = format!(
            "local-{now}-{}",
            TOKEN_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        claims.insert(
            normalized.to_string(),
            ClaimRecord {
                token: token.clone(),
                status: ClaimStatus::Reserved,
                expires_at: Some(now + RESERVATION_TTL_MS),
            },
        );
        enforce_capacity(&mut claims);
        json!({
            "status": "acquired",
            "lease": { "key": normalized, "token": token },
        })
    }

    fn commit_claim(&self, lease: &Value) -> Result<(), ServiceError> {
        let (key, token) = parse_lease(lease)?;
        let mut claims = self.lock();
        prune(&mut claims, now_ms());
        if let Some(current) = claims.get_mut(&key) {
            if current.status == ClaimStatus::Reserved && current.token == token {
                current.status = ClaimStatus::Committed;
                current.expires_at = None;
            }
        }
        Ok(())
    }

    fn release_claim(&self, lease: &Value) -> Result<(), ServiceError> {
        let (key, token) = parse_lease(lease)?;
        let mut claims = self.lock();
        prune(&mut claims, now_ms());
        let matches = claims.get(&key).is_some_and(|current| {
            current.status == ClaimStatus::Reserved && current.token == token
        });
        if matches {
            claims.remove(&key);
        }
        Ok(())
    }

    fn try_claim(&self, key: &str) -> bool {
        let result = self.acquire_claim(key);
        if result["status"] != json!("acquired") {
            return false;
        }
        let lease = result["lease"].clone();
        self.commit_claim(&lease).is_ok()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, ClaimRecord>> {
        self.claims
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for BroadcastService {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Channel for BroadcastService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        match command {
            "send" => {
                let message = args.required_json(0, "message")?;
                self.send(message)?;
                Ok(WireValue::Undefined)
            }
            "acquireClaim" => {
                let key = args.required_string(0, "key")?;
                Ok(WireValue::Json(self.acquire_claim(&key)))
            }
            "commitClaim" => {
                let lease = args.required_json(0, "lease")?;
                self.commit_claim(&lease)?;
                Ok(WireValue::Undefined)
            }
            "releaseClaim" => {
                let lease = args.required_json(0, "lease")?;
                self.release_claim(&lease)?;
                Ok(WireValue::Undefined)
            }
            "tryClaim" => {
                let key = args.required_string(0, "key")?;
                Ok(WireValue::Json(json!(self.try_claim(&key))))
            }
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }

    async fn listen(
        &self,
        _ctx: &ChannelContext,
        event: &str,
        _arg: WireValue,
    ) -> Result<EventSubscription, ChannelFailure> {
        match event {
            "onMessage" => Ok(EventSubscription::from_broadcast(self.bus.subscribe())),
            other => Err(ChannelFailure::event_not_found(other)),
        }
    }
}

fn parse_lease(lease: &Value) -> Result<(String, String), ServiceError> {
    let object = lease
        .as_object()
        .ok_or_else(|| ServiceError::invalid_params("lease must be an object"))?;
    let key = object
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| ServiceError::invalid_params("lease.key must be a string"))?;
    let token = object
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| ServiceError::invalid_params("lease.token must be a string"))?;
    Ok((key.to_string(), token.to_string()))
}

fn prune(claims: &mut HashMap<String, ClaimRecord>, now: u64) {
    claims.retain(|_, claim| {
        claim.status == ClaimStatus::Committed
            || claim.expires_at.is_some_and(|expires_at| expires_at > now)
    });
}

fn enforce_capacity(claims: &mut HashMap<String, ClaimRecord>) {
    while claims.len() > MAX_LOCAL_CLAIMS {
        let victim = claims
            .iter()
            .filter(|(_, claim)| claim.status == ClaimStatus::Reserved)
            .min_by_key(|(_, claim)| claim.expires_at.unwrap_or(u64::MAX))
            .map(|(key, _)| key.clone())
            .or_else(|| claims.keys().next().cloned());
        let Some(victim) = victim else {
            break;
        };
        claims.remove(&victim);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_lifecycle_matches_the_typescript_state_machine() {
        let service = BroadcastService::new();
        let acquired = service.acquire_claim("startup");
        assert_eq!(acquired["status"], json!("acquired"));
        let lease = acquired["lease"].clone();

        let busy = service.acquire_claim("startup");
        assert_eq!(busy["status"], json!("busy"));
        assert!(busy["retryAfterMs"].as_u64().expect("retry") <= CLAIM_RETRY_MS);

        service.commit_claim(&lease).expect("commit");
        assert_eq!(
            service.acquire_claim("startup")["status"],
            json!("committed")
        );
        // release of a committed claim is a no-op
        service.release_claim(&lease).expect("release");
        assert_eq!(
            service.acquire_claim("startup")["status"],
            json!("committed")
        );

        assert_eq!(service.acquire_claim("  ")["status"], json!("unavailable"));
    }

    #[test]
    fn try_claim_is_first_writer_wins() {
        let service = BroadcastService::new();
        assert!(service.try_claim("key"));
        assert!(!service.try_claim("key"));
    }

    #[test]
    fn stale_reservations_expire() {
        let service = BroadcastService::new();
        let acquired = service.acquire_claim("key");
        let lease = acquired["lease"].clone();
        {
            let mut claims = service.lock();
            let key = lease["key"].as_str().expect("key");
            if let Some(record) = claims.get_mut(key) {
                record.expires_at = Some(0);
            }
        }
        assert_eq!(service.acquire_claim("key")["status"], json!("acquired"));
    }
}
