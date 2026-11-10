//! `broadcast` channel — cross-window broadcast and claims.
//!
//! Port of `packages/services/src/broadcast/broadcast.ts` / TS `IBroadcastService` contract.
//! - Claims (`acquireClaim`/`commitClaim`/`releaseClaim`/`tryClaim`): reservation tokens with
//!   TTL for reserved leases, committed leases persisted until released/replaced by policy.
//! - `send`: publishes a `BroadcastMessage` to in-process listeners (EventBus).
//! - `listen(onMessage)`: subscribes to broadcast messages.

/// Handoff skeleton (pure data/types): serde persistence structs, port trait, Send/Sync markers.
pub mod handoff;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nc_code_channels::{
    CancelToken, Channel, ChannelContext, ChannelFailure, EventBus, EventSubscription,
};
use nc_code_rpc::WireValue;
use nc_code_service_core::{ServiceArgs, ServiceError};
use serde_json::{json, Value};

const RESERVED_TTL: Duration = Duration::from_secs(5);
const MAX_CLAIMS: usize = 1024;

#[derive(Clone)]
struct Claim {
    token: String,
    status: ClaimStatus,
    expires_at: Option<Instant>,
}

#[derive(Clone, Debug, PartialEq)]
enum ClaimStatus {
    Reserved,
    Committed,
}

#[derive(Clone)]
pub struct BroadcastService {
    claims: Arc<Mutex<HashMap<String, Claim>>>,
    bus: EventBus,
}

impl BroadcastService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            claims: Arc::new(Mutex::new(HashMap::new())),
            bus: EventBus::new(256),
        })
    }

    fn prune_claims(claims: &mut HashMap<String, Claim>) {
        let now = Instant::now();
        claims.retain(|_, claim| {
            if claim.status == ClaimStatus::Reserved {
                if let Some(expires_at) = claim.expires_at {
                    return expires_at > now;
                }
            }
            true
        });
        while claims.len() > MAX_CLAIMS {
            if let Some(first_key) = claims.keys().next().cloned() {
                claims.remove(&first_key);
            } else {
                break;
            }
        }
    }

    fn parse_key_arg(args: &ServiceArgs) -> Result<String, ServiceError> {
        // Accept string arg or {key: ...} / {channel,payload,...} misuse guard not needed; key is string.
        if let Ok(s) = args.required_string(0, "key") {
            return Ok(s.trim().to_string());
        }
        if let Ok(v) = args.required_json(0, "key") {
            if let Some(s) = v.as_str() {
                return Ok(s.trim().to_string());
            }
            if let Some(s) = v.get("key").and_then(|x| x.as_str()) {
                return Ok(s.trim().to_string());
            }
        }
        Err(ServiceError::invalid_params("key is required"))
    }

    fn parse_lease_arg(args: &ServiceArgs) -> Result<(String, String), ServiceError> {
        let params = args.required_json(0, "lease")?;
        let key = params
            .get("key")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let token = params
            .get("token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        Ok((key, token))
    }

    fn send(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let message = args.required_json(0, "message")?;
        // Publish to in-process listeners. Cross-process relay (parentPort/Main hub) is a
        // host/desktop concern; this service provides the channel contract and local fanout.
        self.bus.send(WireValue::Json(message));
        Ok(WireValue::Json(Value::Null))
    }

    fn acquire_claim(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let key = Self::parse_key_arg(args)?;
        if key.is_empty() {
            return Ok(WireValue::Json(json!({"status": "unavailable"})));
        }
        let mut claims = self.claims.lock().unwrap_or_else(|p| p.into_inner());
        Self::prune_claims(&mut claims);
        if let Some(existing) = claims.get(&key) {
            if existing.status == ClaimStatus::Committed {
                return Ok(WireValue::Json(json!({"status": "committed"})));
            }
            let retry_after_ms = existing
                .expires_at
                .map(|e| {
                    let now = Instant::now();
                    if e > now {
                        e.duration_since(now).as_millis() as u64
                    } else {
                        250
                    }
                })
                .unwrap_or(250);
            return Ok(WireValue::Json(json!({
                "status": "busy",
                "retryAfterMs": retry_after_ms
            })));
        }
        let token = format!(
            "local-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        claims.insert(
            key.clone(),
            Claim {
                token: token.clone(),
                status: ClaimStatus::Reserved,
                expires_at: Some(Instant::now() + RESERVED_TTL),
            },
        );
        Self::prune_claims(&mut claims);
        Ok(WireValue::Json(json!({
            "status": "acquired",
            "lease": { "key": key, "token": token }
        })))
    }

    fn commit_claim(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let (key, token) = Self::parse_lease_arg(args)?;
        if key.is_empty() || token.is_empty() {
            return Ok(WireValue::Json(Value::Null));
        }
        let mut claims = self.claims.lock().unwrap_or_else(|p| p.into_inner());
        Self::prune_claims(&mut claims);
        if let Some(current) = claims.get_mut(&key) {
            if current.status == ClaimStatus::Reserved && current.token == token {
                current.status = ClaimStatus::Committed;
                current.expires_at = None;
            }
        }
        Ok(WireValue::Json(Value::Null))
    }

    fn release_claim(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let (key, token) = Self::parse_lease_arg(args)?;
        if key.is_empty() || token.is_empty() {
            return Ok(WireValue::Json(Value::Null));
        }
        let mut claims = self.claims.lock().unwrap_or_else(|p| p.into_inner());
        Self::prune_claims(&mut claims);
        let should_remove = claims.get(&key).is_some_and(|current| {
            current.status == ClaimStatus::Reserved && current.token == token
        });
        if should_remove {
            claims.remove(&key);
        }
        Ok(WireValue::Json(Value::Null))
    }

    fn try_claim(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let result = self.acquire_claim(args)?;
        if let WireValue::Json(ref json) = result {
            if json.get("status").and_then(|v| v.as_str()) == Some("acquired") {
                if let Some(lease) = json.get("lease") {
                    let commit_args = ServiceArgs::new(vec![WireValue::Json(lease.clone())]);
                    let _ = self.commit_claim(&commit_args);
                    return Ok(WireValue::Json(json!(true)));
                }
            }
        }
        Ok(WireValue::Json(json!(false)))
    }
}

