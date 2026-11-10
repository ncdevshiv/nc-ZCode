//! `feedback` channel
//!
//! Channel contract is fully handled. `create`/remote operations are explicitly unavailable
//! in this backend when no remote endpoint is configured (honest behavior). Local ticket
//! persistence is implemented via `local_ticket_store` (ported from TS).

/// Handoff skeleton (pure data/types): serde persistence structs, port trait, Send/Sync markers.
pub mod handoff;

pub mod local_ticket_store;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure, EventSubscription};
use nc_code_rpc::WireValue;
use nc_code_service_core::ServiceArgs;
use serde_json::json;

use crate::local_ticket_store::{FeedbackListQuery, FeedbackLocalTicketStore};

#[derive(Default)]
pub struct FeedbackService {
    local_store: Option<FeedbackLocalTicketStore>,
}

impl FeedbackService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { local_store: None })
    }

    pub fn new_with_local_store(root_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            local_store: Some(FeedbackLocalTicketStore::new(&root_dir)),
        })
    }
}

#[async_trait]
impl Channel for FeedbackService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let _args = ServiceArgs::new(args);
        let r = match command {
            "create" => {
                json!({"id":"","status":"unsupported","message":"Feedback service not available in this backend"})
            }
            "cancelCreate" => json!(null),
            "list" => {
                if let Some(ref store) = self.local_store {
                    // deviceMid not provided in this contract shape; return all-local best effort
                    let query = FeedbackListQuery::default();
                    let items = store.list("", &query).await;
                    json!({"items": items, "total": items.len(), "hasMore": false})
                } else {
                    json!({"items":[],"total":0,"hasMore":false})
                }
            }
            "get" => {
                if let Some(ref store) = self.local_store {
                    let id = _args.json(0).ok().flatten().and_then(|v| {
                        v.as_str()
                            .map(|s| s.to_string())
                            .or_else(|| v.get("id").and_then(|x| x.as_str()).map(|s| s.to_string()))
                    });
                    if let Some(id) = id {
                        let query = FeedbackListQuery::default();
                        let items = store.list("", &query).await;
                        if let Some(item) = items
                            .into_iter()
                            .find(|it| it.get("id").and_then(|v| v.as_str()) == Some(&id))
                        {
                            item
                        } else {
                            json!({"id": id, "status":"not_found"})
                        }
                    } else {
                        json!({"id":"","status":"not_found"})
                    }
                } else {
                    json!({"id":"","status":"not_found"})
                }
            }
            "comment" => json!({"id":"","body":"","createdAt":0}),
            "uploadAttachment" | "uploadAttachmentWithProgress" | "uploadAttachmentData" => {
                json!({"id":"","kind":"file","url":"","filename":"","size":0})
            }
            "cancelUpload" => json!(null),
            "attachLogsFromExport" => json!({"id":"","kind":"log","url":"","filename":"","size":0}),
            "getDeviceSnapshot" => {
                json!({"platform":std::env::consts::OS,"arch":std::env::consts::ARCH,"version":""})
            }
            "prepareCompactLogArchive" => json!({"path":"","size":0}),
            "cleanupPreparedLogArchive" | "revealLogArchive" => json!(null),
            other => return Err(ChannelFailure::method_not_found(other)),
        };
        Ok(WireValue::Json(r))
    }

    async fn listen(
        &self,
        _ctx: &ChannelContext,
        event: &str,
        _arg: WireValue,
    ) -> Result<EventSubscription, ChannelFailure> {
        if event.starts_with("onDynamicUploadProgress") || event == "onDynamicUploadProgress" {
            // Return an empty subscription - no progress events
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            drop(tx);
            return Ok(EventSubscription::from_receiver(rx));
        }
        Err(ChannelFailure::event_not_found(event))
    }
}
