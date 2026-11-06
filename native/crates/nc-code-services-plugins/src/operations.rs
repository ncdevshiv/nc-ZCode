//! Operation registry for `plugin-management` — the Rust counterpart of the CLI's
//! `pluginOperationControllers` map (`nc-code-protocol/server.ts`) and the per-operation
//! progress emitter (`getPluginOperationProgressEmitter`).
//!
//! Contract being mirrored:
//! - a long-running request may carry a client-provided `operationId`; the CLI stores one
//!   `AbortController` under that id and passes its signal into the plugin engine;
//! - `cancelPluginOperation({operationId})` aborts that controller and reports whether an
//!   active operation was found;
//! - `onDynamicPluginOperationProgress(operationId)` subscribes to a per-operation event
//!   stream; `resolveSuggestedPluginReference` emits `{operationId, state: "refreshing"}`
//!   before it refreshes the official marketplace.
//!
//! The Rust engine has no `AbortSignal`; every long-running request owns one
//! [`AtomicBool`] that the orchestration layer already probes at its network/disk
//! boundaries (`crate::fetch::check_cancelled`). The registry stores that same flag under
//! the operation id, so cancelling by id flips exactly the flag the engine reads.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use nc_code_channels::{CancelToken, EventBus, EventSubscription};
use nc_code_rpc::WireValue;
use serde_json::json;

/// One in-flight request's cancellation flag, optionally registered under an operation id.
pub struct OperationTicket {
    operation_id: Option<String>,
    flag: Arc<AtomicBool>,
}

impl OperationTicket {
    /// Flag handed to the orchestration layer (shared with `cancelPluginOperation`).
    pub fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.flag)
    }
}

/// Stops the forwarding task when the request finishes.
pub struct CancelBridge {
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for CancelBridge {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[derive(Default)]
pub struct OperationRegistry {
    flags: Mutex<HashMap<String, Arc<AtomicBool>>>,
    progress: Mutex<HashMap<String, EventBus>>,
}

impl OperationRegistry {
    /// Register a fresh flag for this request. Without an `operationId` the flag still
    /// exists (the request-level cancel token feeds it) but nothing can address it by id.
    pub fn begin(&self, operation_id: Option<&str>) -> OperationTicket {
        let flag = Arc::new(AtomicBool::new(false));
        if let Some(operation_id) = operation_id {
            self.lock_flags()
                .insert(operation_id.to_string(), Arc::clone(&flag));
        }
        OperationTicket {
            operation_id: operation_id.map(str::to_string),
            flag,
        }
    }

    /// Release the operation id. Only removes the entry when it still points at this
    /// ticket (the TypeScript server compares controller identity for the same reason);
    /// a later request that reused the id keeps its own cancellation entry.
    pub fn finish(&self, ticket: &OperationTicket) {
        let Some(operation_id) = &ticket.operation_id else {
            return;
        };
        let mut flags = self.lock_flags();
        let matches = flags
            .get(operation_id)
            .is_some_and(|flag| Arc::ptr_eq(flag, &ticket.flag));
        if matches {
            flags.remove(operation_id);
        }
        drop(flags);
        // The progress stream belongs to the operation; dropping the sender ends any
        // still-connected subscription after delivering queued values.
        self.lock_progress().remove(operation_id);
    }

    /// `cancelPluginOperation`: abort the operation registered under `operation_id`.
    pub fn cancel(&self, operation_id: &str) -> bool {
        let flag = self.lock_flags().remove(operation_id);
        match flag {
            Some(flag) => {
                flag.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    /// Subscribe to `onDynamicPluginOperationProgress(operationId)`.
    pub fn progress_subscription(&self, operation_id: &str) -> EventSubscription {
        let bus = self.progress_bus(operation_id);
        EventSubscription::from_broadcast(bus.subscribe())
    }

    /// Emit one progress notification for the operation (dropped when nobody listens,
    /// mirroring the TypeScript emitter).
    pub fn publish_progress(&self, operation_id: &str, state: &str) {
        let bus = self.progress_bus(operation_id);
        bus.send(WireValue::Json(json!({
            "operationId": operation_id,
            "state": state,
        })));
    }

    fn progress_bus(&self, operation_id: &str) -> EventBus {
        self.lock_progress()
            .entry(operation_id.to_string())
            .or_default()
            .clone()
    }

    fn lock_flags(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<AtomicBool>>> {
        self.flags
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_progress(&self) -> std::sync::MutexGuard<'_, HashMap<String, EventBus>> {
        self.progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Forward a transport-level `CancelToken` (client `PromiseCancel`) into the flag the
/// orchestration layer checks. Cancelling the promise therefore stops the network/disk
/// work at the next boundary instead of leaving it running while the response is dropped.
pub fn bridge_cancel(cancel: &CancelToken, ticket: &OperationTicket) -> CancelBridge {
    let flag = Arc::clone(&ticket.flag);
    if cancel.is_cancelled() {
        flag.store(true, Ordering::SeqCst);
    }
    let token = cancel.clone();
    let handle = tokio::spawn(async move {
        token.cancelled().await;
        flag.store(true, Ordering::SeqCst);
    });
    CancelBridge { handle }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancel_by_id_flips_the_registered_flag_once() {
        let registry = OperationRegistry::default();
        let ticket = registry.begin(Some("op-1"));
        let flag = ticket.flag();
        assert!(!flag.load(Ordering::SeqCst));
        assert!(registry.cancel("op-1"));
        assert!(flag.load(Ordering::SeqCst));
        // Second cancel finds no active operation (entry was consumed).
        assert!(!registry.cancel("op-1"));
        registry.finish(&ticket);
        assert!(!registry.cancel("op-1"));
    }

    #[tokio::test]
    async fn finish_keeps_a_reused_operation_id() {
        let registry = OperationRegistry::default();
        let first = registry.begin(Some("op"));
        let second = registry.begin(Some("op"));
        // The first request finishing must not remove the second registration.
        registry.finish(&first);
        assert!(registry.cancel("op"));
        assert!(second.flag().load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn transport_cancel_sets_the_operation_flag() {
        let registry = OperationRegistry::default();
        let ticket = registry.begin(Some("op-cancel"));
        let cancel = CancelToken::new();
        let _bridge = bridge_cancel(&cancel, &ticket);
        cancel.cancel();
        for _ in 0..100 {
            if ticket.flag().load(Ordering::SeqCst) {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("cancel token did not reach the operation flag");
    }

    #[tokio::test]
    async fn progress_is_delivered_only_to_subscribers() {
        let registry = OperationRegistry::default();
        registry.publish_progress("op-progress", "refreshing");
        let mut subscription = registry.progress_subscription("op-progress");
        registry.publish_progress("op-progress", "refreshing");
        let value = subscription.next().await.expect("progress event");
        let WireValue::Json(value) = value else {
            panic!("progress payload must be JSON");
        };
        assert_eq!(value["operationId"], json!("op-progress"));
        assert_eq!(value["state"], json!("refreshing"));
    }
}