impl Default for BroadcastService {
    fn default() -> Self {
        Self {
            claims: Arc::new(Mutex::new(HashMap::new())),
            bus: EventBus::new(256),
        }
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
            "send" => self.send(&args).map_err(ChannelFailure::from),
            "acquireClaim" => self.acquire_claim(&args).map_err(ChannelFailure::from),
            "commitClaim" => self.commit_claim(&args).map_err(ChannelFailure::from),
            "releaseClaim" => self.release_claim(&args).map_err(ChannelFailure::from),
            "tryClaim" => self.try_claim(&args).map_err(ChannelFailure::from),
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
            "onMessage" | "message" | "onDynamicMessage" => {
                Ok(EventSubscription::from_broadcast(self.bus.subscribe()))
            }
            _ => Err(ChannelFailure::event_not_found(event)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;
    use tokio::time::{timeout, Duration as TokioDuration};

    fn ctx() -> ChannelContext {
        ChannelContext::new("broadcast", ClientMode::DesktopContinuous)
    }

    #[tokio::test]
    async fn try_claim_acquires_and_blocks_duplicate() {
        let service = BroadcastService::new();
        let result = service
            .call(
                &ctx(),
                "tryClaim",
                vec![WireValue::Json(json!("key1"))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = result else {
            panic!("json")
        };
        assert_eq!(v, json!(true));

        // Second tryClaim on committed key should return false (committed state via acquire path).
        let result2 = service
            .call(
                &ctx(),
                "tryClaim",
                vec![WireValue::Json(json!("key1"))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v2) = result2 else {
            panic!("json")
        };
        // tryClaim commits on acquire; subsequent acquire sees committed => acquire returns committed; tryClaim returns false.
        assert_eq!(v2, json!(false));
    }

    #[tokio::test]
    async fn acquire_commit_release_flow() {
        let service = BroadcastService::new();
        let acq = service
            .call(
                &ctx(),
                "acquireClaim",
                vec![WireValue::Json(json!("key2"))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(a) = acq else {
            panic!("json")
        };
        assert_eq!(a["status"], "acquired");
        let lease = a["lease"].clone();
        assert_eq!(lease["key"], "key2");
        assert!(lease["token"].is_string());

        let committed = service
            .call(
                &ctx(),
                "commitClaim",
                vec![WireValue::Json(lease.clone())],
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(
            committed,
            WireValue::Json(Value::Null) | WireValue::Undefined | WireValue::Json(_)
        ));

        // Re-acquire returns committed.
        let acq2 = service
            .call(
                &ctx(),
                "acquireClaim",
                vec![WireValue::Json(json!("key2"))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(a2) = acq2 else {
            panic!("json")
        };
        assert_eq!(a2["status"], "committed");
    }

    #[tokio::test]
    async fn acquire_busy_then_release_allows_reacquire() {
        let service = BroadcastService::new();
        let acq = service
            .call(
                &ctx(),
                "acquireClaim",
                vec![WireValue::Json(json!("key3"))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(a) = acq else {
            panic!("json")
        };
        assert_eq!(a["status"], "acquired");
        let lease = a["lease"].clone();

        let busy = service
            .call(
                &ctx(),
                "acquireClaim",
                vec![WireValue::Json(json!("key3"))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(b) = busy else {
            panic!("json")
        };
        assert_eq!(b["status"], "busy");
        assert!(b["retryAfterMs"].is_number());

        service
            .call(
                &ctx(),
                "releaseClaim",
                vec![WireValue::Json(lease)],
                CancelToken::new(),
            )
            .await
            .unwrap();

        let acq2 = service
            .call(
                &ctx(),
                "acquireClaim",
                vec![WireValue::Json(json!("key3"))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(a2) = acq2 else {
            panic!("json")
        };
        assert_eq!(a2["status"], "acquired");
    }

    #[tokio::test]
    async fn send_publishes_to_on_message_listener() {
        let service = BroadcastService::new();
        let mut sub = service
            .listen(&ctx(), "onMessage", WireValue::Undefined)
            .await
            .unwrap();
        let msg = json!({"channel": "state:theme", "payload": {"theme": "dark"}});
        service
            .call(
                &ctx(),
                "send",
                vec![WireValue::Json(msg.clone())],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let received = timeout(TokioDuration::from_millis(500), sub.next())
            .await
            .expect("timed out waiting for broadcast")
            .expect("subscription ended");
        let WireValue::Json(v) = received else {
            panic!("expected json message")
        };
        assert_eq!(v["channel"], "state:theme");
        assert_eq!(v["payload"]["theme"], "dark");
    }

    #[tokio::test]
    async fn listen_unknown_event_fails() {
        let service = BroadcastService::new();
        assert!(service
            .listen(&ctx(), "onNope", WireValue::Undefined)
            .await
            .is_err());
    }
}
