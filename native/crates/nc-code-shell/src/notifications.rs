//! Task-notification click routing for the Tauri shell.
//!
//! The notification plugin reports an `actionPerformed` event containing the
//! numeric notification id. This registry maps that id back to the renderer's
//! `taskId` so the click can be emitted as `zcode:task-notification-click`
//! (Electron's `PlatformChannels.TaskNotificationClick`).
//!
//! Entries are bounded: notifications are ephemeral, and an old unclicked
//! notification must not grow the map forever.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

const MAX_PENDING: usize = 64;

#[derive(Default)]
pub struct NotificationClickRegistry {
    next_id: AtomicI32,
    tasks: Mutex<BTreeMap<i32, String>>,
}

impl NotificationClickRegistry {
    /// Allocate a positive notification id and remember its task id. Ids are
    /// monotonic; the oldest pending entry is evicted when the cap is reached.
    pub fn register(&self, task_id: Option<&str>) -> i32 {
        let previous = self.next_id.fetch_add(1, Ordering::SeqCst);
        // `Default` starts at 0; the first allocation must be 1, and the
        // atomic must resume at 2 rather than replaying 1.
        let id = if previous <= 0 {
            self.next_id.store(2, Ordering::SeqCst);
            1
        } else {
            previous
        };
        let Some(task_id) = task_id.map(str::trim).filter(|value| !value.is_empty()) else {
            return id;
        };
        let mut guard = match self.tasks.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        while guard.len() >= MAX_PENDING {
            let Some(oldest) = guard.keys().next().copied() else {
                break;
            };
            guard.remove(&oldest);
        }
        guard.insert(id, task_id.to_string());
        id
    }

    /// Consume the task id for a clicked notification.
    pub fn take(&self, id: i32) -> Option<String> {
        let mut guard = match self.tasks.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.remove(&id)
    }
}

/// A click on the notification body is reported as `tap` on Windows/macOS and
/// `default` by the freedesktop notification spec on Linux.
pub fn is_click_action(action_id: &str) -> bool {
    matches!(action_id, "tap" | "default")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_positive_unique_ids() {
        let registry = NotificationClickRegistry::default();
        let first = registry.register(Some("task-a"));
        let second = registry.register(Some("task-b"));
        assert!(first > 0);
        assert!(second > first);
        assert_eq!(registry.take(first).as_deref(), Some("task-a"));
        assert_eq!(registry.take(second).as_deref(), Some("task-b"));
    }

    #[test]
    fn ignores_blank_task_ids() {
        let registry = NotificationClickRegistry::default();
        let id = registry.register(Some("   "));
        assert!(registry.take(id).is_none());
    }

    #[test]
    fn take_consumes_the_entry() {
        let registry = NotificationClickRegistry::default();
        let id = registry.register(Some("task-a"));
        assert_eq!(registry.take(id).as_deref(), Some("task-a"));
        assert!(registry.take(id).is_none());
    }

    #[test]
    fn evicts_oldest_entries_at_the_cap() {
        let registry = NotificationClickRegistry::default();
        let first = registry.register(Some("task-0"));
        for index in 1..=MAX_PENDING {
            registry.register(Some(&format!("task-{index}")));
        }
        assert!(registry.take(first).is_none());
        let guard = registry.tasks.lock().unwrap();
        assert_eq!(guard.len(), MAX_PENDING);
        assert!(!guard.contains_key(&first));
    }

    #[test]
    fn click_actions_cover_platform_variants() {
        assert!(is_click_action("tap"));
        assert!(is_click_action("default"));
        assert!(!is_click_action("snooze"));
        assert!(!is_click_action(""));
    }
}
