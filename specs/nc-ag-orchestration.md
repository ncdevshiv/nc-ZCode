# Spec: NC-AG — Autonomous Multi-Agent Orchestration Platform

Status: **implemented in this checkout** (see "Implementation map" and `docs/nc-ag/*`)
Date: 2026-11-07
Owner: `native/crates/nc-code-orchestrator` (single owner of orchestration state)
Governing specs: `specs/rust-native-crates.md` (crate map, dependency direction),
`specs/rust-cli-provider-boundary.md` (model request/stream semantics),
`specs/typescript-frontend-only.md` (backend stays Rust; UI is frontend TS).

NC-AG is a second product surface inside this application: an autonomous organization of
specialised agents that turns a user request into verified deliverables. Its backend is a
native crate, exposed over the existing axum server and the existing channel-protocol
(`/ws`) transport, and observed through a Web dashboard.

## 1. Product rules (non-negotiable)

1. **Real execution only.** Every stdout/DB row the product shows is produced by a real
   code path. There is no "demo mode", no canned agent answer and no fake progress meter.
   Without a configured model provider, an agent turn fails closed with
   `ProviderNotConfigured`; a request stays `waiting_provider` and says so.
2. **Evidence over assertions.** A task reaches `completed` only when its acceptance
   criteria have machine-checkable evidence (exit code / output assertion / review
   approval / audit finding). Agent-claimed success never completes a task.
3. **User sovereignty with structured challenge.** The orchestrator may disagree with the
   user; it must present the debate record (requested approach, recommendation, evidence,
   pros/cons/risks, explicit decision request). After the user decides, the decision is
   persisted, agents are notified and the plan is recalculated. Settled decisions are not
   reopened without new evidence.
4. **Isolation is mandatory.** Agents never edit the shared workspace. Each task runs in
   its own git worktree; integration is a separate, evidence-producing step.
5. **Least privilege.** Retrieval/tool content is untrusted data. Instruction-like text
   inside repository files, tool output or messages cannot change permissions, system
   policy, or the task list (prompt-injection guard). Secrets are never logged.
6. **Durability.** Orchestration state, task graph, messages, leases, decisions, evidence
   and audit entries survive process restart; recovery resumes work instead of losing it.
7. **Budgeted autonomy.** Token/cost/duration limits are enforced before spend; the
   cheapest suitable model is chosen by policy; exceeding a hard budget pauses the
   project and requests approval.
8. **Truthful reporting.** Unavailable integrations (e.g. unconfigured provider) are
   reported as unavailable, with the exact missing configuration named.

## 2. State owners and single write paths

```text
HTTP/WS API ──commands──▶ Orchestrator (single writer of orchestration state)
                                │
                                ├─▶ Store (SQLite, WAL): projects/tasks/agents/messages/
                                │   leases/decisions/evidence/audit/events/cost
                                ├─▶ Scheduler loop  (task admission from the DAG)
                                ├─▶ AgentRuntime     (provider turn + workspace tools)
                                ├─▶ GitManager      (worktree create/commit/merge)
                                └─▶ EventBus        (in-process broadcast → SSE)
```

- **Store** is the only component that writes SQL. Everything else goes through its API.
- **Orchestrator** is the only writer of task/agent/lease/decision transitions.
- **AgentRuntime** owns one agent's conversation and workspace; its only writes outside
  the worktree are `append_message`, `record_usage` and `heartbeat`.
- **Scheduler loop** is a *driver*: it reads due work and calls Orchestrator commands;
  it never mutates rows directly.
- **UI** reads through HTTP `/api/nc-ag/*` and never writes files, DB or agent state.

Event order (one dispatch): `claim task → mark assigned → create/reuse agent → open
worktree → run turn(s) → submit result → independent verification → review/audit →
integration → completed`, each step appending an audit row before the SSE event.

## 3. Domain model (abridged; `types.rs` is authoritative)

