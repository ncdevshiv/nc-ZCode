//! Timing wrapper for streaming transport (the single source of truth for throughput measurement).
//!
//! WHY wrap a layer instead of scattering timestamps in the executor: `executor::execute` has only two
//! injection points, transport and clock (see the module comment in `transport.rs`); recording "first/last
//! chunk arrival time" inside a transport wrapper neither touches retry nor classification logic, and lets
//! callers read the real network cadence after `execute` returns — so TPS is measured, not estimated.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::transport::{
    ProviderTransport, TransportFailure, TransportRequest, TransportResponseHead,
};

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as u64)
        .unwrap_or(0)
}

/// The streaming window of one session (or one round). Atomic values = shareable across awaits/threads, no lock needed.
#[derive(Clone, Default)]
pub struct StreamTiming {
    first_chunk_ms: Arc<AtomicU64>,
    last_chunk_ms: Arc<AtomicU64>,
    chunk_count: Arc<AtomicU64>,
}

impl StreamTiming {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cleared at round start: TPS may only cover this round's chunks, never reuse the previous round's last chunk.
    pub fn reset(&self) {
        self.first_chunk_ms.store(0, Ordering::SeqCst);
        self.last_chunk_ms.store(0, Ordering::SeqCst);
        self.chunk_count.store(0, Ordering::SeqCst);
    }

    fn record_chunk(&self) {
        let now = epoch_ms();
        self.last_chunk_ms.store(now, Ordering::SeqCst);
        self.chunk_count.fetch_add(1, Ordering::SeqCst);
        // Record the first chunk only once: a CAS loop instead of fetch_update (the latter is deprecated in newer compilers).
        let mut current = self.first_chunk_ms.load(Ordering::SeqCst);
        loop {
            if current != 0 {
                break;
            }
            match self.first_chunk_ms.compare_exchange(
                current,
                now,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(seen) => current = seen,
            }
        }
    }

    /// `(first-chunk ms, last-chunk ms, chunk count)`; returns None when no chunk arrived.
    pub fn window(&self) -> Option<(u64, u64, u64)> {
        let first = self.first_chunk_ms.load(Ordering::SeqCst);
        let last = self.last_chunk_ms.load(Ordering::SeqCst);
        let count = self.chunk_count.load(Ordering::SeqCst);
        if first == 0 || count == 0 {
            return None;
        }
        Some((first, last, count))
    }

    /// Generation window (last chunk − first chunk). When the whole span falls in the same millisecond it returns 0,
    /// and the caller labels `windowSource` truthfully rather than fabricating a duration.
    pub fn generation_ms(&self) -> u64 {
        self.window()
            .map(|(first, last, _)| last.saturating_sub(first))
            .unwrap_or(0)
    }
}

/// A transport wrapper that records chunk arrival times; all other behavior forwards verbatim to inner.
pub struct TimedTransport<T: ProviderTransport> {
    inner: T,
    timing: StreamTiming,
}

impl<T: ProviderTransport> TimedTransport<T> {
    pub fn new(inner: T, timing: StreamTiming) -> Self {
        Self { inner, timing }
    }

    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: ProviderTransport + Send> ProviderTransport for TimedTransport<T> {
    fn open(
        &mut self,
        request: TransportRequest,
        idle_timeout_ms: Option<u64>,
    ) -> impl Future<Output = Result<TransportResponseHead, TransportFailure>> + Send {
        self.inner.open(request, idle_timeout_ms)
    }

    fn read_body(&mut self) -> impl Future<Output = Result<Vec<u8>, TransportFailure>> + Send {
        self.inner.read_body()
    }

    fn next_chunk(
        &mut self,
        idle_timeout_ms: Option<u64>,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, TransportFailure>> + Send {
        // First take out inner's mutable borrow and the recorder separately, so the async block does not double-borrow self.
        let timing = self.timing.clone();
        let inner = &mut self.inner;
        async move {
            let chunk = inner.next_chunk(idle_timeout_ms).await?;
            if chunk.is_some() {
                timing.record_chunk();
            }
            Ok(chunk)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Scripted {
        chunks: Vec<Vec<u8>>,
        index: usize,
        delay_ms: u64,
    }

    impl ProviderTransport for Scripted {
        fn open(
            &mut self,
            _request: TransportRequest,
            _idle_timeout_ms: Option<u64>,
        ) -> impl Future<Output = Result<TransportResponseHead, TransportFailure>> + Send {
            std::future::ready(Ok(TransportResponseHead {
                status: 200,
                headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
            }))
        }

        fn read_body(&mut self) -> impl Future<Output = Result<Vec<u8>, TransportFailure>> + Send {
            std::future::ready(Ok(Vec::new()))
        }

        fn next_chunk(
            &mut self,
            _idle_timeout_ms: Option<u64>,
        ) -> impl Future<Output = Result<Option<Vec<u8>>, TransportFailure>> + Send {
            let delay = self.delay_ms;
            let item = if self.index < self.chunks.len() {
                self.index += 1;
                Some(self.chunks[self.index - 1].clone())
            } else {
                None
            };
            async move {
                if delay > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                Ok(item)
            }
        }
    }

    #[tokio::test]
    async fn records_first_and_last_chunk_window() {
        let timing = StreamTiming::new();
        let inner = Scripted {
            chunks: vec![
                b"data: a".to_vec(),
                b"data: b".to_vec(),
                b"data: c".to_vec(),
            ],
            index: 0,
            delay_ms: 12,
        };
        let mut transport = TimedTransport::new(inner, timing.clone());
        for _ in 0..4 {
            let _ = transport
                .next_chunk(None)
                .await
                .expect("chunk read must succeed");
        }
        let (first, last, count) = timing.window().expect("window recorded");
        assert_eq!(count, 3, "only real chunks count");
        assert!(last > first, "3 chunks at 12ms spacing must span time");
        assert!(timing.generation_ms() >= 20);
    }

    #[tokio::test]
    async fn reset_clears_the_previous_round_window() {
        let timing = StreamTiming::new();
        let inner = Scripted {
            chunks: vec![b"data: a".to_vec()],
            index: 0,
            delay_ms: 0,
        };
        let mut transport = TimedTransport::new(inner, timing.clone());
        let _ = transport.next_chunk(None).await.expect("chunk");
        assert!(timing.window().is_some());
        timing.reset();
        assert!(timing.window().is_none());
        assert_eq!(timing.generation_ms(), 0);
    }

    #[test]
    fn unused_lock_helper_is_not_needed() {
        // The recorder must be shareable across tasks without a Mutex (atomic counters).
        let timing = StreamTiming::new();
        let cloned = timing.clone();
        let _ = std::thread::spawn(move || cloned.record_chunk());
        let _ = Arc::strong_count(&timing.chunk_count);
    }

    fn assert_send<T: Send>(_: &T) {}

    #[test]
    fn timing_is_send_across_tasks() {
        let timing = StreamTiming::new();
        assert_send(&timing);
        let _ = Mutex::new(());
    }
}
