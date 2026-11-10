# NC-AG — API reference

Base path: `/api/nc-ag`. All responses are JSON with camelCase keys. Errors use
`{"error": {"code": "...", "message": "...", "detail?": ...}}` and these status codes:

| code | HTTP |
| --- | --- |
| `invalid_request`, `serialization_error` | 400 |
| `not_found` | 404 |
| `conflict`, `user_decision_required`, `illegal_transition` | 409 |
| `permission_denied`, `command_not_allowlisted` | 403 |
| `provider_unavailable` | 503 |
| anything else (`internal`, `storage_error`, `git_failed`, …) | 500 |

When `NCAG_API_TOKEN` is set, every request must carry `?token=<token>` or
`Authorization: Bearer <token>`.

## Health & configuration

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/health` | liveness + audit-chain verification + provider wiring + DB path |
| GET | `/config` | effective workspace, budgets, policy, model selection |
| GET | `/profiles` | the 16 registered agent profiles |
| GET | `/dashboard` | per-project counters, usage, open decisions, in-flight, recent events |

`GET /health` sample:

```json
{"status":"ok","component":"nc-ag","auditChainVerified":true,"inFlight":0,
 "provider":{"configured":true,"note":"resolution happens per dispatch …"},
 "database":"…/.ncag/ncag.sqlite"}
```

## Projects

| Method | Path | Body | Result |
| --- | --- | --- | --- |
| GET | `/projects` | — | `{projects: Project[]}` |
| POST | `/projects` | `{name, workspace?, baseBranch?}` | 201 `{project}` |
| GET | `/projects/{id}` | — | `{project, requests, repoFacts}` |
| GET | `/projects/{id}/requests` | — | `{requests}` |
| POST | `/projects/{id}/requests` | `{instruction}` | 201 `{request, tasks, requirements}` |
| POST | `/projects/{id}/pause` | — | `{project}` (status `paused`) |
| POST | `/projects/{id}/resume` | — | `{project}` (status `active`) |
| GET | `/projects/{id}/delivery` | — | `{projectId, delivery}` of the latest request |

`workspace` defaults to `NCAG_WORKSPACE_ROOT`. The directory must exist **and be a git
repository**, otherwise `invalid_request` names the reason. Creating a project also
creates its base branch (`ncag/main` by default) in that repository.

## Requests

| Method | Path | Body | Result |
| --- | --- | --- | --- |
| GET | `/requests/{id}` | — | `{request, tasks, requirements, decisions}` |
| POST | `/requests/{id}/approve` | `{approved, note?}` | `{request}` (only from `awaiting_approval`) |
| GET | `/requests/{id}/delivery` | — | `{delivery}` |

`request.status` is one of `received, analyzing, planned, awaiting_approval,
waiting_provider, executing, completed, failed, cancelled`.

## Tasks

| Method | Path | Body | Result |
| --- | --- | --- | --- |
| GET | `/tasks?projectId=` | — | `{tasks}` |
| GET | `/tasks/{id}` | — | `{task, evidence, agent}` |
| GET | `/tasks/{id}/dag` | — | `{requestId, nodes[]}` (id, title, status, role, priority, criticalPathWeight, dependsOn) |
| GET | `/tasks/{id}/evidence` | — | `{evidence}` |
| DELETE | `/tasks/{id}` | — | `{task}` (cancelled) |
| POST | `/tasks/{id}/reassign` | `{note}` | `{task}` (returns to `ready`) |
| POST | `/tasks/{id}/handoff` | `{toAgent, note}` | `{task}` (handoff recorded + message) |

## Agents

| Method | Path | Body | Result |
| --- | --- | --- | --- |
| GET | `/agents?projectId=` | — | `{agents}` |
| GET | `/agents/{id}` | — | `{agent, messages, task}` |
| POST | `/agents/{id}/pause` | — | `{agent}` |
| POST | `/agents/{id}/resume` | — | `{agent}` |
| POST | `/agents/{id}/cancel` | — | `{agent}` (also cancels the task, releases leases) |
| POST | `/agents/{id}/instruct` | `{instruction}` | `{ok}` (persisted message, ack required) |
| POST | `/agents/{id}/help` | `{capability, problem}` | `{helper}` (spawns a helper within budget) |

## Messages

| Method | Path | Body | Result |
| --- | --- | --- | --- |
| GET | `/messages?projectId=` | — | `{messages}` |
| POST | `/messages` | `{projectId, taskId?, kind?, sender?, recipient?, subject, body, requiresAck?}` | 201 `{message}` |
| POST | `/messages/{id}/ack` | — | `{acknowledged}` |

## Decisions (debate centre)

| Method | Path | Body | Result |
| --- | --- | --- | --- |
| GET | `/decisions` | — | `{decisions, open}` |
| POST | `/decisions/{id}/resolve` | `{resolution, rationale?, decidedBy?}` | `{decision}` |

`resolution` is `recommended` (NC-AG's proposal) or `requested` (the user's original
approach); any other string is stored verbatim as the chosen option. A `material` or
`user_reserved` decision can only be resolved with `decidedBy: "user"` — otherwise
`409 user_decision_required`.

## Leases & integration

| Method | Path | Body | Result |
| --- | --- | --- | --- |
| GET | `/leases?projectId=` | — | `{leases}` |
| GET | `/leases/waiters?projectId=` | — | `{waiters}` (task, resource, holder, expiry, fencing token) |
| DELETE | `/leases/{id}` | — | `{released}` |
| GET | `/integrations?projectId=` | — | `{integrations}` |

## Usage, events, audit

| Method | Path | Result |
| --- | --- | --- |
| GET | `/usage?projectId=` | `{usage: {inputTokens, outputTokens, costUsd, calls}, rows}` (404 for an unknown project) |
| GET | `/events?afterId=&limit=&projectId=` | `{events, latestId}` |
| GET | `/events/stream` | SSE: one `data:` frame per persisted event, `event:` = kind, `id:` = row id |
| GET | `/audit` | `{entries}` (newest 200, with `prevHash`/`hash`) |
| GET | `/audit/verify` | `{verified, firstBrokenId, entries}` |

## Example: a full run with curl

```sh
# 1. create a project over a git repository
curl -sX POST localhost:3030/api/nc-ag/projects \
  -H 'content-type: application/json' \
  -d '{"name":"Demo","workspace":"/path/to/repo"}'

# 2. submit a request (analysis + plan happen inline)
curl -sX POST localhost:3030/api/nc-ag/projects/<projectId>/requests \
  -H 'content-type: application/json' \
  -d '{"instruction":"Add a documented health endpoint"}'

# 3. watch it run
curl -N localhost:3030/api/nc-ag/events/stream
curl -s localhost:3030/api/nc-ag/tasks?projectId=<projectId>

# 4. collect the delivery pack
curl -s localhost:3030/api/nc-ag/requests/<requestId>/delivery

# 5. prove the audit log was not edited
curl -s localhost:3030/api/nc-ag/audit/verify
```
