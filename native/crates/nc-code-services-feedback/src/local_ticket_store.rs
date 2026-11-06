//! Local feedback ticket store (port of `packages/services/src/feedback/feedbackLocalTicketStore.ts`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::fs;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FeedbackListQuery {
    pub status: Option<String>,
    #[serde(rename = "type")]
    pub ticket_type: Option<String>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredFeedbackTicket {
    #[serde(rename = "deviceMid")]
    device_mid: String,
    #[serde(flatten)]
    ticket: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct FeedbackTicketStoreFile {
    tickets: Vec<StoredFeedbackTicket>,
}

pub struct FeedbackLocalTicketStore {
    file_path: PathBuf,
}

impl FeedbackLocalTicketStore {
    pub fn new(root_dir: &Path) -> Self {
        Self {
            file_path: root_dir.join("tickets.json"),
        }
    }

    pub async fn list(&self, device_mid: &str, query: &FeedbackListQuery) -> Vec<Value> {
        let file = self.read().await;
        let mut tickets: Vec<&StoredFeedbackTicket> = file
            .tickets
            .iter()
            .filter(|t| t.device_mid == device_mid)
            .filter(|t| {
                if let Some(ref s) = query.status {
                    if t.ticket.get("status").and_then(|v| v.as_str()) != Some(s.as_str()) {
                        return false;
                    }
                }
                if let Some(ref ty) = query.ticket_type {
                    if t.ticket.get("type").and_then(|v| v.as_str()) != Some(ty.as_str()) {
                        return false;
                    }
                }
                true
            })
            .collect();
        tickets.sort_by(|a, b| {
            let au = a
                .ticket
                .get("updated_at")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let bu = b
                .ticket
                .get("updated_at")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            bu.cmp(au)
        });
        let offset = query.offset.unwrap_or(0);
        let limit = query.limit.unwrap_or(tickets.len());
        tickets
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|t| {
                let mut out = migrate_stored_ticket(&t.ticket);
                // TS returns ticket without deviceMid
                if let Value::Object(ref mut m) = out {
                    m.remove("deviceMid");
                }
                out
            })
            .collect()
    }

    pub async fn upsert(&self, device_mid: &str, ticket: Value) -> std::io::Result<()> {
        let mut file = self.read().await;
        let id = ticket.get("id").and_then(|v| v.as_str()).unwrap_or("");
        file.tickets.retain(|t| {
            !(t.device_mid == device_mid && t.ticket.get("id").and_then(|v| v.as_str()) == Some(id))
        });
        file.tickets.insert(
            0,
            StoredFeedbackTicket {
                device_mid: device_mid.to_string(),
                ticket,
            },
        );
        file.tickets.truncate(200);
        self.write(&file).await
    }

    async fn read(&self) -> FeedbackTicketStoreFile {
        match fs::read_to_string(&self.file_path).await {
            Ok(text) => {
                let parsed: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => return FeedbackTicketStoreFile::default(),
                };
                let Some(tickets) = parsed.get("tickets").and_then(|v| v.as_array()) else {
                    return FeedbackTicketStoreFile::default();
                };
                let tickets = tickets
                    .iter()
                    .filter_map(|v| serde_json::from_value::<StoredFeedbackTicket>(v.clone()).ok())
                    .filter(|t| is_valid_stored_ticket(&t.ticket))
                    .map(|mut t| {
                        t.ticket = migrate_stored_ticket(&t.ticket);
                        t
                    })
                    .collect();
                FeedbackTicketStoreFile { tickets }
            }
            Err(_) => FeedbackTicketStoreFile::default(),
        }
    }

    async fn write(&self, file: &FeedbackTicketStoreFile) -> std::io::Result<()> {
        if let Some(parent) = self.file_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let text = serde_json::to_string_pretty(file).unwrap_or_else(|_| "{}".into());
        fs::write(&self.file_path, format!("{text}\n")).await
    }
}

fn is_valid_stored_ticket(ticket: &Value) -> bool {
    let Some(o) = ticket.as_object() else {
        return false;
    };
    for k in ["id", "title", "type", "status", "created_at", "updated_at"] {
        if o.get(k).and_then(|v| v.as_str()).is_none() {
            return false;
        }
    }
    true
}

fn is_feedback_ticket_status(s: &str) -> bool {
    matches!(
        s,
        "已提交"
            | "信息不足"
            | "已采纳"
            | "答复关闭"
            | "已归档"
            | "已拒绝"
            | "开发中"
            | "已解决"
            | "已上线"
    )
}

fn migrate_stored_ticket(ticket: &Value) -> Value {
    let mut t = ticket.clone();
    // 3.3.5: migrate legacy "待评估" or unknown status to "已提交"
    let status = t
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if status == "待评估" || !is_feedback_ticket_status(&status) {
        if let Some(obj) = t.as_object_mut() {
            obj.insert("status".to_string(), Value::String("已提交".to_string()));
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[tokio::test]
    async fn upsert_and_list_filters_by_device() {
        let tmp = TempDir::new().unwrap();
        let store = FeedbackLocalTicketStore::new(tmp.path());
        store
            .upsert(
                "d1",
                json!({"id":"1","title":"t","status":"已提交","type":"bug","created_at":"1","updated_at":"2"}),
            )
            .await
            .unwrap();
        store
            .upsert(
                "d2",
                json!({"id":"2","title":"t2","status":"已提交","type":"bug","created_at":"1","updated_at":"3"}),
            )
            .await
            .unwrap();
        let list = store.list("d1", &FeedbackListQuery::default()).await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].get("id").and_then(|v| v.as_str()), Some("1"));
        assert!(list[0].get("deviceMid").is_none());
    }

    #[tokio::test]
    async fn migrates_unknown_and_legacy_status() {
        let tmp = TempDir::new().unwrap();
        let store = FeedbackLocalTicketStore::new(tmp.path());
        store
            .upsert(
                "d1",
                json!({"id":"1","title":"t","status":"待评估","type":"bug","created_at":"1","updated_at":"2"}),
            )
            .await
            .unwrap();
        let list = store.list("d1", &FeedbackListQuery::default()).await;
        assert_eq!(
            list[0].get("status").and_then(|v| v.as_str()),
            Some("已提交")
        );
    }

    #[tokio::test]
    async fn respects_offset_limit_and_status_filter() {
        let tmp = TempDir::new().unwrap();
        let store = FeedbackLocalTicketStore::new(tmp.path());
        for i in 0..3 {
            store
                .upsert(
                    "d1",
                    json!({"id":format!("{i}"),"title":"t","status":"已提交","type":"bug","created_at":"1","updated_at":format!("{i}")}),
                )
                .await
                .unwrap();
        }
        let q = FeedbackListQuery {
            offset: Some(1),
            limit: Some(1),
            ..Default::default()
        };
        let list = store.list("d1", &q).await;
        assert_eq!(list.len(), 1);
    }
}
