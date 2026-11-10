# NC-AG — agent profiles and lifecycle

## 1. Profiles

Profiles are capability contracts. They are registered in
`native/crates/nc-code-orchestrator/src/profiles.rs` and exposed by
`GET /api/nc-ag/profiles`. A profile that is not registered can never be spawned.

| id | role | read | write | shell | git.merge | review-capable |
| --- | --- | --- | --- | --- | --- | --- |
| `generalist` | general | ✓ | ✓ | — | — | no |
| `architect` | architecture | ✓ | ✓ | — | — | yes |
| `backend` | backend | ✓ | ✓ | ✓ | — | no |
| `frontend` | frontend | ✓ | ✓ | ✓ | — | no |
| `api` | api | ✓ | ✓ | ✓ | — | no |
| `database` | database | ✓ | ✓ | ✓ | — | no |
| `devops` | devops | ✓ | ✓ | ✓ | — | no |
| `docs` | documentation | ✓ | ✓ | — | — | no |
| `qa` | qa | ✓ | ✓ | ✓ | — | yes |
| `e2e` | e2e-test | ✓ | ✓ | ✓ | — | yes |
| `security` | security | ✓ | — | ✓ | — | yes |
| `reviewer` | review | ✓ | — | ✓ | — | yes |
| `critiquer` | critique | ✓ | — | — | — | yes |
| `integrator` | integration | ✓ | — | ✓ | ✓ | no |
| `performance` | performance | ✓ | ✓ | ✓ | — | yes |
| `analyst` | requirements | ✓ | ✓ | — | — | yes |

Each profile also carries: `persona` (reasoning/communication style), `expertise`,
`capabilities` (the tokens the planner matches), `tools`, `restrictions`,
`deliverable`, `quality_bar` and `escalation`. All of it is injected into the agent's
system prompt.

**The permission ceiling is enforced twice.** A profile declares permissions; the
orchestrator then intersects them with `security::ceiling_for_role(role)`, which is
read-only for `review`/`critique`/`security` and includes `git.merge` only for
`integration`. A reviewer cannot write even if a project owner edits the profile.

### Adding a specialisation

```rust
registry.register(AgentProfile {
    id: "ml".into(), role: "ml".into(), display_name: "ML Engineer".into(),
    persona: "Empirical; measures before claiming.".into(),
    expertise: vec!["training".into()],
    capabilities: vec!["model-training".into()],
    tools: vec!["read_file".into(), "write_file".into()],
    permissions: vec!["fs.read".into(), "fs.write".into()],
    restrictions: vec!["no dataset exfiltration".into()],
    deliverable: "trained checkpoint + eval numbers".into(),
    quality_bar: "metrics reproducible from the recorded command".into(),
    escalation: "stop and ask when the dataset is missing".into(),
    review_capable: false,
})?;
```

Registration fails with `conflict` for a duplicate id. The planner refuses a task whose
capability has no profile (`invalid_request`), so a plan can never reference a
non-existent specialist.

## 2. Lifecycle

```
CREATED → INITIALIZING → STARTING → IDLE ⇄ ASSIGNED → WORKING
                                       ├→ WAITING → WORKING
                                       ├→ BLOCKED → WORKING
                                       ├→ HELP_REQUESTED → WORKING
                                       ├→ REVIEWING → WORKING | IDLE
                                       ├→ HANDOFF_PENDING → WORKING | STOPPED
                                       └→ RECOVERING → WORKING | IDLE
WORKING → STOPPING → STOPPED | FAILED
FAILED → RECOVERING → WORKING | STOPPED
```

Transitions are validated by `state.rs`; an illegal transition is an
`illegal_transition` error and the stored status is left untouched. Terminal states
(`STOPPED`, `FAILED`) never resume — a replacement agent is spawned instead.

What each edge means in practice:

