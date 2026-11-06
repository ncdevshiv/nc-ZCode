//! Event subscriptions and per-request cancellation.
//!
//! Channels produce [`EventSubscription`]s for `EventListen`; the connection forwards
//! every value as `EventFire`. A subscription backed by [`EventBus`] mirrors the
//! TypeScript `Event` semantics closely enough for the frontend: values emitted while
//! nobody listens are dropped (TS `bufferEvent` only buffers during subscription
//! hand-off), and slow consumers drop the oldest values instead of blocking producers.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use nc_code_rpc::WireValue;
use tokio::sync::{broadcast, mpsc, Notify};

/// Cancellation token for one in-flight request or event subscription.
///
/// `PromiseCancel` / `EventDispose` set it; long-running handlers await
/// [`CancelToken::cancelled`] and stop producing. The connection suppresses any
/// response/event produced after cancellation, so a canceled id never receives frames.
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if !self.flag.swap(true, Ordering::SeqCst) {
            // notify_one stores a permit when no waiter is registered, so a
            // `cancelled()` call that starts after `cancel()` still returns.
            self.notify.notify_one();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub(crate) fn is_same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.flag, &other.flag)
    }

    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        self.notify.notified().await;
    }
}

/// A stream of event payloads for one subscription id.
pub struct EventSubscription {
    source: EventSource,
}

enum EventSource {
    Mpsc(mpsc::Receiver<WireValue>),
    Broadcast(broadcast::Receiver<WireValue>),
    /// Broadcast with a projection: the host's stdout fan-out carries every
    /// notification (conversation frames, sessions-index frames, local TTFT
    /// facts); one channel event only wants its own notification params.
    Mapped {
        receiver: broadcast::Receiver<WireValue>,
        project: Arc<dyn Fn(WireValue) -> Option<WireValue> + Send + Sync>,
    },
}

impl EventSubscription {
    pub fn from_receiver(receiver: mpsc::Receiver<WireValue>) -> Self {
        Self {
            source: EventSource::Mpsc(receiver),
        }
    }

    pub fn from_broadcast(receiver: broadcast::Receiver<WireValue>) -> Self {
        Self {
            source: EventSource::Broadcast(receiver),
        }
    }

    /// Project each broadcast value; values the projection rejects are skipped
    /// (the upstream keeps flowing, so a foreign notification never terminates
    /// or leaks into this event).
    pub fn from_broadcast_mapped(
        receiver: broadcast::Receiver<WireValue>,
        project: impl Fn(WireValue) -> Option<WireValue> + Send + Sync + 'static,
    ) -> Self {
        Self {
            source: EventSource::Mapped {
                receiver,
                project: Arc::new(project),
            },
        }
    }

    /// Next payload; `None` once the producer is gone.
    pub async fn next(&mut self) -> Option<WireValue> {
        match &mut self.source {
            EventSource::Mpsc(receiver) => receiver.recv().await,
            EventSource::Broadcast(receiver) => loop {
                match receiver.recv().await {
                    Ok(value) => return Some(value),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            },
            EventSource::Mapped { receiver, project } => loop {
                match receiver.recv().await {
                    Ok(value) => {
                        if let Some(projected) = project(value) {
                            return Some(projected);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            },
        }
    }
}

/// Process-wide event fan-out for one channel event name.
#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<WireValue>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(1));
        Self { sender }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<WireValue> {
        self.sender.subscribe()
    }

    pub fn send(&self, value: WireValue) {
        // No subscribers is the normal idle state, not an error.
        let _ = self.sender.send(value);
    }

    pub fn receiver_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(256)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancel_is_idempotent_and_wakes_late_waiters() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
        token.cancel();
        token.cancel();
        assert!(token.is_cancelled());
        tokio::time::timeout(std::time::Duration::from_millis(50), token.cancelled())
            .await
            .expect("a token cancelled before the wait must return immediately");
    }

    #[tokio::test]
    async fn cancel_wakes_an_active_waiter() {
        let token = CancelToken::new();
        let waiter = token.clone();
        let handle = tokio::spawn(async move { waiter.cancelled().await });
        token.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("waiter must complete")
            .expect("task");
    }

    #[tokio::test]
    async fn broadcast_subscription_drops_before_subscribe_and_delivers_after() {
        let bus = EventBus::new(8);
        bus.send(WireValue::Int(1));
        let mut subscription = EventSubscription::from_broadcast(bus.subscribe());
        bus.send(WireValue::Int(2));
        assert_eq!(
            subscription.next().await,
            Some(WireValue::Int(2)),
            "values emitted before subscribe are not replayed"
        );
    }

    #[tokio::test]
    async fn lagged_broadcast_subscription_keeps_the_newest_values() {
        let bus = EventBus::new(2);
        let mut subscription = EventSubscription::from_broadcast(bus.subscribe());
        for value in 0..10 {
            bus.send(WireValue::Int(value));
        }
        let first = subscription.next().await.expect("first");
        let second = subscription.next().await.expect("second");
        assert!(matches!(first, WireValue::Int(value) if value >= 8));
        assert!(matches!(second, WireValue::Int(value) if value >= 8));
    }

    #[tokio::test]
    async fn mapped_subscription_projects_and_skips_rejected_values() {
        let bus = EventBus::new(8);
        let mut subscription =
            EventSubscription::from_broadcast_mapped(bus.subscribe(), |value| match value {
                WireValue::Int(even) if even % 2 == 0 => Some(WireValue::Int(even * 10)),
                _ => None,
            });
        bus.send(WireValue::Int(1));
        bus.send(WireValue::Int(2));
        bus.send(WireValue::Int(3));
        bus.send(WireValue::Int(4));
        assert_eq!(subscription.next().await, Some(WireValue::Int(20)));
        assert_eq!(
            subscription.next().await,
            Some(WireValue::Int(40)),
            "rejected values are skipped, not delivered and not fatal"
        );
    }
}
