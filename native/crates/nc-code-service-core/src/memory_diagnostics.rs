//! In-process memory diagnostics registry (port of `packages/services/src/memoryDiagnostics.ts`
//! and `@zcode/shared` registry concept). Services register providers; collector aggregates.

use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

pub type MemoryDiagnosticsProvider = Arc<dyn Fn() -> BTreeMap<String, f64> + Send + Sync>;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Default)]
pub struct MemoryDiagnosticsRegistry {
    inner: Arc<Mutex<BTreeMap<u64, (String, MemoryDiagnosticsProvider)>>>,
}

pub struct MemoryDiagnosticsHandle {
    id: u64,
    inner: Arc<Mutex<BTreeMap<u64, (String, MemoryDiagnosticsProvider)>>>,
}

impl Drop for MemoryDiagnosticsHandle {
    fn drop(&mut self) {
        let mut inner = self.inner.lock().unwrap();
        inner.remove(&self.id);
    }
}

impl MemoryDiagnosticsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &self,
        name: impl Into<String>,
        provider: MemoryDiagnosticsProvider,
    ) -> MemoryDiagnosticsHandle {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let mut inner = self.inner.lock().unwrap();
        inner.insert(id, (name.into(), provider));
        MemoryDiagnosticsHandle {
            id,
            inner: self.inner.clone(),
        }
    }

    pub fn collect(&self) -> BTreeMap<String, f64> {
        let snapshot: Vec<(String, MemoryDiagnosticsProvider)> = {
            let inner = self.inner.lock().unwrap();
            inner.values().cloned().collect()
        };
        let mut out = BTreeMap::new();
        for (name, provider) in snapshot {
            for (k, v) in provider() {
                out.insert(format!("{name}.{k}"), v);
            }
        }
        out
    }
}

/// Global registry matching TS `memoryDiagnosticsRegistry` export.
pub fn global_registry() -> &'static MemoryDiagnosticsRegistry {
    static REG: std::sync::OnceLock<MemoryDiagnosticsRegistry> = std::sync::OnceLock::new();
    REG.get_or_init(MemoryDiagnosticsRegistry::new)
}

pub fn register_memory_diagnostics_provider(
    name: impl Into<String>,
    provider: MemoryDiagnosticsProvider,
) -> MemoryDiagnosticsHandle {
    global_registry().register(name, provider)
}

pub fn collect_service_memory_diagnostics() -> BTreeMap<String, f64> {
    global_registry().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_collect_and_drop() {
        let reg = MemoryDiagnosticsRegistry::new();
        let handle = reg.register("svc", Arc::new(|| BTreeMap::from([("n".to_string(), 2.0)])));
        let got = reg.collect();
        assert_eq!(got.get("svc.n"), Some(&2.0));
        drop(handle);
        assert!(reg.collect().is_empty());
    }
}
