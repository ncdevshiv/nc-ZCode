//! HTTP API and SSE stream for NC-AG (spec: specs/nc-ag-orchestration.md section 8).
//!
//! Routes are mounted ahead of the server's static fallback so unknown `/api/nc-ag/*`
//! paths return JSON, never HTML. Every handler performs a real operation and returns
//! either the operation's result or a typed `{error:{code,message}}`.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::NcagError;
use crate::events::EventHub;
use crate::orchestrator::Orchestrator;
use crate::types::DecisionStatus;
use crate::Store;

#[derive(Clone)]
pub struct ApiState {
    pub orchestrator: Arc<Orchestrator>,
    pub store: Arc<Store>,
    pub hub: Arc<EventHub>,
    pub token: Option<String>,
}

type ApiResult<T> = std::result::Result<T, ApiError>;

pub struct ApiError(NcagError);

impl From<NcagError> for ApiError {
    fn from(error: NcagError) -> Self {
        Self(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.0.code.as_str() {
            "not_found" => StatusCode::NOT_FOUND,
            "invalid_request" | "serialization_error" => StatusCode::BAD_REQUEST,
            "conflict" | "user_decision_required" | "illegal_transition" => StatusCode::CONFLICT,
            "permission_denied" | "command_not_allowlisted" => StatusCode::FORBIDDEN,
            "provider_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(self.0.to_json())).into_response()
    }
}

#[derive(Deserialize, Default)]
struct PageQuery {
    #[serde(default)]
    after_id: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
    // The dashboard uses camelCase; snake_case is accepted for curl ergonomics.
    #[serde(default, alias = "projectId")]
    project_id: Option<String>,
}

#[derive(Deserialize)]
struct CreateProjectBody {
    name: String,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default, alias = "baseBranch")]
    base_branch: Option<String>,
}

#[derive(Deserialize)]
struct SubmitRequestBody {
    /// Accepted for client symmetry; the project id in the path is authoritative.
    #[serde(default, rename = "projectId")]
    _project_id: Option<String>,
    instruction: String,
}

#[derive(Deserialize, Default)]
struct ResolveDecisionBody {
    resolution: String,
    #[serde(default)]
    rationale: Option<String>,
    #[serde(default, alias = "decidedBy")]
    decided_by: Option<String>,
}

#[derive(Deserialize, Default)]
struct MessageBody {
    #[serde(alias = "projectId")]
    project_id: String,
    #[serde(default, alias = "taskId")]
    task_id: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    sender: Option<String>,
    #[serde(default)]
    recipient: Option<String>,
    subject: String,
    body: String,
    #[serde(default)]
    requires_ack: bool,
}

#[derive(Deserialize, Default)]
struct InstructBody {
    instruction: String,
}

#[derive(Deserialize, Default)]
struct HelpBody {
    capability: String,
    problem: String,
}

#[derive(Deserialize, Default)]
struct HandoffBody {
    to_agent: String,
    #[serde(default)]
    note: String,
}

#[derive(Deserialize, Default)]
struct ApprovalBody {
    approved: bool,
    #[serde(default)]
    note: Option<String>,
}

