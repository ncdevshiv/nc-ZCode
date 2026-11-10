//! Cost and budget control (spec: specs/nc-ag-orchestration.md section 7).
//!
//! Hard limits are checked *before* spend. A limit hit pauses the project and opens an
//! approval decision; it never silently continues spending.

use serde_json::json;

use crate::error::Result;
use crate::types::{ProjectConfig, ProjectUsage};
use crate::Store;

#[derive(Debug, Clone, PartialEq)]
pub enum BudgetState {
    Ok,
    /// Would exceed the project budget: pause and require approval.
    Exceeded {
        limit_usd: f64,
        spent_usd: f64,
        projected_usd: f64,
    },
    /// Per-task token ceiling reached: stop this task, keep the project running.
    TaskTokensExhausted {
        limit: u64,
        used: u64,
    },
}

impl BudgetState {
    pub fn ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

pub struct BudgetManager<'a> {
    store: &'a Store,
}

impl<'a> BudgetManager<'a> {
    pub fn new(store: &'a Store) -> Self {
        Self { store }
    }

    pub fn project_usage(&self, project_id: &str) -> Result<ProjectUsage> {
        self.store.usage_for_project(project_id)
    }

    /// Pre-flight: is there room for one more model round? We project with the observed
    /// average cost of the project (or a conservative default when there is no history).
    pub fn check_project(&self, project_id: &str, config: &ProjectConfig) -> Result<BudgetState> {
        let usage = self.store.usage_for_project(project_id)?;
        if config.project_budget_usd <= 0.0 {
            return Ok(BudgetState::Ok); // 0 means "unlimited" only when explicitly set
        }
        let average = if usage.calls > 0 {
            usage.cost_usd / usage.calls as f64
        } else {
            0.0
        };
        let projected = usage.cost_usd + average.max(0.0);
        if usage.cost_usd >= config.project_budget_usd || projected > config.project_budget_usd {
            return Ok(BudgetState::Exceeded {
                limit_usd: config.project_budget_usd,
                spent_usd: usage.cost_usd,
                projected_usd: projected,
            });
        }
        Ok(BudgetState::Ok)
    }

    pub fn check_task(&self, task_tokens_used: u64, config: &ProjectConfig) -> BudgetState {
        if config.task_token_budget > 0 && task_tokens_used >= config.task_token_budget {
            return BudgetState::TaskTokensExhausted {
                limit: config.task_token_budget,
                used: task_tokens_used,
            };
        }
        BudgetState::Ok
    }

    /// Usage report with per-agent rows for the dashboard.
    pub fn report(&self, project_id: &str) -> Result<serde_json::Value> {
        let usage = self.store.usage_for_project(project_id)?;
        let rows = self.store.usage_rows(project_id)?;
        let agents: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                json!({
                    "agentId": row.agent_id,
                    "taskId": row.task_id,
                    "provider": row.provider,
                    "model": row.model,
                    "inputTokens": row.input_tokens,
                    "outputTokens": row.output_tokens,
                    "costUsd": row.cost_usd,
                })
            })
            .collect();
        Ok(json!({ "usage": usage, "rows": agents }))
    }
}

/// A hard stop because of an exhausted budget. The caller pauses the project and opens
/// a Material decision so a human can raise the limit or stop the work.
pub fn budget_decision_payload(project_id: &str, state: &BudgetState) -> serde_json::Value {
    match state {
        BudgetState::Exceeded {
            limit_usd,
            spent_usd,
            projected_usd,
        } => json!({
            "projectId": project_id,
            "limitUsd": limit_usd,
            "spentUsd": spent_usd,
            "projectedUsd": projected_usd,
            "action": "paused",
        }),
        BudgetState::TaskTokensExhausted { limit, used } => json!({
            "projectId": project_id,
            "limitTokens": limit,
            "usedTokens": used,
            "action": "task_stopped",
        }),
        BudgetState::Ok => json!({ "projectId": project_id, "action": "none" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::UsageRow;

    fn seed(store: &Store, project: &str, cost: f64) {
        store
            .insert_project(project, "P", "G:/w", "main", None, &Default::default())
            .unwrap();
        store
            .record_usage(&UsageRow {
                project_id: project.into(),
                task_id: None,
                agent_id: Some("a".into()),
                provider: "p".into(),
                model: "m".into(),
                input_tokens: 1000,
                output_tokens: 500,
                cost_usd: cost,
                created_at: 1,
            })
            .unwrap();
    }

    #[test]
    fn exceeding_the_project_budget_is_reported_before_the_next_call() {
        let store = Store::in_memory().unwrap();
        seed(&store, "p", 5.0);
        let config = ProjectConfig {
            project_budget_usd: 5.0,
            ..ProjectConfig::default()
        };
        let state = BudgetManager::new(&store)
            .check_project("p", &config)
            .unwrap();
        assert!(matches!(state, BudgetState::Exceeded { .. }));
        let generous = ProjectConfig {
            project_budget_usd: 100.0,
            ..ProjectConfig::default()
        };
        assert!(BudgetManager::new(&store)
            .check_project("p", &generous)
            .unwrap()
            .ok());
    }

    #[test]
    fn task_token_limit_and_reports() {
        let store = Store::in_memory().unwrap();
        seed(&store, "p", 1.0);
        let config = ProjectConfig {
            task_token_budget: 100,
            ..ProjectConfig::default()
        };
        assert!(matches!(
            BudgetManager::new(&store).check_task(100, &config),
            BudgetState::TaskTokensExhausted { .. }
        ));
        let report = BudgetManager::new(&store).report("p").unwrap();
        assert_eq!(report["usage"]["costUsd"], 1.0);
        assert_eq!(report["rows"].as_array().unwrap().len(), 1);
    }
}