- `.project`: id, name, workspace path, base branch/commit, budgets, policy, status.
- `.request`: id, project, raw instruction, intent analysis, normalized requirements,
  plan approval state, status (`received → analyzing → planned → awaiting_approval /
  waiting_provider → executing → completed | failed`), trace id.
- `.requirement`: id, request, description, category, priority, acceptance criteria,
  verification method, trace back to the raw instruction.
- `.task`: id, request, title/description, required capability, assigned agent, deps
  (DAG), inputs/outputs, acceptance checks, priority, retry policy, deadlines,
  workspace key, status (`created → planned → ready → assigned → running →
  review → verification → completed | rejected | failed | cancelled | blocked`).
- `.agent_profile`: role, persona, expertise, capabilities, tools, permissions,
  restrictions, deliverable contract, escalation rules, model policy.
- `.agent`: instance id, profile, project, task, status (14 lab states), heartbeat,
  worktree, capabilities, usage, health, events.
- `.message`: type (20 types), sender/recipient, project/task, subject, body, refs,
  correlation id, priority, ack requirement/status, delivery state, dedupe key.
- `.lease`: resource path (normalised per-project), mode (shared-read/exclusive-write),
  owner agent/task, fencing token, expires_at, state.
- `.decision`: classification (autonomous / recommendation / material / user-reserved),
  requested approach, recommendation, evidence, pros/cons/risks, status (open /
  resolved / overridden / superseded), resolution + rationale + decided_by.
- `.evidence`: task criterion, kind (command / review / audit / build / api-contract /
  doc / integration), command/artifact ref, result, log excerpt, produced_by.
- `.integration`: source task/commit, base commit, changed files, textual conflict
  list, semantic review state, tests run, outcome, merged commit.
- `.usage`: per agent/task/project tokens + cost with provider/model dimensions.

Agent states: `created, initializing, starting, idle, assigned, working, waiting,
blocked, help_requested, reviewing, handoff_pending, recovering, stopping, stopped,
failed`. Task transitions are validated by `state_machine.rs`; illegal transitions are
errors, not silently coerced.

## 4. Scheduling

- The DAG is validated on creation (cycle, dangling dep, self-dep). Cycles are rejected
  before any agent spawns.
- A task is `ready` iff all `depends_on` tasks are `completed` and no blocking lease
  conflicts exist for its declared resources.
- Independent tasks run concurrently up to `max_concurrent_agents`; priority = user
  priority, critical-path weight, deadline slack, blocking fan-out, then age.
- Pause/cancel propagate: cancelling a task stops its agent, releases its leases, blocks
  dependents's readiness and records why.
- Dependency failure propagates: a task whose dependency is `failed`, `rejected` or
  `cancelled` can never become ready, so it is terminalized (`failed` / `cancelled`
  respectively) with the blocking dependency in its blocker, and the propagation is
  repeated to a fixpoint so grandchildren settle too. A request settles `failed` when any
  task failed, `cancelled` when nothing completed and something was cancelled, and
  `completed` all-terminal otherwise — a request never reports `completed` over work that
  never ran.
- A dispatch that fails before an agent exists (worktree creation, repository error)
  consumes an attempt and is bounded by `max_retries`; it is never retried on every sweep
  forever.
- Replanning (on a user decision or a failed requirement) recalculates readiness and
  emits `plan_recalculated` events.

## 5. Agent execution contract

```text
agent turn: system(policy + profile + context package) → model(stream) → tool calls
            → tool results (permission-checked, workspace-scoped) → … ≤ max_steps
            → submit_result {summary, files, commands_run, limitations}
```

- Tools available to agents: read/list/glob/grep/write/edit within the task worktree,
  and `bash` restricted to an allowlist of test/build commands; every call is audited
  with agent+task ids and a correlation id.
- The context package is built per task: project summary, requirement + acceptance,
  approved architecture decisions, interface contracts, relevant file snippets,
  messages addressed to the agent, and the lease state. Full history is not injected.
- A heartbeat is liveness, not progress. Stuck detection uses repeated identical tool
  calls, repeated command failures, no new artifact for `stall_after`, and error rate.
  Recovery escalates: nudge → mutual-help request → reassign with preserved worktree,
  then fail with evidence.

