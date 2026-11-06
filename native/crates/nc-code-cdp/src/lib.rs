//! nc-code-cdp —— the R3 raw CDP client island (specs/rust-pure-native-roadmap.md §7 R3).
//!
//! A hand-written Chrome DevTools Protocol client replacing playwright-core:
//! - [`transport`]: the WS send/receive loop + command/response correlation by id + events broadcast by method;
//! - [`discovery`]: registry-free browser executable discovery (Windows/macOS/Linux);
//! - [`launch`]: managed launch (a free port + an isolated profile + DevTools readiness polling + process reaping);
//! - [`snapshot`]: Accessibility.getFullAXTree → the schema equivalent of the TS BrowserSnapshot;
//! - [`auto_wait`]: turns Playwright's implicit actionability waits into explicit, testable strategies;
//! - [`page`]: CDP operations on a single tab (navigation/input/screenshot/snapshot/ref resolution);
//! - [`commands`]: page-level dispatch of the TS BrowserCommand discriminated union;
//! - [`modifiers`]: BrowserKeyModifier[] → the CDP modifiers bit field.
//!
//! ## Boundaries
//! - Playwright locator engines, recording, browserVisibility and the claimTab/finalize lifecycle are
//!   unsupported — matching the unsupported set of the TS managed-CDP backend (descriptor.ts:16-25 /
//!   index.ts:336-350); when hit, return capability_unsupported rather than silently degrading.
//! - networkidle is unsupported (CDP has no ready-made network-idle criterion); waitUntil only offers
//!   domContentLoaded/load (auto_wait::ReadyState).
//! - framePath (iframe traversal) is not produced yet: per-frame AX trees require a separate
//!   getFullAXTree per frameId, to be wired in during integration (the "known differences" section of the
//!   snapshot.rs module doc).
//! - The error codes returned by wire_code() converge onto the strict 9-value set of the TS
//!   browserErrorCodeSchema; the Rust internal diagnostic superset never leaks onto the wire (error.rs).

pub mod auto_wait;
pub mod commands;
pub mod discovery;
pub mod error;
pub mod launch;
pub mod modifiers;
pub mod page;
pub mod snapshot;
pub mod transport;

pub use auto_wait::{
    ready_state_satisfied, selector_probe, wait_for_ready, wait_for_selector, wait_for_timeout,
    ReadyState, WaitPolicy,
};
pub use commands::{error_json, execute_page_command, is_allowed_url};
pub use discovery::{
    discover_installed_browser, discover_installed_browsers, validate_explicit, BrowserExecutable,
    BrowserFamily,
};
pub use error::{CdpError, CdpErrorCode, CdpResult};
pub use launch::{
    default_args, reserve_free_port, LaunchOptions, ManagedBrowser, DEFAULT_LAUNCH_TIMEOUT,
};
pub use modifiers::modifier_mask;
pub use page::{Page, ResolvedRef, DEFAULT_VIEWPORT, LEFT_BUTTON_HELD};
pub use snapshot::{
    ax_nodes_in_dom_order, build_snapshot, normalize_text, parse_ax_tree, BrowserElementRect,
    BrowserSnapshot, BrowserSnapshotDomNode, BrowserSnapshotElement, Geometry, RawAxNode,
    ResolvedGeometry, SnapshotOptions, DEFAULT_MAX_DOM_NODES, DEFAULT_MAX_ELEMENTS,
};
pub use transport::{CdpConnection, CdpEvent, EventReceiver, DEFAULT_COMMAND_TIMEOUT};

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {
        assert!(true);
    }
}
