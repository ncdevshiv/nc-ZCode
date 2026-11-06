//! 反向请求（permission / userInput）的登记表与协议投影。
//!
//! 职责边界（单一所有者）：
//! - 本模块只拥有「待应答交互」的事实与其 `pendingInteractionSchema` 投影；
//! - 不发送帧、不等待应答 —— 编排在 `host_interactions`（投影）与
//!   `turn.rs`（工具执行处等待）里完成，因此可独立单测（pluggable）。
//!
//! 语义对齐 TS interaction-broker：
//! - 先到先得：`resolve` 移除登记项并返回结果；晚到/未知 id → None（幂等成功）；
//! - `snooze` 只对带自动收敛倒计时的交互生效（当前只由 userInput 产生），
//!   未命中返回 false（幂等）。

use serde_json::{json, Value};

/// `pendingInteractionSchema.kind`（workspaceHookReview 未在本 host 实现）。
pub const KIND_PERMISSION: &str = "permission";
pub const KIND_USER_INPUT: &str = "userInput";

/// permission 选项闭集（`permissionOptionSchema.kind`）。
pub const OPTION_ALLOW_ONCE: &str = "allowOnce";
pub const OPTION_ALLOW_ALWAYS: &str = "allowAlways";
pub const OPTION_DENY: &str = "deny";
pub const OPTION_CUSTOM: &str = "custom";
/// `PERMISSION_FULL_ACCESS_OPTION_ID`：授予本会话后续全部非只读调用。
pub const OPTION_FULL_ACCESS: &str = "fullAccess";

#[derive(Debug, Clone)]
pub struct PendingInteraction {
    pub id: String,
    pub kind: &'static str,
    /// 关联的 toolCall 行；None = 会话级。
    pub anchor_row_id: Option<u64>,
    pub created_at: i64,
    pub payload: Value,
    /// `interactionAutoResolutionSchema`（仅 userInput 倒计时）。
    pub auto_resolution: Option<Value>,
}