## 6. Verification, review, integration

1. Developer agent self-check (its stated command list is re-executed for real).
2. Independent verifier agent (never the author) checks each acceptance criterion and
   runs the criterion commands in the task worktree; records `evidence` rows.
3. Reviewer/critiquer/security-auditor verdicts are separate evidence kinds; review
   cycles are bounded by `max_review_cycles`.
4. Integration: changed-file manifest vs base commit; overlap detection; merge attempt
   in a scratch worktree; on textual conflict or semantic incompatibility, a
   `semantic_conflict` discussion is created and escalated (user-reserved if material).
5. Merge only after required evidence is green; then the project branch advances and
   leases are released.

## 7. Security

- Per-agent permission set is enforced by `security.rs` in front of the tool executor
  (`fs.read`, `fs.write`, `shell.exec`, `git.merge`, `net.http`); default deny.
- Worktree isolation + path normalisation: any path escaping the worktree is denied.
- Untrusted-content guard wraps every tool/retrieved text and strips instruction-like
  directives before they reach a system prompt; forbidden actions cannot be enabled by
  content. A malicious-instruction test proves it.
- Credentials come from env/config only; log/audit redaction covers API keys, bearer
  tokens and private keys.
- Destructive operations require explicit permission (`git.merge`, `shell.exec`) and an
  audit entry.
- Audit entries are hash-chained (`prev_hash`) and verifiable (`verify_audit_chain`).

## 8. API surface (all routes are real)

`/api/nc-ag/health`, `/config`, `/projects` (GET/POST), `/projects/{id}`,
`/projects/{id}/requests` (POST), `/requests/{id}`, `/requests/{id}/plan`
(approve/reject), `/requests/{id}/analyze` (retry planning), `/tasks` (GET),
`/tasks/{id}` (GET), `/tasks/{id}/cancel|pause|resume`, `/tasks/{id}/dag`,
`/agents`, `/agents/{id}`, `/agents/{id}/pause|resume|cancel`,
`/agents/{id}/instruct|handoff|reassign`, `/messages` (GET/POST),
`/messages/{id}/ack`, `/decisions`, `/decisions/{id}/resolve`,
`/leases` (GET/DELETE), `/integrations`, `/usage`, `/events` (GET, paged),
`/events/stream` (SSE), `/audit/verify`.

Request/response shapes are JSON with camelCase keys (fern compliance with the existing
frontend conventions); errors are `{ "error": { "code", "message" } }` with stable codes.

`/events/stream` is Server-Sent Events with **unnamed** frames (`id:` + `data:`, the event
`kind` inside the JSON body). The dashboard consumes it with `EventSource.onmessage`, which
only fires for unnamed frames; a named `event:` line is delivered to
`addEventListener(<kind>)` and would be dropped (observed: the live badge stayed
`connecting` for a whole run while the wire looked correct). The same frames are used for
the replay backlog, so a late subscriber's first frames are deliverable too.

## 9. Configuration

`NCAG_*` env plus a JSON config file (`NCAG_CONFIG_FILE`):

```jsonc
{ "workspaceRoot": ".", "model": { "providerId": "...", "modelId": "...", },
  "budgets": { "projectUsd": 25, "taskTokens": 250000, "concurrency": 4,
               "maxAgentsPerTask": 3, "maxRetries": 2, "maxReviewCycles": 2 },
  "policy": { "requirePlanApproval": false, "defaultDecisionClass": "technical_recommendation" } }
```

Model credentials resolve through the existing provider store
(`nc-code-services-provider`, the same single source the host uses); NC-AG never
invents a provider. Missing provider/model ⇒ `provider_unavailable`, surfaced in
health and in the request status.

## 10. Acceptance scenarios (E2E, `tests/e2e_scenarios.rs` unless noted)

