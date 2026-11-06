//! Controller task-row mapping: `session_view` → `WindowHostControllerTaskRow`.
//!
//! Split from `controller.rs` (RPC state + dispatch) to keep each file under
//! the 400-line limit.

use serde_json::{json, Value};

fn phase_to_status(view: &Value) -> Option<String> {
    match view.get("phase").and_then(Value::as_str) {
        Some("running") | Some("prewarming") | Some("draft") => Some("running".to_string()),
        Some("error") => Some("error".to_string()),
        Some("completedSuccess") | Some("completedInterrupted") => Some("completed".to_string()),
        _ => None,
    }
}

fn live_status(view: &Value) -> &'static str {
    let pending = view.get("pendingInteractions").and_then(Value::as_array);
    if pending.is_some_and(|items| !items.is_empty()) {
        return "waiting";
    }
    match view.get("phase").and_then(Value::as_str) {
        Some("prewarming") | Some("running") => "running",
        Some("error") => "error",
        Some("completedSuccess") | Some("completedInterrupted") => "completed",
        _ => "idle",
    }
}

fn task_mode(view: &Value) -> String {
    let mode = view.get("mode").and_then(Value::as_str).unwrap_or("build");
    match mode {
        "yolo" | "plan" | "edit" | "auto" | "autoEdit" | "build" => mode.to_string(),
        _ => "build".to_string(),
    }
}

/// Build one `WindowHostControllerTaskRow` from a `session_view` value.
pub fn task_row_for_view(
    view: &Value,
    fallback_workspace: &str,
    fallback_identity: Option<&str>,
) -> Value {
    let task_id = view
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let workspace_path = view
        .get("workspacePath")
        .and_then(Value::as_str)
        .unwrap_or(fallback_workspace)
        .to_string();
    let workspace_identity = view
        .get("workspaceIdentity")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| fallback_identity.map(str::to_string));
    let title = view
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let created_at = view.get("createdAt").and_then(Value::as_i64).unwrap_or(0);
    let updated_at = view
        .get("updatedAt")
        .and_then(Value::as_i64)
        .unwrap_or(created_at);
    let mut meta = json!({
        "taskId": task_id,
        "traceId": task_id,
        "title": title,
        "workspacePath": workspace_path,
        "createdAt": created_at,
        "updatedAt": updated_at,
        "mode": task_mode(view),
        "provider": view.get("provider").and_then(Value::as_str).filter(|v| !v.is_empty()).unwrap_or("glm"),
    });
    if let Some(identity) = &workspace_identity {
        meta["workspaceIdentity"] = json!(identity);
    }
    if let Some(model) = view
        .get("model")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        meta["model"] = json!(model);
    }
    if let Some(thought) = view
        .get("thought")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        meta["thoughtLevel"] = json!(thought);
    }
    if let Some(status) = phase_to_status(view) {
        meta["status"] = json!(status);
    }
    let mut address = json!({ "workspacePath": workspace_path, "taskId": task_id });
    if let Some(identity) = &workspace_identity {
        address["workspaceIdentity"] = json!(identity);
    }
    json!({
        "address": address,
        "meta": meta,
        "membership": { "pinned": false, "archived": false, "active": true },
        "sourceAvailability": "online",
        "liveStatus": live_status(view),
    })
}

pub fn controller_tasks_snapshot(server: &crate::HostServer, epoch: &str, seq: u64) -> Vec<Value> {
    let _ = (epoch, seq);
    controller_tasks_snapshot_impl(server)
}

pub fn controller_tasks_snapshot_impl(server: &crate::HostServer) -> Vec<Value> {
    let host_path = server.workspace().to_string_lossy().into_owned();
    let mut rows: Vec<Value> = server
        .session_ids()
        .iter()
        .filter_map(|id| server.session_view(id))
        .filter(|view| {
            view.get("workspacePath")
                .and_then(Value::as_str)
                .unwrap_or("")
                == host_path
                || view.get("workspacePath").is_none()
        })
        .map(|view| task_row_for_view(&view, &host_path, None))
        .collect();
    rows.sort_by(|a, b| {
        let bu = b["meta"]["updatedAt"].as_i64().unwrap_or(0);
        let au = a["meta"]["updatedAt"].as_i64().unwrap_or(0);
        let ka = format!(
            "{}\0{}",
            a["meta"]["workspacePath"].as_str().unwrap_or(""),
            a["meta"]["taskId"].as_str().unwrap_or("")
        );
        let kb = format!(
            "{}\0{}",
            b["meta"]["workspacePath"].as_str().unwrap_or(""),
            b["meta"]["taskId"].as_str().unwrap_or("")
        );
        bu.cmp(&au).then_with(|| ka.cmp(&kb))
    });
    rows
}

pub fn controller_workspaces_snapshot_impl(server: &crate::HostServer) -> Vec<Value> {
    let path = server.workspace().to_string_lossy().into_owned();
    vec![json!({
        "workspacePath": path,
        "sourceAvailability": "online",
        "connectionState": "online",
    })]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn task_row_shape_matches_ts() {
        let view = json!({ "sessionId": "s-1", "workspacePath": "/ws", "workspaceIdentity": "id-1", "title": "Hi", "createdAt": 10, "updatedAt": 20, "phase": "running", "mode": "build", "provider": "glm", "model": "m", "thought": "t" });
        let row = task_row_for_view(&view, "/ws", None);
        assert_eq!(row["address"]["taskId"], json!("s-1"));
        assert_eq!(row["address"]["workspaceIdentity"], json!("id-1"));
        assert_eq!(row["meta"]["taskId"], json!("s-1"));
        assert_eq!(row["meta"]["traceId"], json!("s-1"));
        assert_eq!(row["meta"]["mode"], json!("build"));
        assert_eq!(row["liveStatus"], json!("running"));
        assert_eq!(row["membership"]["pinned"], json!(false));
        assert!(row["address"].get("remoteSessionId").is_none());
    }
    #[test]
    fn task_row_maps_phase_to_status_and_live_status() {
        let view = json!({ "sessionId": "s", "workspacePath": "/ws", "createdAt": 1, "updatedAt": 2, "phase": "completedSuccess", "mode": "plan" });
        let row = task_row_for_view(&view, "/ws", None);
        assert_eq!(row["meta"]["status"], json!("completed"));
        assert_eq!(row["meta"]["mode"], json!("plan"));
        assert_eq!(row["liveStatus"], json!("completed"));
        let view = json!({ "sessionId": "s", "workspacePath": "/ws", "createdAt": 1, "updatedAt": 2, "phase": "idle", "mode": "bogus" });
        let row = task_row_for_view(&view, "/ws", None);
        assert_eq!(row["meta"]["mode"], json!("build"));
        assert_eq!(row["liveStatus"], json!("idle"));
    }
}