| Transition | Trigger |
| --- | --- |
| `STARTING → WORKING` | dispatch assigned the task and the worktree exists |
| `WORKING → REVIEWING` | the agent submitted and pre-review verification passed |
| `WORKING → WAITING` | user paused the agent (`POST /agents/{id}/pause`) |
| `WORKING → HELP_REQUESTED` | recovery ladder chose "request help" |
| `WORKING → HANDOFF_PENDING` | `POST /tasks/{id}/handoff` was accepted |
| `WORKING → RECOVERING` | startup found an orphaned agent (orchestrator restart) |
| `* → FAILED` | model failure after the repair budget, or recovery exhausted |

## 3. Health and stuck detection

`recovery::evaluate` compares two clocks and three counters:

- **heartbeat** (`last_heartbeat`) → liveness only; a fresh heartbeat never proves progress;
- **activity** (`last_activity`, written by real events) → progress;
- tool-call repetition (≥5 identical calls), command failure repetition (≥3 identical
  failures), artifact count (new files in the worktree).

Findings map onto the ladder:

| Ladder step | Condition | Effect |
| --- | --- | --- |
| `Nudge` | first recovery | message to the agent, `agent.progress` marker `nudged` |
| `RequestHelp` | second recovery | `HELP_REQUEST` message, agent → `HELP_REQUESTED` |
| `Reassign` | third recovery while attempts ≤ retries | agent `FAILED`, task back to `READY`, worktree preserved |
| `Fail` | recovery exhausted | task `FAILED` with the accumulated reason, escalated |

A long-running task is **not** failed for taking longer than expected: the stall
threshold is configured per project (`stallAfterSecs`) and suppressed by any new artifact.

## 4. Context package

Every agent receives exactly two system blocks:

1. **policy + persona + hard rules** — worktree confinement, untrusted-data rule,
   verification rule, allowed commands, escalation rule;
2. **project context** — request description, acceptance criteria, settled decisions,
   relevant messages, held leases, workspace top level, detected repository facts.

The second block is wrapped by `security::wrap_untrusted`, so instruction-like text from
files or messages is neutralised before the model sees it.

## 5. Tools

| Tool | Permission | Notes |
| --- | --- | --- |
| `read_file` | `fs.read` | 200KB cap, numbered lines, offset/limit |
| `list_dir` | `fs.read` | non-recursive, sorted |
| `grep` | `fs.read` | regex, skips `target/`, `node_modules/`, `.git/` |
| `write_file` | `fs.write` | 2MB cap, creates parents |
| `edit_file` | `fs.write` | requires exactly one `old_text` match |
| `run_command` | `shell.exec` | allowlist prefix match, 900s cap, output truncated |
| `submit_result` | none (control) | ends the task with summary/files/commands/limitations |
| `request_help` | none (control) | routed to the helper budget |

Every invocation is written to `events` (`agent.tool`) and to `audit`
(`tool.executed`), attributed to the agent, task and project.

## 6. Communication kinds

`INFORMATION, TASK_UPDATE, HELP_REQUEST, HELP_RESPONSE, BLOCKER, RESOURCE_REQUEST,
RESOURCE_RELEASE, CONTRACT_PROPOSAL, CONTRACT_CHANGE, ARCHITECTURE_PROPOSAL,
ARCHITECTURE_CONFLICT, REVIEW_REQUEST, REVIEW_FINDING, HANDOFF, ESCALATION,
TASK_COMPLETION, FAILURE, SYSTEM_EVENT`.

Messages carry id, UTC timestamp, sender, recipient/channel, project, task, subject,
body, refs, correlation id, priority, acknowledgment requirement/state, delivery state
and a dedupe key (duplicate sends are no-ops). Dead letters are swept when an
acknowledgment stays pending past the project's message TTL.

## 7. Handoff

`POST /tasks/{id}/handoff` records a package containing the completed work, the worktree
and branch, the evidence rows, the previous agent and a note, sends it as a `HANDOFF`
message (ack required), marks the sender `HANDOFF_PENDING` and reassigns the task. The
previous worktree is never deleted before the handoff is persisted.