A. simple request → plan/assign/execute/verify/deliver; B. full-stack plan with parallel
tasks; C. concurrent projects isolated; D. two tasks on the same file honoured by leases
(second waits, first commits, integration merges); E. killed agent → detection + recovery
+ task continuity; F. looping agent → stuck detection + escalation; G. helper request →
capability discovery + helper spawn; H. user disagreement → debate record + decision
request; I. override → decision persisted, agents notified, plan recalculated; J. review
disagreement bounded by cycles; K. restart → state/tasks/messages/decisions recover;
L. budget exhaustion → pause + pending approval; M. interrupt/resume long task without
duplicate execution (idempotent claim); N. malicious instructions in repo file cannot
widen permissions; O. delivery includes implementation/tests/reviews/limitations.

Scenarios use a real local HTTP provider (axum listener implementing the OpenAI-compatible
SSE contract) so the model transport, streaming, tool loop and token accounting are
exercised for real.

`tests/live_provider_e2e.rs` additionally runs the same orchestration against an
**external** OpenAI-compatible gateway, skipped unless credentials are exported
(`ZCODE_NCAG_LIVE_BASE_URL` / `_API_KEY` / `_MODEL`). It asserts real token accounting
attributed to the configured provider/model, a settled request, evidence for every task,
and the HTTP surface (health/dashboard/tasks/usage) reflecting the same durable state.
Verified against `deepseek-v4.1-flash-eco` on the AMR gateway: request `completed`,
3–4 tasks, ~270k input / ~28k output tokens across ~80 model calls, integration merged,
delivery `completed_with_limitations`.

Live failures are regression-tested rather than just fixed:
- an agent that committed its `target/` build output made the integration worktree fail to
  check out ("Filename too long" on Windows) — build output is now excluded from
  commits/manifests (`git::ARTIFACT_DIRS`) and `core.longpaths=true` is set for every git
  invocation;
- a worktree failure used to be retried forever (no attempt was consumed) and a dependent
  of a failed task stayed `planned` — `record_dispatch_failure` and
  `fail_orphaned_dependents` now settle both;
- a completed-then-repaired task kept its stale blocker/agent error (`completed ·
  recovery exhausted`);
- evidence history was read as state: a superseded review rejection refused the merge of
  an approved task, and a pre-review `FileExists` pass was recorded as a `Review` row —
  which made `has_passing_review` approve a task whose review had actually failed.
  Evidence now carries its own kind (`CriterionResult.kind`), and every reader
  (`has_passing_review`, `failing`, `precheck`) judges the **current** (latest per
  kind+criterion) rows through `verification::current_evidence`;
- the SSE frames were named, so the dashboard's `onmessage` never received them.

All of these have tests in `orchestrator::tests`, `git::tests`, `verification::tests`,
`integration::tests` and `api::tests`, and the browser run is scripted end to end
(`/nc-ag`: create project → submit request → terminal delivery, with screenshots).

## 11. Implementation map

| Concern | Files |
| --- | --- |
| domain types, state machines | `src/types.rs`, `src/state.rs` |
| persistence, migrations, audit | `src/store.rs`, `migrations/0001_init.sql` |
| profiles | `src/profiles.rs` |
| planning (intake → DAG) | `src/planner.rs`, `src/dag.rs` |
| agent turn loop, tools | `src/agent.rs`, `src/tools.rs` |
| model client | `src/provider.rs` |
| scheduler, recovery, health | `src/scheduler.rs`, `src/recovery.rs` |
| leases, messaging, events | `src/leases.rs`, `src/messaging.rs`, `src/events.rs`, `src/audit.rs` |
| git worktrees, merge | `src/git.rs`, `src/integration.rs` |
| review, verification, critique | `src/review.rs`, `src/verification.rs`, `src/decisions.rs` |
| orchestration commands | `src/orchestrator.rs`, `src/supervisor.rs` |
| HTTP API, SSE | `src/api.rs` |
| cost, security, config | `src/budget.rs`, `src/security.rs`, `src/config.rs` |
| UI | `packages/ui/src/nc-ag/*`, route `/nc-ag` in `packages/web/src/main.tsx` |
| docs | `docs/nc-ag/*.md` |
