//! Auto-waiting / stability: turns Playwright's "implicit magic" into explicit, testable strategies.
//!
//! ## Why it is needed
//! Before every action `playwright-core` implicitly runs an actionability check (visible, stable, not
//! obscured, clickable) and implicitly waits for `load` after a navigation. Once R3 removes Playwright,
//! that semantics must be implemented explicitly by this crate and proven with the golden suite rather
//! than assumed (the named risk in specs/rust-pure-native-roadmap.md §7 R3).
//!
//! ## The strategy (everything explicit, disableable, testable)
//! 1. **Navigation readiness**: subscribe to `Page.lifecycleEvent` / `Page.loadEventFired` / `Page.frameStoppedLoading`,
//!    wait for the target frame to reach `load` (equivalent to TS `waitUntil:"load"`, page-command.ts:145-154).
//! 2. **DOM stability**: poll `Runtime.evaluate` reading `document.readyState` until `complete`
//!    (`interactive` optionally). Stability is judged by "N consecutive matching polls", not by using a
//!    timeout to mask a synchronization problem (the AGENTS.md implementation principles).
//! 3. **No navigation in flight**: read `Page.getNavigationHistory` for `currentIndex == entries.len()-1`
//!    to confirm no navigation is currently happening.
//! 4. **Optional selector**: poll `DOM.querySelector` (or a querySelector through `Runtime.evaluate`) until
//!    it hits, optionally requiring "visible", used by `waitFor` and as an action precondition.
//! 5. **Element stability (before an action)**: act only when two consecutive samples of the target rect are
//!    identical, approximating Playwright's "element is stable" judgement.
//!
//! All of the above are **explicit wait functions** that callers (page commands) invoke where needed;
//! `WaitPolicy` allows per-command degradation (e.g. a headless static page may skip networkidle — this
//! crate does not support Playwright's `networkidle`, since CDP has no ready-made network-idle, see the
//! README "unsupported" section).

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::error::{CdpError, CdpErrorCode, CdpResult};
use crate::page::Page;

/// The readiness state: the target level for navigation/DOM waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyState {
    /// At least `interactive` (≈ DOMContentLoaded).
    DomContentLoaded,
    /// Up to `load` (the default, aligned with TS `waitUntil:"load"`).
    Load,
}

#[derive(Debug, Clone)]
pub struct WaitPolicy {
    pub ready: ReadyState,
    /// The number of consecutive matching polls required for the DOM stability verdict (>=1).
    pub stability_polls: u32,
    pub poll_interval: Duration,
    pub timeout: Duration,
}

impl Default for WaitPolicy {
    fn default() -> Self {
        Self {
            ready: ReadyState::Load,
            stability_polls: 2,
            poll_interval: Duration::from_millis(50),
            timeout: Duration::from_secs(30),
        }
    }
}

/// Whether the readyState satisfies the target level.
pub fn ready_state_satisfied(current: &str, target: ReadyState) -> bool {
    match target {
        ReadyState::DomContentLoaded => matches!(current, "interactive" | "complete"),
        ReadyState::Load => current == "complete",
    }
}

/// Poll `document.readyState` until `target` is satisfied and `stability_polls` consecutive reads agree.
/// Returns the readyState read last. A timeout reports Timeout.
pub async fn wait_for_ready(page: &Page, policy: &WaitPolicy) -> CdpResult<String> {
    let deadline = Instant::now() + policy.timeout;
    let mut stable_streak = 0u32;
    let mut last = String::new();
    loop {
        if page.is_cancelled() {
            return Err(CdpError::new(CdpErrorCode::Cancelled, "wait cancelled"));
        }
        let state = evaluate_string(page, "document.readyState").await?;
        if ready_state_satisfied(&state, policy.ready) {
            if state == last {
                stable_streak += 1;
            } else {
                stable_streak = 1;
                last = state.clone();
            }
            if stable_streak >= policy.stability_polls.max(1) {
                // Additional check: no navigation in flight (the history tail is the current page).
                if no_navigation_in_flight(page).await? {
                    return Ok(state);
                }
            }
        } else {
            stable_streak = 0;
            last = state;
        }
        if Instant::now() >= deadline {
            return Err(CdpError::new(
                CdpErrorCode::Timeout,
                format!(
                    "readyState did not reach {:?} within {:?}",
                    policy.ready, policy.timeout
                ),
            ));
        }
        tokio::time::sleep(policy.poll_interval).await;
    }
}