/// Build the NC-AG router. When `state.token` is set, every request needs `?token=`
/// or `Authorization: Bearer <token>`; the check is a real middleware layer, not a helper
/// that callers may forget.
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/api/nc-ag/health", get(health))
        .route("/api/nc-ag/config", get(config_view))
        .route("/api/nc-ag/profiles", get(profiles_view))
        .route("/api/nc-ag/dashboard", get(dashboard))
        .route(
            "/api/nc-ag/projects",
            get(list_projects).post(create_project),
        )
        .route("/api/nc-ag/projects/{id}", get(get_project))
        .route(
            "/api/nc-ag/projects/{id}/requests",
            get(list_requests).post(submit_request),
        )
        .route("/api/nc-ag/projects/{id}/pause", post(pause_project))
        .route("/api/nc-ag/projects/{id}/resume", post(resume_project))
        .route("/api/nc-ag/projects/{id}/delivery", get(project_delivery))
        .route("/api/nc-ag/requests/{id}", get(get_request))
        .route("/api/nc-ag/requests/{id}/approve", post(approve_plan))
        .route("/api/nc-ag/requests/{id}/delivery", get(get_delivery))
        .route("/api/nc-ag/tasks", get(list_tasks))
        .route("/api/nc-ag/tasks/{id}", get(get_task).delete(cancel_task))
        .route("/api/nc-ag/tasks/{id}/dag", get(task_dag))
        .route("/api/nc-ag/tasks/{id}/evidence", get(task_evidence))
        .route("/api/nc-ag/tasks/{id}/reassign", post(reassign_task))
        .route("/api/nc-ag/tasks/{id}/handoff", post(handoff_task))
        .route("/api/nc-ag/agents", get(list_agents))
        .route("/api/nc-ag/agents/{id}", get(get_agent))
        .route("/api/nc-ag/agents/{id}/pause", post(pause_agent))
        .route("/api/nc-ag/agents/{id}/resume", post(resume_agent))
        .route("/api/nc-ag/agents/{id}/cancel", post(cancel_agent))
        .route("/api/nc-ag/agents/{id}/instruct", post(instruct_agent))
        .route("/api/nc-ag/agents/{id}/help", post(help_agent))
        .route("/api/nc-ag/messages", get(list_messages).post(send_message))
        .route("/api/nc-ag/messages/{id}/ack", post(ack_message))
        .route("/api/nc-ag/decisions", get(list_decisions))
        .route("/api/nc-ag/decisions/{id}/resolve", post(resolve_decision))
        .route("/api/nc-ag/leases", get(list_leases))
        .route("/api/nc-ag/leases/waiters", get(list_waiters))
        .route("/api/nc-ag/leases/{id}", delete(release_lease))
        .route("/api/nc-ag/integrations", get(list_integrations))
        .route("/api/nc-ag/usage", get(usage_view))
        .route("/api/nc-ag/events", get(list_events))
        .route("/api/nc-ag/events/stream", get(event_stream))
        .route("/api/nc-ag/audit", get(list_audit))
        .route("/api/nc-ag/audit/verify", get(verify_audit))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            token_middleware,
        ))
        .with_state(state)
}

/// Reject a request that does not present the configured token. `NCAG_API_TOKEN` is
/// optional; when absent the middleware is a pass-through.
async fn token_middleware(State(state): State<ApiState>, request: Request, next: Next) -> Response {
    if state.token.is_none() {
        return next.run(request).await;
    }
    let query_token = request
        .uri()
        .query()
        .and_then(|query| {
            query
                .split('&')
                .find_map(|pair| pair.strip_prefix("token="))
        })
        .map(percent_decode);
    let header_token = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .map(str::to_string);
    let presented = query_token
        .or(header_token)
        .filter(|value| !value.is_empty());
    if presented.as_deref() == state.token.as_deref() {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": {
                "code": "unauthorized",
                "message": "this NC-AG service requires a token: pass ?token=... or Authorization: Bearer ...",
            }
        })),
    )
        .into_response()
}

/// Minimal percent decoding for the token query value (`+` and `%XX`).
fn percent_decode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '+' => out.push(' '),
            '%' => {
                let hi = chars.next().and_then(|c| c.to_digit(16));
                let lo = chars.next().and_then(|c| c.to_digit(16));
                match (hi, lo) {
                    (Some(hi), Some(lo)) => {
                        out.push(char::from_u32(hi * 16 + lo).unwrap_or('?'));
                    }
                    _ => out.push('%'),
                }
            }
            other => out.push(other),
        }
    }
    out
}

async fn health(State(state): State<ApiState>) -> ApiResult<Json<Value>> {
    let audit_ok = state.store.verify_audit_chain()?.is_none();
    let provider = state.orchestrator.provider_status();
    Ok(Json(json!({
        "status": "ok",
        "component": "nc-ag",
        "auditChainVerified": audit_ok,
        "inFlight": state.orchestrator.in_flight(),
        "provider": provider,
        "database": state.store.path().to_string_lossy(),
    })))
}

async fn config_view(State(state): State<ApiState>) -> ApiResult<Json<Value>> {
    let config = &state.orchestrator.config;
    Ok(Json(json!({
        "workspaceRoot": config.workspace_root,
        "dataDir": config.data_dir,
        "budgets": config.budgets,
        "policy": config.policy,
        "model": config.model,
    })))
}

async fn profiles_view(State(state): State<ApiState>) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({ "profiles": state.orchestrator.profiles.all() }),
    ))
}

