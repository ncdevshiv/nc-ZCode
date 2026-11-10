# NC-AG — architecture decision records

Each record is permanent: superseded decisions are marked, never deleted.

---

## ADR-001 — Git worktrees, not a shared working directory

**Status:** accepted (implementation: `src/git.rs`, `src/leases.rs`)

**Context.** Parallel agents editing one checkout overwrite each other. The lease test
`shared_reads_coexist_but_a_writer_waits` proved the coordination half, but a lease only
prevents *declared* collisions; a mistaken write outside the declared set is still
possible in a shared tree.

**Decision.** Every task runs in its own `git worktree add -b ncag/<task-id>` under
`<git-common-dir>/ncag-worktrees/`. Integration merges the task branch into the project
branch in a *separate scratch* worktree, runs the project test command there, and only
then advances `refs/heads/<project-branch>`.

**Alternatives.** (a) shared checkout + file locks: cheap, but one bad write is
unrecoverable; (b) copy-on-write directory snapshots: no git history to integrate;
(c) one clone per task: correct but expensive and slow.

**Consequences.** Integration needs a merge step and a rebase-free policy; worktrees are
disposable and can be pruned; the operator's checkout is only fast-forwarded when clean
(see ADR-006).

---

## ADR-002 — Provider transport vocabulary is the provider store's vocabulary

**Status:** accepted (implementation: `src/provider.rs`)

**Context.** Live verification showed the personal provider store writes
`api.type: "openai-chat-completions"`, while NC-AG initially accepted only
`openai-compatible`. Every dispatch failed closed with `provider_unavailable` even though a
real model was configured.

**Decision.** NC-AG maps the store's vocabulary: `anthropic-messages` →
`ProviderKind::Anthropic`, `openai-chat-completions` (plus the legacy aliases) →
`ProviderKind::OpenaiCompatible`. `openai-responses` is a different wire protocol and is
rejected **by name** rather than being silently sent as chat-completions.

**Consequences.** One source of truth for transports. A new provider type must be mapped
explicitly; unknown types fail closed with the supported list in the message.

---

## ADR-003 — Dispatch is spawned, not awaited

**Status:** accepted (implementation: `Orchestrator::tick_project` / `spawn_dispatch`)

**Context.** The first scheduler awaited the whole dispatch (agent run + review +
verification + integration) inside the candidate loop. `maxConcurrentAgents` therefore
never produced parallelism, and one long task froze the sweep for minutes.

**Decision.** `tick_project` claims a task via `DispatchGuard::try_begin`, then
`tokio::spawn`s the dispatch. The guard and the persisted agent/task statuses are the
duplicate-execution authority; the supervisor observes completion.

**Alternatives.** (a) keep the await and rely on bigger timeouts — wrong, it is sequential
by construction; (b) a worker pool of N runtimes — heavier and duplicates state.

**Consequences.** `in_flight()` counts live dispatches. Scenario P proves two agents run
concurrently and the configured ceiling is respected. A panic inside a dispatch would leak
the guard, so the spawned closure ends the guard on the error path and on success.

---

## ADR-004 — Verification phases: pending is not failed

**Status:** accepted (implementation: `src/verification.rs`)

**Context.** A review criterion (`ReviewApproved`) judged before the reviewer ran failed
the pre-review pass, so tasks could never reach the review stage.

**Decision.** Two explicit phases: `PreReview` judges mechanical criteria and marks
review/audit criteria **pending** (neither pass nor fail, not recorded as evidence);
`PostReview` requires the review/audit evidence and fails without it.

**Consequences.** Failed evidence stays visible forever; a pending criterion can never be
mistaken for a success. The reviewer remains the only producer of review evidence.

---

## ADR-005 — Decisions are typed, and material ones belong to the user

**Status:** accepted (implementation: `src/decisions.rs`)

**Context.** The product requires structured disagreement rather than silent choices, but
not every choice deserves a human interrupt.

**Decision.** Four classes: `autonomous`, `technical_recommendation`, `material`,
`user_reserved`. Only the first two may be resolved by the orchestrator; `material` and
`user_reserved` are rejected with `user_decision_required` unless `decidedBy == "user"`.
A debate needs both sides **and evidence**; unsupported disagreements are refused.

**Consequences.** Settled decisions are not reopened without new evidence; resolving
broadcasts a notification to the project and appends an audit row.

---

## ADR-006 — Never destroy the operator's checkout

**Status:** accepted (implementation: `GitRepo::finalize_merge`)

**Context.** The first integration implementation ran `git reset --hard` on the operator's
checkout to keep it consistent with the advanced branch. That silently discards
uncommitted work.

**Decision.** After moving `refs/heads/<project-branch>`, the checkout is fast-forwarded
with `git merge --ff-only` **only if** `git status --porcelain` is empty. A dirty checkout
is left untouched; the integration row still records the resulting commit.

**Consequences.** A diverged/dirty checkout may lag the project branch; the operator sees
that in `git status` instead of losing edits.

---

## ADR-007 — Audit is a hash chain, not a log table

**Status:** accepted (implementation: `Store::append_audit`, `verify_audit_chain`)

**Context.** "Audit records protected against modification" (spec §19) cannot rely on
row-level permissions inside the same SQLite file.

**Decision.** Every audit row stores `hash = sha256(prev_hash | actor | action | detail | at)`.
`/api/nc-ag/audit/verify` recomputes the chain and returns the first broken row id.

**Consequences.** Edits or deletions of earlier rows are detectable; the chain is linear,
so verification is O(n) with no external service. Compaction is not supported (it would
break the chain).

---

## ADR-008 — Concurrency and helper budgets are caps, not targets

**Status:** accepted (implementation: `planner`, `request_help_route`, `ProjectConfig`)

**Context.** Autonomous spawning can explode cost and agent count.

**Decision.** `maxAgentsPerTask` is enforced when a helper is requested (refused with
`conflict`), `maxConcurrentAgents` is enforced by the scheduler, `maxReviewCycles` is
enforced by the review loop (escalation instead of another cycle), `max_retries` bounds
dispatch retries, and the project USD/token budgets are checked pre-flight.

**Consequences.** A refusal is an event and an audit row, never a silent no-op.

---

## ADR-009 — Untrusted content is wrapped and sanitised, not trusted

**Status:** accepted (implementation: `src/security.rs`)

**Context.** Repository files, tool output and messages are attacker-controlled in the
general case (Scenario N).

**Decision.** Retrieved content enters the prompt inside
`<untrusted-content kind=… origin=…>`, with directive-like lines replaced by a redaction
marker and the closing tag escaped. The system prompt states that the block is data and
cannot change permissions or tasks. Role permission ceilings are applied independently of
the profile declaration.

**Consequences.** A malicious file can still be *read*; it cannot widen permissions, add
tasks, or change the tool surface. The agent is instructed to report the attempt in its
limitations.