/// The `questions` array of an `AskUserQuestion` input, in the v4 product-projection
/// shape. Unknown/malformed entries are passed through so the tool's own validation
/// reports them instead of silently dropping a question.
pub fn questions_payload(input: &Value) -> Vec<Value> {
    input
        .get("questions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

impl PendingInteraction {
    pub fn projection(&self) -> Value {
        let mut value = json!({
            "interactionId": self.id,
            "kind": self.kind,
            "anchorRowId": self.anchor_row_id,
            "createdAt": self.created_at,
            "payload": self.payload,
        });
        if let Some(auto) = &self.auto_resolution {
            value["autoResolution"] = auto.clone();
        }
        value
    }
}

/// `resolve` 的结果（登记项已被移除；调用方据此放行/拒绝/回填）。
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    Permission {
        allowed: bool,
        /// 用户选择了 `fullAccess`：本会话后续非只读调用自动允许。
        grant_full_access: bool,
        free_text: Option<String>,
    },
    UserInput {
        free_text: Option<String>,
        content: Option<Value>,
    },
}

#[derive(Debug, Default)]
pub struct InteractionRegistry {
    items: Vec<PendingInteraction>,
    next_seq: u64,
}

impl InteractionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn list_projection(&self) -> Value {
        Value::Array(
            self.items
                .iter()
                .map(PendingInteraction::projection)
                .collect(),
        )
    }

    pub fn get(&self, interaction_id: &str) -> Option<&PendingInteraction> {
        self.items.iter().find(|item| item.id == interaction_id)
    }

    fn next_id(&mut self, prefix: &str) -> String {
        self.next_seq += 1;
        format!("{prefix}-{}", self.next_seq)
    }

    /// 打开 permission 交互；返回 interactionId。`allow_full_access` = 附带
    /// `fullAccessOption`（会话已授权时不再提供）。
    #[allow(clippy::too_many_arguments)]
    pub fn open_permission(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        summary: &str,
        detail: &Value,
        anchor_row_id: Option<u64>,
        created_at: i64,
        allow_full_access: bool,
    ) -> String {
        let id = self.next_id("interaction");
        let mut payload = json!({
            "kind": KIND_PERMISSION,
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            "summary": summary,
            "detail": detail,
            "options": [
                {"optionId": "allowOnce", "label": "Allow once", "kind": OPTION_ALLOW_ONCE},
                {"optionId": "deny", "label": "Deny", "kind": OPTION_DENY},
            ],
        });
        if allow_full_access {
            payload["fullAccessOption"] = json!({
                "optionId": OPTION_FULL_ACCESS,
                "label": "Always allow in this session",
                "kind": OPTION_CUSTOM,
            });
        }
        self.items.push(PendingInteraction {
            id: id.clone(),
            kind: KIND_PERMISSION,
            anchor_row_id,
            created_at,
            payload,
            auto_resolution: None,
        });
        id
    }

    /// Open a userInput interaction with a fully-formed product payload (the
    /// AskUserQuestion / plan-approval shapes need structured `questions`, not just
    /// a single prompt).
    pub fn open_user_input_payload(
        &mut self,
        payload: Value,
        anchor_row_id: Option<u64>,
        created_at: i64,
        auto_resolution: Option<Value>,
    ) -> String {
        let id = self.next_id("interaction");
        self.items.push(PendingInteraction {
            id: id.clone(),
            kind: KIND_USER_INPUT,
            anchor_row_id,
            created_at,
            payload,
            auto_resolution,
        });
        id
    }

    /// 打开 userInput 交互（AskUserQuestion / plan 审批）。
    pub fn open_user_input(
        &mut self,
        prompt: &str,
        free_text: bool,
        options: Option<Vec<(String, String)>>,
        anchor_row_id: Option<u64>,
        created_at: i64,
        auto_resolution: Option<Value>,
    ) -> String {
        let id = self.next_id("interaction");
        let mut payload = json!({
            "kind": KIND_USER_INPUT,
            "prompt": prompt,
            "freeText": free_text,
        });
        if let Some(options) = options {
            payload["options"] = Value::Array(
                options
                    .into_iter()
                    .map(|(option_id, label)| json!({"optionId": option_id, "label": label}))
                    .collect(),
            );
        }
        self.items.push(PendingInteraction {
            id: id.clone(),
            kind: KIND_USER_INPUT,
            anchor_row_id,
            created_at,
            payload,
            auto_resolution,
        });
        id
    }

    /// 先到先得：移除并返回结果；未知/已应答 → None（调用方按幂等成功收口）。
    pub fn resolve(&mut self, interaction_id: &str, answer: &Value) -> Option<Resolution> {
        let position = self
            .items
            .iter()
            .position(|item| item.id == interaction_id)?;
        let item = self.items.remove(position);
        let option_id = answer.get("optionId").and_then(Value::as_str);
        let free_text = answer
            .get("freeText")
            .and_then(Value::as_str)
            .map(str::to_string);
        match item.kind {
            KIND_PERMISSION => {
                let allowed = matches!(option_id, Some(OPTION_ALLOW_ONCE | OPTION_FULL_ACCESS));
                Some(Resolution::Permission {
                    allowed,
                    grant_full_access: option_id == Some(OPTION_FULL_ACCESS),
                    free_text,
                })
            }
            _ => Some(Resolution::UserInput {
                free_text,
                content: answer.get("content").cloned(),
            }),
        }
    }

    /// 直接移除登记项（取消/关闭路径）。
    pub fn remove(&mut self, interaction_id: &str) -> Option<PendingInteraction> {
        let position = self
            .items
            .iter()
            .position(|item| item.id == interaction_id)?;
        Some(self.items.remove(position))
    }

    /// Current auto-resolution state for an interaction (`None` = no countdown/unknown).
    pub fn auto_resolution(&self, interaction_id: &str) -> Option<Value> {
        self.items
            .iter()
            .find(|item| item.id == interaction_id)
            .and_then(|item| item.auto_resolution.clone())
    }

    /// Replace the auto-resolution payload (hiddenGrace → visibleCountdown).
    pub fn set_auto_resolution(&mut self, interaction_id: &str, value: Value) -> bool {
        let Some(item) = self.items.iter_mut().find(|item| item.id == interaction_id) else {
            return false;
        };
        item.auto_resolution = Some(value);
        true
    }

    /// 只对带自动收敛倒计时的交互生效；未命中/无倒计时 → false（幂等）。
    pub fn snooze(&mut self, interaction_id: &str, now: i64) -> bool {
        let Some(item) = self.items.iter_mut().find(|item| item.id == interaction_id) else {
            return false;
        };
        let Some(auto) = &item.auto_resolution else {
            return false;
        };
        let started_at = auto.get("startedAt").and_then(Value::as_i64).unwrap_or(now);
        item.auto_resolution = Some(json!({
            "state": "snoozed",
            "startedAt": started_at,
            "snoozedAt": now,
        }));
        true
    }

    /// 取消全部待应答（turn 取消/会话关闭时使用），返回被移除的 interactionId。
    pub fn clear(&mut self) -> Vec<String> {
        self.items.drain(..).map(|item| item.id).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn open(registry: &mut InteractionRegistry) -> String {
        registry.open_permission(
            "toolu_1",
            "Write",
            "Write native-e2e.txt",
            &json!({"file_path": "native-e2e.txt"}),
            Some(7),
            1_700_000_000_000,
            true,
        )
    }

    #[test]
    fn permission_projection_matches_schema_shape() {
        let mut registry = InteractionRegistry::new();
        let id = open(&mut registry);
        let list = registry.list_projection();
        let item = &list[0];
        assert_eq!(item["interactionId"], json!(id));
        assert_eq!(item["kind"], json!("permission"));
        assert_eq!(item["anchorRowId"], json!(7));
        assert_eq!(item["createdAt"], json!(1_700_000_000_000u64));
        assert!(item.get("autoResolution").is_none());
        let payload = &item["payload"];
        assert_eq!(payload["kind"], json!("permission"));
        assert_eq!(payload["toolCallId"], json!("toolu_1"));
        assert_eq!(payload["toolName"], json!("Write"));
        assert_eq!(payload["summary"], json!("Write native-e2e.txt"));
        assert_eq!(payload["detail"]["file_path"], json!("native-e2e.txt"));
        assert_eq!(payload["options"][0]["optionId"], json!("allowOnce"));
        assert_eq!(payload["options"][1]["kind"], json!("deny"));
        assert_eq!(payload["fullAccessOption"]["optionId"], json!("fullAccess"));
    }

    #[test]
    fn resolve_is_first_come_first_served_and_allows_or_denies() {
        let mut registry = InteractionRegistry::new();
        let id = open(&mut registry);
        let allowed = registry
            .resolve(&id, &json!({"optionId": "allowOnce"}))
            .expect("resolved");
        assert_eq!(
            allowed,
            Resolution::Permission {
                allowed: true,
                grant_full_access: false,
                free_text: None
            }
        );
        // 晚到 = 幂等 None
        assert!(registry
            .resolve(&id, &json!({"optionId": "deny"}))
            .is_none());
        assert!(registry.is_empty());

        let id = open(&mut registry);
        let denied = registry
            .resolve(&id, &json!({"optionId": "deny"}))
            .expect("resolved");
        assert_eq!(
            denied,
            Resolution::Permission {
                allowed: false,
                grant_full_access: false,
                free_text: None
            }
        );

        // 未知 option → 拒绝（fail-closed）
        let id = open(&mut registry);
        let unknown = registry
            .resolve(&id, &json!({"optionId": "somethingElse"}))
            .expect("resolved");
        assert_eq!(
            unknown,
            Resolution::Permission {
                allowed: false,
                grant_full_access: false,
                free_text: None
            }
        );
        assert!(registry.resolve("missing", &json!({})).is_none());
    }

    #[test]
    fn full_access_option_grants_session_scope() {
        let mut registry = InteractionRegistry::new();
        let id = open(&mut registry);
        let resolution = registry
            .resolve(&id, &json!({"optionId": "fullAccess"}))
            .expect("resolved");
        assert_eq!(
            resolution,
            Resolution::Permission {
                allowed: true,
                grant_full_access: true,
                free_text: None
            }
        );
    }

    #[test]
    fn snooze_only_applies_to_interactions_with_countdown() {
        let mut registry = InteractionRegistry::new();
        let prompt_id = registry.open_user_input(
            "Pick one",
            true,
            None,
            Some(9),
            100,
            Some(json!({
                "state": "visibleCountdown",
                "startedAt": 100,
                "visibleAt": 200,
                "deadlineAt": 300,
            })),
        );
        assert!(registry.snooze(&prompt_id, 250));
        let item = registry.get(&prompt_id).expect("still pending");
        assert_eq!(
            item.auto_resolution.as_ref().unwrap()["state"],
            json!("snoozed")
        );
        assert_eq!(
            item.auto_resolution.as_ref().unwrap()["snoozedAt"],
            json!(250)
        );
        // 幂等：再次 snooze 仍成功（已是 snoozed）
        assert!(registry.snooze(&prompt_id, 260));

        // 无倒计时的 permission → false
        let permission_id = open(&mut registry);
        assert!(!registry.snooze(&permission_id, 270));
        assert!(!registry.snooze("missing", 280));
    }

    #[test]
    fn user_input_resolution_carries_free_text_and_content() {
        let mut registry = InteractionRegistry::new();
        let id = registry.open_user_input(
            "Name?",
            true,
            Some(vec![("opt-a".into(), "A".into())]),
            None,
            1,
            None,
        );
        let resolution = registry
            .resolve(
                &id,
                &json!({"freeText": "custom", "content": {"answer": "x"}}),
            )
            .expect("resolved");
        assert_eq!(
            resolution,
            Resolution::UserInput {
                free_text: Some("custom".to_string()),
                content: Some(json!({"answer": "x"})),
            }
        );
    }

    #[test]
    fn clear_returns_all_pending_ids() {
        let mut registry = InteractionRegistry::new();
        let first = open(&mut registry);
        let second = open(&mut registry);
        let cleared = registry.clear();
        assert_eq!(cleared, vec![first, second]);
        assert!(registry.is_empty());
    }
}