async fn dashboard(State(state): State<ApiState>) -> ApiResult<Json<Value>> {
    Ok(Json(state.orchestrator.dashboard()?))
}

async fn list_projects(State(state): State<ApiState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "projects": state.store.projects()? })))
}

async fn create_project(
    State(state): State<ApiState>,
    Json(body): Json<CreateProjectBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let project = state.orchestrator.create_project(
        &body.name,
        body.workspace.as_deref(),
        body.base_branch.as_deref(),
    )?;
    Ok((StatusCode::CREATED, Json(json!({ "project": project }))))
}

async fn get_project(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let project = state.store.project(&id)?;
    Ok(Json(json!({
        "project": project,
        "requests": state.store.requests_for_project(&id)?,
        "repoFacts": state.orchestrator.repo_facts(&project),
    })))
}

async fn list_requests(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({ "requests": state.store.requests_for_project(&id)? }),
    ))
}

async fn submit_request(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<SubmitRequestBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let request = state
        .orchestrator
        .submit_request(&id, &body.instruction)
        .await?;
    let tasks = state.store.tasks_for_request(&request.id)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "request": request,
            "tasks": tasks,
            "requirements": state.store.requirements_for_request(&request.id)?,
        })),
    ))
}

async fn get_request(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let request = state.store.request(&id)?;
    Ok(Json(json!({
        "request": request,
        "tasks": state.store.tasks_for_request(&id)?,
        "requirements": state.store.requirements_for_request(&id)?,
        "decisions": state.store.open_decisions()?.into_iter().filter(|d| d.request_id.as_deref() == Some(id.as_str())).collect::<Vec<_>>(),
    })))
}

async fn approve_plan(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<ApprovalBody>,
) -> ApiResult<Json<Value>> {
    let request = state
        .orchestrator
        .approve_plan(&id, body.approved, body.note.as_deref())?;
    // An approval unblocks execution immediately; the loop also picks it up.
    let _ = state.orchestrator.tick().await;
    Ok(Json(json!({ "request": request })))
}

async fn get_delivery(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({ "delivery": state.orchestrator.delivery(&id)? }),
    ))
}

async fn project_delivery(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let requests = state.store.requests_for_project(&id)?;
    let latest = requests.last().map(|request| request.id.clone());
    let delivery = match latest {
        Some(request_id) => Some(state.orchestrator.delivery(&request_id)?),
        None => None,
    };
    Ok(Json(json!({ "projectId": id, "delivery": delivery })))
}

async fn pause_project(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({ "project": state.orchestrator.pause_project(&id, true)? }),
    ))
}

async fn resume_project(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    // Resuming re-checks the budget; a still-exhausted budget keeps the project paused.
    let project = state.orchestrator.pause_project(&id, false)?;
    let _ = state.orchestrator.tick().await;
    Ok(Json(json!({ "project": project })))
}

async fn list_tasks(
    State(state): State<ApiState>,
    Query(query): Query<PageQuery>,
) -> ApiResult<Json<Value>> {
    let tasks = match query.project_id {
        Some(project) => state.store.tasks_for_project(&project)?,
        None => state.store.all_tasks()?,
    };
    Ok(Json(json!({ "tasks": tasks })))
}

async fn get_task(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let task = state.store.task(&id)?;
    Ok(Json(json!({
        "task": task,
        "evidence": state.store.evidence_for_task(&id)?,
        "agent": task.agent_id.as_ref().map(|agent| state.store.agent(agent)).transpose()?,
    })))
}

async fn task_dag(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let task = state.store.task(&id)?;
    let tasks = state.store.tasks_for_project(&task.project_id)?;
    let critical = crate::dag::critical_path_weights(&tasks);
    let nodes: Vec<Value> = tasks
        .iter()
        .map(|item| {
            json!({
                "id": item.id,
                "title": item.title,
                "status": item.status.as_str(),
                "role": item.role,
                "priority": item.priority,
                "criticalPathWeight": critical.get(&item.id).copied().unwrap_or(0),
                "dependsOn": item.depends_on,
                "agentId": item.agent_id,
            })
        })
        .collect();
    Ok(Json(
        json!({ "requestId": task.request_id, "nodes": nodes }),
    ))
}

async fn task_evidence(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({ "evidence": state.store.evidence_for_task(&id)? }),
    ))
}

async fn cancel_task(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({ "task": state.orchestrator.cancel_task(&id)? }),
    ))
}

