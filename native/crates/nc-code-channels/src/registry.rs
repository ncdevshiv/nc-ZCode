//! Channel registry and the handler trait.
//!
//! **Argument shape:** the frontend's `ProxyChannel.toService` always calls
//! `channel.call(method, [...args])`, so handlers receive a positional argument array.
//! Direct channel users (tests, raw clients) may send `undefined` for a no-argument
//! call; both are valid wire values and the service-layer kit normalizes them.
//!
//! **Events:** `ProxyChannel` treats `on[A-Z]…` members as events (`listen(name)`) and
//! `onDynamic[A-Z]…` as dynamic events (`listen(name, arg)`). Channels return an
//! [`EventSubscription`] from [`Channel::listen`]; everything else is a [`Channel::call`].

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use nc_code_rpc::WireValue;

use crate::error::ChannelFailure;
use crate::events::{CancelToken, EventSubscription};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientMode {
    /// Browser / remote client: replayable delivery profile.
    WebRemoteReplayable,
    /// Trusted desktop host connection: continuous delivery profile.
    DesktopContinuous,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelContext {
    pub connection_id: String,
    pub client_mode: ClientMode,
}

impl ChannelContext {
    pub fn new(connection_id: impl Into<String>, client_mode: ClientMode) -> Self {
        Self {
            connection_id: connection_id.into(),
            client_mode,
        }
    }
}

#[async_trait]
pub trait Channel: Send + Sync {
    async fn call(
        &self,
        ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure>;

    async fn listen(
        &self,
        _ctx: &ChannelContext,
        event: &str,
        _arg: WireValue,
    ) -> Result<EventSubscription, ChannelFailure> {
        Err(ChannelFailure::event_not_found(event))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateChannel {
    pub name: String,
}

impl std::fmt::Display for DuplicateChannel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "channel already registered: {}", self.name)
    }
}

impl std::error::Error for DuplicateChannel {}

/// Process-wide channel map. Registration happens once during composition, before
/// connections are accepted; failed registration is a startup error, never silent.
#[derive(Default)]
pub struct ChannelRegistry {
    channels: RwLock<HashMap<String, Arc<dyn Channel>>>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &self,
        name: impl Into<String>,
        channel: Arc<dyn Channel>,
    ) -> Result<(), DuplicateChannel> {
        let name = name.into();
        let mut channels = self
            .channels
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if channels.contains_key(&name) {
            return Err(DuplicateChannel { name });
        }
        channels.insert(name, channel);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Channel>> {
        self.channels
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(name)
            .cloned()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.channels
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(name)
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .channels
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    pub fn len(&self) -> usize {
        self.channels
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Empty;

    #[async_trait]
    impl Channel for Empty {
        async fn call(
            &self,
            _ctx: &ChannelContext,
            command: &str,
            _args: Vec<WireValue>,
            _cancel: CancelToken,
        ) -> Result<WireValue, ChannelFailure> {
            Err(ChannelFailure::method_not_found(command))
        }
    }

    #[test]
    fn duplicate_registration_is_an_error() {
        let registry = ChannelRegistry::new();
        registry.register("a", Arc::new(Empty)).expect("first");
        let error = registry
            .register("a", Arc::new(Empty))
            .expect_err("duplicate");
        assert_eq!(error.name, "a");
        assert_eq!(registry.names(), vec!["a".to_string()]);
    }
}
