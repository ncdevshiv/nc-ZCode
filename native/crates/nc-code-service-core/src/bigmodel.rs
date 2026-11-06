//! BigModel coding-plan entitlement/team-plan helpers (port of
//! `packages/services/src/bigmodel/codingPlanEntitlement.ts` and `teamPlanApiKey.ts`).
//!
//! HTTP I/O is abstracted behind traits so the decision logic is fully unit-testable.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BigModelTeamPlanBizContext {
    pub organization_id: String,
    pub project_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BigModelTeamPlanApiKeySummary {
    pub api_key: Option<String>,
    pub key_type: Option<i64>,
    pub name: Option<String>,
}

pub const BIGMODEL_TEAM_PLAN_API_KEY_NAME: &str = "zcode-team-api-key";
pub const BIGMODEL_TEAM_PLAN_API_KEY_TYPE: i64 = 2;

pub fn is_usable_bigmodel_team_plan_api_key(item: &BigModelTeamPlanApiKeySummary) -> bool {
    item.name.as_deref() == Some(BIGMODEL_TEAM_PLAN_API_KEY_NAME)
        && item.key_type == Some(BIGMODEL_TEAM_PLAN_API_KEY_TYPE)
        && item
            .api_key
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
}

pub fn create_bigmodel_biz_headers(
    authorization: &str,
    team_context: Option<&BigModelTeamPlanBizContext>,
) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Authorization".to_string(), authorization.to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
    ];
    if let Some(ctx) = team_context {
        headers.push((
            "bigmodel-organization".to_string(),
            ctx.organization_id.clone(),
        ));
        headers.push(("bigmodel-project".to_string(), ctx.project_id.clone()));
    }
    headers
}

pub fn is_successful_bigmodel_biz_envelope(envelope: &Value) -> bool {
    if envelope.get("success").and_then(|v| v.as_bool()) == Some(false) {
        return false;
    }
    if let Some(code) = envelope.get("code").and_then(|v| v.as_i64()) {
        return code == 0 || code == 200;
    }
    envelope.get("success").and_then(|v| v.as_bool()) == Some(true)
        || envelope.get("data").map(|v| !v.is_null()).unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodingPlanEntitlement<T> {
    Available { subscription: T },
    Unavailable { reason: Option<String> },
    Unknown,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PersonalCodingPlanSubscription {
    pub product_id: Option<String>,
    pub product_name: Option<String>,
    pub status: Option<String>,
    pub in_current_period: Option<bool>,
    pub auto_renew: Option<Value>,
    pub next_renew_time: Option<String>,
    pub valid: Option<String>,
    pub billing_cycle: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TeamCodingPlanSubscription {
    pub has_subscription: bool,
    pub status: Option<String>,
    pub member_grant_status: Option<String>,
    pub product_id: Option<String>,
    pub product_name: Option<String>,
    pub subscribe_end_time: Option<String>,
    pub subscribe_period: Option<String>,
}

pub fn is_coding_plan_product(product_id: Option<&str>, product_name: Option<&str>) -> bool {
    [product_id, product_name]
        .into_iter()
        .flatten()
        .any(|f| f.to_lowercase().contains("coding"))
}

pub fn is_active_personal_coding_plan(sub: &PersonalCodingPlanSubscription) -> bool {
    is_coding_plan_product(sub.product_id.as_deref(), sub.product_name.as_deref())
        && sub.status.as_deref() == Some("VALID")
        && sub.in_current_period == Some(true)
}

pub fn read_successful_data(payload: &Value) -> Option<Value> {
    let obj = payload.as_object()?;
    if obj.get("success").and_then(|v| v.as_bool()) == Some(false) {
        return None;
    }
    if let Some(code) = obj.get("code").and_then(|v| v.as_i64()) {
        if code != 0 && code != 200 {
            return None;
        }
    }
    obj.get("data").cloned()
}

pub fn classify_team_coding_plan(
    subscription: &TeamCodingPlanSubscription,
) -> CodingPlanEntitlement<TeamCodingPlanSubscription> {
    if !subscription.has_subscription {
        return CodingPlanEntitlement::Unavailable { reason: None };
    }
    if subscription.status.as_deref() == Some("EXPIRED") {
        return CodingPlanEntitlement::Unavailable {
            reason: Some("expired".into()),
        };
    }
    if subscription.status.as_deref() == Some("EFFECTIVE")
        && subscription.member_grant_status.as_deref() == Some("UNASSIGNED")
    {
        return CodingPlanEntitlement::Unavailable {
            reason: Some("unassigned".into()),
        };
    }
    if subscription.status.as_deref() != Some("EFFECTIVE")
        || subscription.member_grant_status.as_deref() != Some("VALID")
    {
        return CodingPlanEntitlement::Unknown;
    }
    CodingPlanEntitlement::Available {
        subscription: subscription.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn personal_active_requires_coding_valid_in_period() {
        let sub = PersonalCodingPlanSubscription {
            product_id: Some("coding-pro".into()),
            status: Some("VALID".into()),
            in_current_period: Some(true),
            ..Default::default()
        };
        assert!(is_active_personal_coding_plan(&sub));
    }

    #[test]
    fn team_entitlement_classification() {
        let sub = TeamCodingPlanSubscription {
            has_subscription: false,
            ..Default::default()
        };
        assert!(matches!(
            classify_team_coding_plan(&sub),
            CodingPlanEntitlement::Unavailable { reason: None }
        ));
        let sub = TeamCodingPlanSubscription {
            has_subscription: true,
            status: Some("EXPIRED".into()),
            ..Default::default()
        };
        assert!(
            matches!(classify_team_coding_plan(&sub), CodingPlanEntitlement::Unavailable { reason: Some(r) } if r == "expired")
        );
        let sub = TeamCodingPlanSubscription {
            has_subscription: true,
            status: Some("EFFECTIVE".into()),
            member_grant_status: Some("VALID".into()),
            ..Default::default()
        };
        assert!(matches!(
            classify_team_coding_plan(&sub),
            CodingPlanEntitlement::Available { .. }
        ));
    }

    #[test]
    fn envelope_helpers() {
        assert!(is_successful_bigmodel_biz_envelope(
            &json!({"code":200,"data":{}})
        ));
        assert!(!is_successful_bigmodel_biz_envelope(
            &json!({"success":false})
        ));
        assert_eq!(
            read_successful_data(&json!({"code":0,"data":[1]})),
            Some(json!([1]))
        );
    }

    #[test]
    fn usable_team_api_key() {
        let item = BigModelTeamPlanApiKeySummary {
            api_key: Some("k".into()),
            key_type: Some(2),
            name: Some(BIGMODEL_TEAM_PLAN_API_KEY_NAME.into()),
        };
        assert!(is_usable_bigmodel_team_plan_api_key(&item));
    }
}