async fn reassign_task(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<HandoffBody>,
) -> ApiResult<Json<Value>> {
    let task = state.orchestrator.reassign_task(&id, &body.note)?;
    Ok(Json(json!({ "task": task })))
}

async fn handoff_task(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<HandoffBody>,
) -> ApiResult<Json<Value>> {
    let task = state
        .orchestrator
        .handoff(&id, &body.to_agent, &body.note)?;
    Ok(Json(json!({ "task": task })))
}

async fn list_agents(
    State(state): State<ApiState>,
    Query(query): Query<PageQuery>,
) -> ApiResult<Json<Value>> {
    let agents = match query.project_id {
        Some(project) => state.store.agents_for_project(&project)?,
        None => state.store.all_agents()?,
    };
    Ok(Json(json!({ "agents": agents })))
}

async fn get_agent(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let agent = state.store.agent(&id)?;
    Ok(Json(json!({
        "agent": agent,
        "messages": state.store.messages_for_agent(&agent.project_id, &id)?,
        "task": agent.task_id.as_ref().map(|task| state.store.task(task)).transpose()?,
    })))
}

async fn pause_agent(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    state.orchestrator.pause_agent(&id)?;
    Ok(Json(json!({ "agent": state.store.agent(&id)? })))
}

async fn resume_agent(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    state.orchestrator.resume_agent(&id)?;
    Ok(Json(json!({ "agent": state.store.agent(&id)? })))
}

async fn cancel_agent(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    state.orchestrator.cancel_agent(&id)?;
    Ok(Json(json!({ "agent": state.store.agent(&id)? })))
}

async fn instruct_agent(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<InstructBody>,
) -> ApiResult<Json<Value>> {
    state.orchestrator.instruct_agent(&id, &body.instruction)?;
    Ok(Json(json!({ "ok": true })))
}

async fn help_agent(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<HelpBody>,
) -> ApiResult<Json<Value>> {
    let agent = state.store.agent(&id)?;
    let Some(task_id) = agent.task_id.clone() else {
        return Err(NcagError::invalid("agent has no task").into());
    };
    let helper = state.orchestrator.request_help_route(
        &agent.project_id,
        &task_id,
        &id,
        &body.capability,
        &body.problem,
    )?;
    Ok(Json(json!({ "helper": helper })))
}

async fn list_messages(
    State(state): State<ApiState>,
    Query(query): Query<PageQuery>,
) -> ApiResult<Json<Value>> {
    let messages = match query.project_id {
        Some(project) => state
            .store
            .messages_for_project(&project, query.limit.unwrap_or(200))?,
        None => {
            let mut all = Vec::new();
            for project in state.store.projects()? {
                all.extend(state.store.messages_for_project(&project.id, 100)?);
            }
            all.sort_by_key(|message| message.created_at);
            all.into_iter()
                .rev()
                .take(query.limit.unwrap_or(200) as usize)
                .collect()
        }
    };
    Ok(Json(json!({ "messages": messages })))
}

async fn send_message(
    State(state): State<ApiState>,
    Json(body): Json<MessageBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let kind = body
        .kind
        .as_deref()
        .and_then(crate::types::MessageKind::parse)
        .unwrap_or(crate::types::MessageKind::Information);
    let recipient = body
        .recipient
        .clone()
        .unwrap_or_else(|| crate::messaging::project_channel(&body.project_id));
    let message = crate::messaging::NewMessage {
        project_id: body.project_id.clone(),
        task_id: body.task_id,
        kind,
        sender: body.sender.unwrap_or_else(|| "user".to_string()),
        channel: recipient.clone(),
        recipient,
        subject: body.subject.clone(),
        body: crate::security::redact(&body.body),
        refs: Vec::new(),
        correlation_id: uuid::Uuid::new_v4().to_string(),
        priority: 2,
        requires_ack: body.requires_ack,
        dedupe_key: None,
    };
    let sent = crate::messaging::send(&state.store, message)?;
    Ok((StatusCode::CREATED, Json(json!({ "message": sent }))))
}

async fn ack_message(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let acked = state.store.ack_message(&id, "user")?;
    Ok(Json(json!({ "acknowledged": acked })))
}

