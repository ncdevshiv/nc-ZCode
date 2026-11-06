//! Short-lived, single-use host capabilities for `/ws/host`.
//!
//! Mirror of `packages/server/src/hostCapability.ts`: a capability is consumed on
//! first use regardless of expiry/replay, so a leaked ticket cannot be replayed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rand::rngs::OsRng;
use rand::RngCore;
use serde_json::{json, Value};

pub const DEFAULT_HOST_CAPABILITY_TTL_MS: u64 = 30_000;
pub const HOST_CAPABILITY_HEADER: &str = "x-nc-code-rpc-host-capability";

pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

pub struct HostCapabilityStore {
    ttl_ms: u64,
    now: Clock,
    expires_by_capability: Mutex<HashMap<String, u64>>,
}

impl HostCapabilityStore {
    pub fn new(ttl_ms: u64) -> Self {
        Self::with_clock(ttl_ms, Arc::new(default_now_ms))
    }

    pub fn with_clock(ttl_ms: u64, now: Clock) -> Self {
        Self {
            ttl_ms,
            now,
            expires_by_capability: Mutex::new(HashMap::new()),
        }
    }

    pub fn issue(&self) -> Value {
        let issued_at = (self.now)();
        self.purge_expired(issued_at);
        let capability = random_capability();
        let expires_at = issued_at.saturating_add(self.ttl_ms);
        self.lock().insert(capability.clone(), expires_at);
        json!({ "capability": capability, "expiresAt": expires_at })
    }

    pub fn consume(&self, capability: Option<&str>) -> bool {
        let Some(capability) = capability else {
            return false;
        };
        let consumed_at = (self.now)();
        let expires_at = self.lock().remove(capability);
        self.purge_expired(consumed_at);
        expires_at.is_some_and(|expires_at| expires_at > consumed_at)
    }

    fn purge_expired(&self, at: u64) {
        self.lock().retain(|_, expires_at| *expires_at > at);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, u64>> {
        self.expires_by_capability
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for HostCapabilityStore {
    fn default() -> Self {
        Self::new(DEFAULT_HOST_CAPABILITY_TTL_MS)
    }
}

fn default_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn random_capability() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let mut encoded = String::with_capacity(64);
    for byte in bytes {
        encoded.push_str(&format!("{byte:02x}"));
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn clock() -> (Clock, Arc<AtomicU64>) {
        let now = Arc::new(AtomicU64::new(1_000));
        let handle = Arc::clone(&now);
        (Arc::new(move || handle.load(Ordering::SeqCst)), now)
    }

    #[test]
    fn capability_is_single_use() {
        let (clock, _) = clock();
        let store = HostCapabilityStore::with_clock(30_000, clock);
        let issued = store.issue();
        let capability = issued["capability"].as_str().expect("capability");
        assert_eq!(issued["expiresAt"], json!(31_000));
        assert!(store.consume(Some(capability)));
        assert!(!store.consume(Some(capability)), "replay must fail");
    }

    #[test]
    fn capability_expires_and_unknown_is_rejected() {
        let (clock, now) = clock();
        let store = HostCapabilityStore::with_clock(1_000, clock);
        let capability = store.issue()["capability"]
            .as_str()
            .expect("capability")
            .to_string();
        now.store(2_001, Ordering::SeqCst);
        assert!(!store.consume(Some(&capability)));
        assert!(!store.consume(None));
        assert!(!store.consume(Some("not-a-ticket")));
    }
}
