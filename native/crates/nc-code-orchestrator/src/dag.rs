//! DAG validation and readiness (spec: specs/nc-ag-orchestration.md section 4).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::error::{NcagError, Result};
use crate::types::Task;

/// Validate a task graph: every dependency must exist, no self-dependency, no cycle.
pub fn validate_graph(tasks: &[Task]) -> Result<()> {
    let ids: BTreeSet<&str> = tasks.iter().map(|task| task.id.as_str()).collect();
    let mut edges: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for task in tasks {
        if let Some(blocker) = task
            .depends_on
            .iter()
            .find(|dep| !ids.contains(dep.as_str()))
        {
            return Err(NcagError::invalid(format!(
                "task {} depends on unknown task {blocker}",
                task.id
            )));
        }
        if task.depends_on.iter().any(|dep| dep == &task.id) {
            return Err(NcagError::invalid(format!(
                "task {} depends on itself",
                task.id
            )));
        }
        edges.insert(
            task.id.as_str(),
            task.depends_on.iter().map(String::as_str).collect(),
        );
    }
    // Kahn's algorithm, deterministic ordering (BTreeMap).
    let mut indegree: BTreeMap<&str, usize> = edges.keys().map(|id| (*id, 0)).collect();
    for deps in edges.values() {
        for dep in deps {
            if let Some(count) = indegree.get_mut(dep) {
                *count += 0;
            }
        }
    }
    for (id, deps) in &edges {
        indegree.insert(*id, deps.len());
    }
    let mut queue: VecDeque<&str> = indegree
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(id, _)| *id)
        .collect();
    let mut visited = 0usize;
    while let Some(next) = queue.pop_front() {
        visited += 1;
        for (id, deps) in &edges {
            if deps.contains(&next) {
                let count = indegree.get_mut(*id).expect("node exists");
                *count -= 1;
                if *count == 0 {
                    queue.push_back(*id);
                }
            }
        }
    }
    if visited != edges.len() {
        let cycle_nodes: Vec<&str> = indegree
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(id, _)| *id)
            .collect();
        return Err(NcagError::invalid(format!(
            "dependency cycle detected among: {}",
            cycle_nodes.join(", ")
        )));
    }
    Ok(())
}

/// Longest-path weight from a node to any sink; used for critical-path priority.
pub fn critical_path_weights(tasks: &[Task]) -> BTreeMap<String, i64> {
    let mut weights: BTreeMap<String, i64> = BTreeMap::new();
    let by_id: BTreeMap<&str, &Task> = tasks.iter().map(|task| (task.id.as_str(), task)).collect();
    fn weight(
        id: &str,
        by_id: &BTreeMap<&str, &Task>,
        memo: &mut BTreeMap<String, i64>,
        visiting: &mut BTreeSet<String>,
    ) -> i64 {
        if let Some(value) = memo.get(id) {
            return *value;
        }
        if !visiting.insert(id.to_string()) {
            return 0; // cycle guard (validate_graph rejects cycles first)
        }
        let Some(task) = by_id.get(id) else {
            return 0;
        };
        let mut depth = 0;
        for dep in &task.depends_on {
            depth = depth.max(1 + weight(dep, by_id, memo, visiting));
        }
        visiting.remove(id);
        memo.insert(id.to_string(), depth);
        depth
    }
    for task in tasks {
        let value = weight(task.id.as_str(), &by_id, &mut weights, &mut BTreeSet::new());
        weights.insert(task.id.clone(), value);
    }
    weights
}

/// A task may run when every dependency completed and the project is not paused.
pub fn is_ready(task: &Task, completed: &BTreeSet<String>, blocked: &BTreeSet<String>) -> bool {
    !task.status.is_terminal()
        && !matches!(
            task.status,
            crate::types::TaskStatus::Running | crate::types::TaskStatus::Assigned
        )
        && task.depends_on.iter().all(|dep| completed.contains(dep))
        && !task.depends_on.iter().any(|dep| blocked.contains(dep))
}

/// Topological order that respects dependency order (deterministic).
pub fn topological_order(tasks: &[Task]) -> Vec<String> {
    let mut by_id: BTreeMap<&str, &Task> = BTreeMap::new();
    for task in tasks {
        by_id.insert(task.id.as_str(), task);
    }
    let mut done: BTreeSet<String> = BTreeSet::new();
    let mut order: Vec<String> = Vec::new();
    while order.len() < tasks.len() {
        let mut progressed = false;
        for (id, task) in &by_id {
            if done.contains(*id) {
                continue;
            }
            if task.depends_on.iter().all(|dep| done.contains(dep)) {
                done.insert((*id).to_string());
                order.push((*id).to_string());
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    order
}

/// Total-order priority: user priority, critical path depth, deadline urgency, fan-out.
pub fn priority_score(task: &Task, critical: i64, blocking_count: i64, now_ms: i64) -> i64 {
    let deadline = task
        .deadline_at
        .map(|at| ((at - now_ms) / 60_000).max(0))
        .map(|minutes| (24 * 60 - minutes.min(24 * 60)) / 10)
        .unwrap_or(0);
    i64::from(task.priority) * 100 + critical * 30 + deadline * 2 + blocking_count * 5
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Task, TaskStatus};

    fn task(id: &str, deps: &[&str]) -> Task {
        Task {
            id: id.to_string(),
            project_id: "p".into(),
            request_id: "r".into(),
            requirement_id: None,
            title: id.into(),
            description: String::new(),
            capability: "code".into(),
            role: "backend".into(),
            agent_id: None,
            status: TaskStatus::Planned,
            priority: 1,
            depends_on: deps.iter().map(|dep| dep.to_string()).collect(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            acceptance: Vec::new(),
            resources: Vec::new(),
            max_retries: 2,
            attempts: 0,
            timeout_ms: 60_000,
            deadline_at: None,
            worktree: None,
            branch: None,
            result: None,
            blocker: None,
            created_at: 0,
            updated_at: 0,
            started_at: None,
            ended_at: None,
        }
    }

    #[test]
    fn valid_graph_passes_and_cycle_is_rejected() {
        let ok = vec![task("a", &[]), task("b", &["a"]), task("c", &["a", "b"])];
        assert!(validate_graph(&ok).is_ok());
        let cycle = vec![task("a", &["b"]), task("b", &["a"])];
        assert!(validate_graph(&cycle).is_err());
        let missing = vec![task("a", &["ghost"])];
        assert!(validate_graph(&missing).is_err());
    }

    #[test]
    fn critical_path_is_depth_of_dependencies() {
        let tasks = vec![task("a", &[]), task("b", &["a"]), task("c", &["b"])];
        let weights = critical_path_weights(&tasks);
        assert_eq!(weights["a"], 0);
        assert_eq!(weights["b"], 1);
        assert_eq!(weights["c"], 2);
    }

    #[test]
    fn readiness_requires_completed_deps() {
        let tasks = [task("a", &[]), task("b", &["a"])];
        let mut completed = BTreeSet::new();
        assert!(is_ready(&tasks[0], &completed, &BTreeSet::new()));
        assert!(!is_ready(&tasks[1], &completed, &BTreeSet::new()));
        completed.insert("a".to_string());
        assert!(is_ready(&tasks[1], &completed, &BTreeSet::new()));
    }
}
