//! Channel RPC **server** runtime — the Rust counterpart of `ChannelServer` +
//! `ProxyChannel.fromService` in `packages/rpc`.
//!
//! The wire layer lives in [`nc_code_rpc`]; this crate owns what happens after a
//! message is decoded:
//! - a [`ChannelRegistry`] maps channel names to [`Channel`] handlers;
//! - a [`ChannelConnection`] owns one client connection: `Initialize`, request
//!   dispatch, event subscriptions, cancellation and event fan-out;
//! - handlers receive **positional argument arrays** (see [`registry`] docs), because
//!   the current frontend's `ProxyChannel.toService` always sends `[...args]`.
//!
//! Outbound values are channel-message payloads; the transport (`nc-code-server`) wraps
//! them in 13-byte frames. Keeping framing out of this crate lets tests drive the
//! exact bytes the TypeScript client sends without opening sockets.

pub mod connection;
pub mod error;
pub mod events;
pub mod registry;

pub use connection::{ChannelConnection, ProtocolViolation};
pub use error::ChannelFailure;
pub use events::{CancelToken, EventBus, EventSubscription};
pub use registry::{Channel, ChannelContext, ChannelRegistry, ClientMode, DuplicateChannel};

#[cfg(test)]
mod tests {
    #[test]
    fn basic_test() {
        assert!(true);
    }
}