async fn list_decisions(State(state): State<ApiState>) -> ApiResult<Json<Value>> {
    let decisions = state.store.all_decisions()?;
    let open: Vec<_> = decisions
        .iter()
        .filter(|d| d.status == DecisionStatus::Open)
        .collect();
    Ok(Json(json!({ "decisions": decisions, "open": open })))
}

async fn resolve_decision(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<ResolveDecisionBody>,
) -> ApiResult<Json<Value>> {
    let decided_by = body.decided_by.as_deref().unwrap_or("user");
    let decision = crate::decisions::resolve(
        &state.store,
        &state.hub,
        &id,
        &body.resolution,
        body.rationale.as_deref(),
        decided_by,
    )?;
    // Executing requests were waiting on this; recalculate readiness.
    if let Some(request_id) = &decision.request_id {
        state.orchestrator.refresh_request_status(request_id)?;
    }
    Ok(Json(json!({ "decision": decision })))
}

async fn list_leases(
    State(state): State<ApiState>,
    Query(query): Query<PageQuery>,
) -> ApiResult<Json<Value>> {
    let leases = match query.project_id {
        Some(project) => state.store.leases_for_project(&project)?,
        None => state.store.active_leases()?,
    };
    Ok(Json(json!({ "leases": leases })))
}

async fn list_waiters(
    State(state): State<ApiState>,
    Query(query): Query<PageQuery>,
) -> ApiResult<Json<Value>> {
    let project = query
        .project_id
        .ok_or_else(|| NcagError::invalid("projectId is required"))?;
    let waiters = crate::leases::LeaseManager::new(&state.store, 0).waiters(&project)?;
    Ok(Json(json!({ "waiters": waiters })))
}

async fn release_lease(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    state.store.release_lease(&id, "released by user")?;
    Ok(Json(json!({ "released": true })))
}

async fn list_integrations(
    State(state): State<ApiState>,
    Query(query): Query<PageQuery>,
) -> ApiResult<Json<Value>> {
    let integrations = match query.project_id {
        Some(project) => state.store.integrations_for_project(&project)?,
        None => {
            let mut all = Vec::new();
            for project in state.store.projects()? {
                all.extend(state.store.integrations_for_project(&project.id)?);
            }
            all
        }
    };
    Ok(Json(json!({ "integrations": integrations })))
}

async fn usage_view(
    State(state): State<ApiState>,
    Query(query): Query<PageQuery>,
) -> ApiResult<Json<Value>> {
    let project = query
        .project_id
        .ok_or_else(|| NcagError::invalid("projectId is required"))?;
    // An unknown project is a 404 (not a silent empty report) so a typo is visible.
    state.store.project(&project)?;
    Ok(Json(
        crate::budget::BudgetManager::new(&state.store).report(&project)?,
    ))
}

async fn list_events(
    State(state): State<ApiState>,
    Query(query): Query<PageQuery>,
) -> ApiResult<Json<Value>> {
    let events = state
        .store
        .events_after(query.after_id.unwrap_or(0), query.limit.unwrap_or(200))?
        .into_iter()
        .filter(|event| {
            query
                .project_id
                .as_ref()
                .map(|project| event.project_id.as_deref() == Some(project.as_str()))
                .unwrap_or(true)
        })
        .collect::<Vec<_>>();
    Ok(Json(
        json!({ "events": events, "latestId": state.store.latest_event_id()? }),
    ))
}

async fn event_stream(
    State(state): State<ApiState>,
) -> Sse<impl futures_util::Stream<Item = std::result::Result<Event, Infallible>>> {
    let receiver = state.hub.subscribe();
    // Replay the newest persisted events first so a late subscriber sees current context.
    let backlog = state.store.events_after(0, 50).unwrap_or_default();
    let stream = async_stream_from(receiver, backlog);
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

fn async_stream_from(
    receiver: tokio::sync::broadcast::Receiver<crate::types::Event>,
    backlog: Vec<crate::types::Event>,
) -> impl futures_util::Stream<Item = std::result::Result<Event, Infallible>> {
    use futures_util::StreamExt;
    // The backlog is emitted first so a late subscriber sees current context, then the
    // stream follows the hub. `unfold` owns the receiver and never blocks the runtime.
    let backlog_events: Vec<std::result::Result<Event, Infallible>> = backlog
        .into_iter()
        .rev()
        .take(20)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|event| Ok(sse_event(&event)))
        .collect();
    let live = futures_util::stream::unfold(receiver, |mut receiver| async move {
        loop {
            match receiver.recv().await {
                Ok(event) => return Some((Ok(sse_event(&event)), receiver)),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    futures_util::stream::iter(backlog_events).chain(live)
}

/// One SSE frame. Deliberately **unnamed**: `kind` already travels inside the JSON body,
/// and a named `event:` line is invisible to `EventSource.onmessage` (the browser only
/// dispatches it to `addEventListener(<name>)`). The dashboard's live link therefore
/// received nothing while the frames looked correct on the wire (observed live: the
/// stream badge stayed "connecting" through a whole run).
fn sse_event(event: &crate::types::Event) -> Event {
    Event::default()
        .id(event.id.to_string())
        .data(serde_json::to_string(event).unwrap_or_else(|_| "{}".to_string()))
}

async fn list_audit(State(state): State<ApiState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "entries": state.store.recent_audit(200)? })))
}

