# NC-AG — architecture

Status: implemented (`native/crates/nc-code-orchestrator`, `packages/ui/src/nc-ag`)
Governing spec: [`specs/nc-ag-orchestration.md`](../../specs/nc-ag-orchestration.md)

NC-AG is an autonomous multi-agent orchestration platform that turns a user request into
verified deliverables. It is a native Rust service inside ZCode; the dashboard is a
front-end view over its HTTP/SSE API.

## 1. Context

```
user (dashboard / curl / any client)
   │  /api/nc-ag/*  (JSON)           /api/nc-ag/events/stream (SSE)
   ▼
nc-code-server  (axum: channel router + static UI + NC-AG router)
   ▼
NcagRuntime
   ├── Orchestrator    decisions: intake, planning, dispatch, review, verification, delivery
   ├── Supervisor      driver loop: scheduler tick, health sweep, lease/dead-letter sweep
   ├── Store           SQLite (WAL): the only writer of orchestration state
   ├── EventHub        in-process broadcast → SSE
   └── provider store  the existing personal provider registry (one credential truth)
```

The composition is shared by all three entries:

| Entry | How NC-AG is mounted |
| --- | --- |
| `zcode-server` (web/remote) | `ServerOptions.ncag = Some(runtime)`; router merged before the SPA fallback |
| `zcode-shell` (desktop) | same field, runtime built from `AppPaths` (warns and disables if config is invalid) |
| `zcode-ncag` (headless) | standalone axum service, useful for operations and CI |

## 2. Ownership model

| State | Single owner | Everyone else |
| --- | --- | --- |
| tasks, agents, leases, decisions, requests | `Store` (row-level) | reads through store API |
| task/agent/request transitions | `Orchestrator` | never writes status directly |
| agent transcript + worktree | one `AgentRuntime` per agent | reads events/rows |
| worktree commits and integration | `GitRepo` (integration manager) | never merges on its own |
| audit chain | `Store::append_audit` | verifies only |
| provider credentials | provider store (`provider-config-rust-v1.json`) | NC-AG never persists a secret |

Duplicate execution is prevented by `DispatchGuard`: one task id can be in flight once,
and a second dispatch attempt returns "not started" instead of spawning another agent.

## 3. Request lifecycle

```
POST /projects/{id}/requests
  → analyze_request(facts)         deterministic intent/ambiguity/contradiction analysis
  → normalize_requirements         traceable requirements with acceptance checks
  → plan_tasks | model_plan+validate
        model plan is accepted only if it passes the same DAG/capability validation;
        otherwise the rejection is recorded and the deterministic plan is used
  → expose contradictions as Material decisions (execution waits for the user)
  → persist tasks (planned) + plan
  → scheduler tick: ready → assigned → running
       per task: acquire leases → create worktree → spawn/reuse agent → run turn loop
                 → pre-review verification → independent review → post-review verification
                 → integration (merge + merged-tree tests) → completed | retry | failed
```

## 4. Agent execution

`AgentRuntime::run` is a bounded loop (≤24 rounds, per-round timeout, task deadline):

```
system prompt (persona + policy + wrapped untrusted context)
  → provider::generate (nc-code-provider executor: SSE, retries, classification)
  → tool calls (read_file, list_dir, grep, write_file, edit_file, run_command,
                submit_result, request_help)
  → permission gate + worktree path guard + command allowlist
  → tool result back into the transcript
```

Every round and every tool call is an event row and an audit row; usage is recorded
per agent/task/project with the provider/model dimensions and the configured prices.

## 5. Scheduling

`DagEngine` guarantees: no cycles, no unknown dependencies, deterministic topological
order. Readiness = all dependencies completed and not blocked. Priority = user priority ×
100 + critical-path weight × 30 + deadline urgency × 2 + fan-out × 5. Concurrency is
capped per project, and a lease waiter returns concrete evidence (holder task/agent,
expiry, mode) instead of silently queueing.

## 6. Isolation and integration

- one git worktree per task at `<git-common-dir>/ncag-worktrees/<task-id>`, branch `ncag/<task-id>`;
- the agent can only read/write inside that worktree (path normalisation + canonical prefix check);
- integration merges into the project branch in a *scratch* worktree, runs the project's
  test command there, commits, then moves the branch reference (and fast-forwards the
  operator's checkout when it is on that branch);
- a merge whose tree equals the base is integrated without a second commit;
- a textual conflict or a red merged-tree test is recorded as a conflict, never forced.

## 7. Failure and recovery

The supervisor sweep (every second) does:

1. lease expiry,
2. dead-letter sweep for unacknowledged messages,
3. health evaluation per live agent (heartbeat vs activity, repeated tool calls,
   repeated command failures, no new artifacts),
4. recovery ladder: nudge → help request → reassign (worktree preserved) → fail,
5. scheduler tick — recovered work becomes dispatchable in the same pass.

At startup, orphaned non-terminal agents are marked `recovering` and their tasks return
to `ready` with the worktree preserved (`recover_on_startup`). The audit chain, decisions,
messages and usage rows are never rewritten.

## 8. Security

| Control | Implementation |
| --- | --- |
| agent permissions | `security::effective_permissions` intersects the profile declaration with the role ceiling (reviewers/auditors are read-only) |
| tool permissions | `tools::invoke` refuses on the first missing permission (`permission_denied`) |
| workspace confinement | absolute/`..`/symlink escape rejected (`path_escapes_workspace`) |
| command policy | project allowlist, prefix match, refused commands are audited |
| untrusted content | `security::wrap_untrusted` sanitises directive-like lines and marks the block as DATA |
| secret hygiene | `redact`/`scrub_value` strip bearer tokens, `sk-…`, API-key fields and private keys from logs, evidence, messages and API payloads |
| audit integrity | hash chain (`sha256(prev|actor|action|detail|at)`), `/audit/verify` reports the first broken row |
| API token | optional; when configured, `?token=` or `Authorization: Bearer` is required |

## 9. Cost control

`BudgetManager` is a pre-flight check, not a report: an exhausted project budget pauses
the project, emits `budget.exceeded`, and opens a Material decision. Per-task token
limits stop that task without stopping the project. Pricing comes from the provider
record (`pricing.inputUsdPerMillionTokens` / `outputUsdPerMillionTokens`); a zero price
means the operator did not state it, and the dashboard shows `$0.00` rather than invent
a number.

## 10. Observability

- `events` table (append-only, ordered, exposed by `/events` and streamed over SSE),
- `audit` table (hash-chained, `/audit/verify`),
- `usage` table (tokens + cost per call),
- `evidence` table (per criterion, per task, producer),
- `integrations` table (base, changed files, conflicts, tests, resulting commit).

The dashboard subscribes to SSE and falls back to polling; a disconnected stream is
shown as `stream down`, not hidden.

## 11. Known boundaries

- Git is mandatory (worktree isolation). A project whose workspace is not a git
  repository is rejected with `invalid_request`.
- Model providers come from the personal provider store; NC-AG does not ship credentials.
  Without an executable provider the dispatch fails closed with `provider_unavailable`
  and the request becomes `waiting_provider`.
- The UI is mounted at `/nc-ag` (and `/cn/nc-ag`) on the existing web shell; no new
  router is introduced.
