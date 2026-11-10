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

/// Handoff skeleton (pure data/types): serde persistence structs, port trait, Send/Sync markers.
pub mod handoff;

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
    use super::*;

    /// A duplicate registration is a protocol error (two owners for one channel
    /// would split dispatch); the second `register` must fail.
    #[test]
    fn duplicate_channel_registration_fails() {
        struct Noop;
        #[async_trait::async_trait]
        impl Channel for Noop {
            async fn call(
                &self,
                _ctx: &ChannelContext,
                _command: &str,
                _args: Vec<nc_code_rpc::WireValue>,
                _cancel: CancelToken,
            ) -> Result<nc_code_rpc::WireValue, ChannelFailure> {
                Ok(nc_code_rpc::WireValue::Undefined)
            }
        }
        let registry = ChannelRegistry::new();
        let channel: std::sync::Arc<dyn Channel> = std::sync::Arc::new(Noop);
        registry.register("dup", channel.clone()).expect("first");
        assert!(registry.get("dup").is_some());
        assert!(
            registry.register("dup", channel).is_err(),
            "second must fail"
        );
    }
}