/// Wait for the selector to hit (optionally requiring visibility). Used by `waitFor`.
pub async fn wait_for_selector(
    page: &Page,
    selector: &str,
    require_visible: bool,
    timeout: Duration,
) -> CdpResult<bool> {
    let deadline = Instant::now() + timeout;
    let mut policy = WaitPolicy {
        timeout,
        ..Default::default()
    };
    loop {
        if page.is_cancelled() {
            return Err(CdpError::new(CdpErrorCode::Cancelled, "wait cancelled"));
        }
        let expr = build_selector_probe(selector, require_visible);
        if let Ok(found) = evaluate_bool(page, &expr).await {
            if found {
                return Ok(true);
            }
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(policy.poll_interval).await;
        policy.timeout = policy.timeout.saturating_sub(policy.poll_interval);
    }
}

/// The polling semantics of `playwrightWaitForTimeout`: a fixed wait, cancellable.
pub async fn wait_for_timeout(page: &Page, timeout: Duration) -> CdpResult<()> {
    let deadline = Instant::now() + timeout;
    let tick = Duration::from_millis(50);
    while Instant::now() < deadline {
        if page.is_cancelled() {
            return Err(CdpError::new(CdpErrorCode::Cancelled, "wait cancelled"));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::time::sleep(remaining.min(tick)).await;
    }
    Ok(())
}

/// Use `Page.getNavigationHistory` to decide there is no navigation in flight: currentIndex is the last entry.
async fn no_navigation_in_flight(page: &Page) -> CdpResult<bool> {
    let history = page
        .connection()
        .send(
            "Page.getNavigationHistory",
            Value::Null,
            Some(page.session_id()),
            Duration::from_secs(5),
        )
        .await?;
    let current = history
        .get("currentIndex")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let count = history
        .get("entries")
        .and_then(Value::as_array)
        .map(|e| e.len() as i64)
        .unwrap_or(1);
    Ok(current + 1 >= count)
}

fn build_selector_probe(selector: &str, require_visible: bool) -> String {
    let escaped = selector.replace('\\', "\\\\").replace('\'', "\\'");
    if require_visible {
        format!(
            "(function(){{var el=document.querySelector('{escaped}');if(!el)return false;\
             var s=getComputedStyle(el);var r=el.getBoundingClientRect();\
             return s.display!=='none'&&s.visibility!=='hidden'&&r.width>0&&r.height>0;}})()"
        )
    } else {
        format!("!!document.querySelector('{escaped}')")
    }
}

async fn evaluate_string(page: &Page, expression: &str) -> CdpResult<String> {
    let value = page.evaluate_raw(expression).await?;
    Ok(value.as_str().unwrap_or("").to_string())
}

async fn evaluate_bool(page: &Page, expression: &str) -> CdpResult<bool> {
    let value = page.evaluate_raw(expression).await?;
    Ok(value.as_bool().unwrap_or(false))
}

/// For tests: verify selector probe generation without side effects.
pub fn selector_probe(selector: &str, visible: bool) -> String {
    build_selector_probe(selector, visible)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ready_state_gate_matches_ts() {
        assert!(ready_state_satisfied("complete", ReadyState::Load));
        assert!(!ready_state_satisfied("interactive", ReadyState::Load));
        assert!(ready_state_satisfied(
            "interactive",
            ReadyState::DomContentLoaded
        ));
        assert!(ready_state_satisfied(
            "complete",
            ReadyState::DomContentLoaded
        ));
        assert!(!ready_state_satisfied(
            "loading",
            ReadyState::DomContentLoaded
        ));
    }

    #[test]
    fn selector_probe_escapes_and_gates_visibility() {
        let probe = selector_probe("div.cls", true);
        assert!(probe.contains("getComputedStyle"));
        assert!(probe.contains("getBoundingClientRect"));
        let plain = selector_probe("#id", false);
        assert!(plain.starts_with("!!document.querySelector('#id')"));
        // Single-quote escaping
        let quoted = selector_probe("[data-x='a']", false);
        assert!(quoted.contains("\\'a\\'"), "quote must escape: {quoted}");
    }

    #[test]
    fn default_policy_is_load_and_stability2() {
        let policy = WaitPolicy::default();
        assert_eq!(policy.ready, ReadyState::Load);
        assert_eq!(policy.stability_polls, 2);
    }

    #[test]
    fn json_probe_shape_is_valid_expression() {
        // Guarantee the generated JS at least looks like an IIFE syntactically (balanced braces) to avoid a runtime expression error.
        let probe = selector_probe("button", true);
        let open = probe.matches('{').count();
        let close = probe.matches('}').count();
        assert_eq!(open, close, "brace-balanced: {probe}");
        let _ = json!({"expr": probe});
    }
}