async fn verify_audit(State(state): State<ApiState>) -> ApiResult<Json<Value>> {
    let broken = state.store.verify_audit_chain()?;
    Ok(Json(json!({
        "verified": broken.is_none(),
        "firstBrokenId": broken,
        "entries": state.store.audit_len()?,
    })))
}

/// Token comparison used by tests and by the middleware above.
pub fn require_token(state: &ApiState, query_token: Option<&str>, header: Option<&str>) -> bool {
    let Some(expected) = &state.token else {
        return true;
    };
    let presented = query_token
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            header
                .and_then(|value| value.strip_prefix("Bearer "))
                .map(str::trim)
                .map(str::to_string)
        });
    presented.as_deref() == Some(expected.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    /// The dashboard reads frames with `EventSource.onmessage`, which only fires for
    /// **unnamed** frames. A named `event:` line is delivered to
    /// `addEventListener(<kind>)` instead — i.e. nowhere. The live dashboard stayed
    /// "connecting" through an entire run while the wire looked correct, so this asserts
    /// the real HTTP bytes of the stream endpoint.
    #[tokio::test]
    async fn the_event_stream_sends_unnamed_frames_that_onmessage_can_receive() {
        let store = std::sync::Arc::new(crate::Store::in_memory().expect("store"));
        store
            .insert_project("p", "P", "G:/w", "main", None, &Default::default())
            .expect("project");
        let hub = std::sync::Arc::new(crate::events::EventHub::new());
        crate::events::emit(
            &store,
            &hub,
            Some("p"),
            None,
            None,
            "task.assigned",
            serde_json::json!({ "role": "backend" }),
        )
        .expect("emit");
        let runtime = crate::NcagRuntime {
            store: std::sync::Arc::clone(&store),
            hub: std::sync::Arc::clone(&hub),
            orchestrator: std::sync::Arc::new(crate::Orchestrator::new(
                std::sync::Arc::clone(&store),
                std::sync::Arc::clone(&hub),
                crate::config::NcagConfig::load(None, |_| None).expect("config"),
                None,
            )),
            supervisor: std::sync::Arc::new(crate::Supervisor::new(
                std::sync::Arc::new(crate::Orchestrator::new(
                    std::sync::Arc::clone(&store),
                    std::sync::Arc::clone(&hub),
                    crate::config::NcagConfig::load(None, |_| None).expect("config"),
                    None,
                )),
                std::sync::Arc::clone(&store),
                std::sync::Arc::clone(&hub),
            )),
            config: crate::config::NcagConfig::load(None, |_| None).expect("config"),
        };
        let router = router(runtime.api_state());
        let request = axum::http::Request::builder()
            .uri("/api/nc-ag/events/stream")
            .body(axum::body::Body::empty())
            .expect("request");
        let response = tower::ServiceExt::oneshot(router, request)
            .await
            .expect("response");
        assert_eq!(response.status(), 200);
        // Read the replayed backlog only: the live tail never completes.
        let mut body = response.into_body();
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), body.frame())
            .await
            .expect("a frame within the deadline")
            .expect("a frame")
            .expect("ok");
        let bytes = chunk.into_data().unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes).to_string();
        assert!(
            !text.contains("event:"),
            "a named event is invisible to onmessage: {text}"
        );
        assert!(
            text.contains("\"kind\":\"task.assigned\""),
            "the kind travels inside the JSON body: {text}"
        );
        assert!(text.contains("data:"), "{text}");
    }
}
