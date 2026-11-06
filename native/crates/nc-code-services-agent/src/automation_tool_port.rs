//! The `AutomationService` → host `AutomationPort` adapter.
//!
//! Dependency direction: host must not depend on `nc-code-services-automation` (the latter depends on host's storage),
//! so host only defines the port trait and this layer (the composition root) injects it. The tool surface's input is wire JSON
//! (the TS zod contract); mapping it onto the domain types is this layer's job, while cron/scheduleRule semantics stay in the service layer.
//!
//! Known difference (recorded in `specs/rust-native-crates.md` S3 "scheduled-task tools"): the TS in-session
//! cron reuses the creator's session; the Rust dispatcher creates a new session for every run (`automation_dispatch`).
//! Therefore `session_id` is currently carried only as an ownership fact and does not change dispatch behaviour.

use std::sync::Arc;

use nc_code_host::tools::AutomationPort;
use nc_code_services_automation::{
    AutomationService, CreateInput, ModelSelection, ScheduleUnit, Scope, UpdateInput,
};
use serde_json::Value;

pub struct AutomationToolPort {
    service: Arc<AutomationService>,
    scope: Scope,
}

impl AutomationToolPort {
    pub fn new(
        service: Arc<AutomationService>,
        workspace_path: String,
        workspace_key: String,
    ) -> Self {
        Self {
            service,
            scope: Scope {
                workspace_path: Some(workspace_path),
                workspace_identity: Some(workspace_key),
            },
        }
    }

    fn schedule_unit(input: &Value) -> Option<ScheduleUnit> {
        match input.get("intervalUnit").and_then(Value::as_str)? {
            "minute" => Some(ScheduleUnit::Minute),
            "hourly" => Some(ScheduleUnit::Hourly),
            "daily" => Some(ScheduleUnit::Daily),
            "weekly" => Some(ScheduleUnit::Weekly),
            "monthly" => Some(ScheduleUnit::Monthly),
            "yearly" => Some(ScheduleUnit::Yearly),
            _ => None,
        }
    }

    /// `provider/model` → `ModelSelection` (same as TS: the model is not decided by the tool's arguments).
    fn model_selection(model: Option<&str>) -> Option<ModelSelection> {
        let (provider_id, model_id) = model?.split_once('/')?;
        Some(ModelSelection {
            provider_id: provider_id.to_string(),
            model_id: model_id.to_string(),
            options: None,
        })
    }

    fn to_json(automation: &nc_code_services_automation::Automation) -> Value {
        serde_json::to_value(automation).unwrap_or(Value::Null)
    }
}

impl AutomationPort for AutomationToolPort {
    fn create(
        &self,
        input: &Value,
        _session_id: &str,
        model: Option<&str>,
    ) -> Result<Value, String> {
        let relative_delay_minutes = input
            .get("delayMinutes")
            .and_then(Value::as_i64)
            .and_then(|value| u32::try_from(value).ok());
        let create = CreateInput {
            title: input
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            cron_expr: input
                .get("cron")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            relative_delay_minutes,
            prompt: input
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            model_selection: Self::model_selection(model),
            mode: None,
            workspace_path: self.scope.workspace_path.clone().unwrap_or_default(),
            workspace_identity: self.scope.workspace_identity.clone(),
            target_task_id: None,
            // a relative delay = one-shot (the TS contract layer already forbids recurring=true; the service layer rejects it once more).
            recurring: relative_delay_minutes.is_none()
                && input
                    .get("recurring")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            max_runs: if relative_delay_minutes.is_some() {
                None
            } else {
                input
                    .get("maxRuns")
                    .and_then(Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok())
            },
            end_at: None,
            schedule_rule: None,
            interval_unit: Self::schedule_unit(input),
            interval: input
                .get("interval")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok()),
        };
        let automation = self
            .service
            .create(&create, &self.scope)
            .map_err(|error| error.to_string())?;
        Ok(Self::to_json(&automation))
    }

    fn list(&self) -> Result<Vec<Value>, String> {
        let automations = self
            .service
            .list(&self.scope)
            .map_err(|error| error.to_string())?;
        Ok(automations.iter().map(Self::to_json).collect())
    }

    fn update(&self, input: &Value) -> Result<Value, String> {
        let id = input
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| "id is required".to_string())?;
        let update = UpdateInput {
            title: input
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string),
            cron_expr: input
                .get("cron")
                .and_then(Value::as_str)
                .map(str::to_string),
            prompt: input
                .get("prompt")
                .and_then(Value::as_str)
                .map(str::to_string),
            model_selection: None,
            mode: None,
            recurring: input.get("recurring").and_then(Value::as_bool),
            max_runs: input
                .get("maxRuns")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .map(Some),
            end_at: None,
            schedule_rule: None,
            interval_unit: Self::schedule_unit(input),
            interval: input
                .get("interval")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok()),
            schedule_edited_by_user: None,
        };
        match self
            .service
            .update(id, &update, &self.scope)
            .map_err(|error| error.to_string())?
        {
            Some(automation) => Ok(Self::to_json(&automation)),
            None => Err(format!(
                "Automation {id} was not found in the current workspace."
            )),
        }
    }

    fn delete(&self, id: &str) -> Result<bool, String> {
        self.service
            .delete(id, &self.scope)
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_services_automation::AutomationRepo;
    use serde_json::json;

    fn port(dir: &std::path::Path) -> AutomationToolPort {
        let repo = AutomationRepo::new(dir.join("tasks.sqlite"));
        AutomationToolPort::new(
            Arc::new(AutomationService::new(Arc::new(repo))),
            dir.to_string_lossy().to_string(),
            "ws-key".to_string(),
        )
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zcode-automation-port-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn create_list_update_delete_round_trip_through_the_real_service() {
        let dir = temp_dir("rt");
        let port = port(&dir);
        let created = port
            .create(
                &json!({"title": "hourly", "prompt": "check", "cron": "0 * * * *"}),
                "session-1",
                Some("anthropic/claude-test"),
            )
            .expect("create");
        let id = created["automationId"].as_str().expect("id").to_string();
        assert_eq!(created["title"], json!("hourly"));
        assert_eq!(created["modelSelection"]["providerId"], json!("anthropic"));

        let listed = port.list().expect("list");
        assert_eq!(listed.len(), 1);

        let updated = port
            .update(&json!({"id": id, "title": "hourly v2"}))
            .expect("update");
        assert_eq!(updated["title"], json!("hourly v2"));

        assert!(port.delete(&id).expect("delete"));
        assert!(port.list().expect("list").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_relative_delay_becomes_a_one_shot_with_a_real_next_run() {
        let dir = temp_dir("delay");
        let port = port(&dir);
        let created = port
            .create(
                &json!({"title": "in 8 minutes", "prompt": "remind", "delayMinutes": 8}),
                "session-1",
                None,
            )
            .expect("create");
        assert_eq!(created["recurring"], json!(false));
        assert!(
            created["nextRunAt"].as_i64().is_some(),
            "the service must compute a concrete next run from its real clock: {created}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_invalid_cron_surfaces_the_service_error() {
        let dir = temp_dir("badcron");
        let port = port(&dir);
        let error = port
            .create(
                &json!({"title": "bad", "prompt": "p", "cron": "not a cron"}),
                "session-1",
                None,
            )
            .expect_err("invalid cron");
        assert!(!error.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
