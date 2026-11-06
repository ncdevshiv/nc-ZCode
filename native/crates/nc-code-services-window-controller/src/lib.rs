//! `window-controller` service channel.
//!
//! Routes task/controller operations to the agent tasks facade. This Rust backend does not yet
//! expose a full window-host controller v4 task facade wiring; operations that require that
//! backend fail closed with an explicit error rather than returning fabricated state.

use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{
    CancelToken, Channel, ChannelContext, ChannelFailure, EventBus, EventSubscription,
};
use nc_code_rpc::WireValue;
use nc_code_service_core::{ServiceArgs, ServiceError};
use serde_json::json;

const NOT_WIRED: &str =
    "window-controller task facade is not wired in this Rust backend; operation unavailable";

pub struct WindowControllerService {
    bus: EventBus,
}

impl WindowControllerService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            bus: EventBus::new(16),
        })
    }

    fn unavailable() -> ChannelFailure {
        ServiceError::named("WindowControllerUnavailable", NOT_WIRED).into()
    }
}

impl Default for WindowControllerService {
    fn default() -> Self {
        Self {
            bus: EventBus::new(16),
        }
    }
}

#[async_trait]
impl Channel for WindowControllerService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let _args = ServiceArgs::new(args);
        match command {
            // Contract-shaped empty list is honest when no tasks are known locally; full facade
            // integration remains pending.
            "listTaskList" => Ok(WireValue::Json(json!({
                "items": [],
                "total": 0,
                "hasMore": false
            }))),
            // Operations requiring a real controller v4 task facade: fail closed.
            "subscribeControllerV4"
            | "resyncControllerV4"
            | "unsubscribeControllerV4"
            | "mutateTask"
            | "deleteArchivedTask"
            | "deleteArchivedTasks" => Err(Self::unavailable()),
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }

    async fn listen(
        &self,
        _ctx: &ChannelContext,
        event: &str,
        _arg: WireValue,
    ) -> Result<EventSubscription, ChannelFailure> {
        match event {
            "onDynamicControllerFrame" => {
                Ok(EventSubscription::from_broadcast(self.bus.subscribe()))
            }
            _ => Err(ChannelFailure::event_not_found(event)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;

    fn ctx() -> ChannelContext {
        ChannelContext::new("window-controller", ClientMode::DesktopContinuous)
    }

    #[tokio::test]
    async fn list_task_list_returns_contract_shaped_empty_result() {
        let service = WindowControllerService::new();
        let result = service
            .call(&ctx(), "listTaskList", vec![], CancelToken::new())
            .await
            .unwrap();
        let WireValue::Json(v) = result else {
            panic!("json")
        };
        assert!(v["items"].is_array());
        assert_eq!(v["total"], 0);
    }

    #[tokio::test]
    async fn facade_operations_fail_closed_honestly() {
        let service = WindowControllerService::new();
        for method in [
            "subscribeControllerV4",
            "resyncControllerV4",
            "mutateTask",
            "deleteArchivedTask",
        ] {
            let err = service
                .call(
                    &ctx(),
                    method,
                    vec![WireValue::Json(json!({}))],
                    CancelToken::new(),
                )
                .await
                .unwrap_err();
            let msg = err.to_string().to_lowercase();
            assert!(
                msg.contains("not wired") || msg.contains("unavailable") || msg.contains("facade"),
                "{method}: {msg}"
            );
        }
    }

    #[tokio::test]
    async fn on_dynamic_controller_frame_listen_returns_subscription() {
        let service = WindowControllerService::new();
        let mut sub = service
            .listen(&ctx(), "onDynamicControllerFrame", WireValue::Undefined)
            .await
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_millis(60), sub.next()).await;
        assert!(result.is_err() || matches!(result, Ok(None)));
    }

    #[tokio::test]
    async fn unknown_method_not_found() {
        let service = WindowControllerService::new();
        let err = service
            .call(&ctx(), "nope", vec![], CancelToken::new())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Method not found"));
    }
}
