# Decision: workflow engine core ported to Rust; native host is the conversation runtime

> **WARNING**: Never trust docs/logs without fully auditing code. Always verify against actual implementation.
Status: **updated** (2026-11-05) — originally accepted 2026-10-27 stating engine stays TypeScript; code audit shows engine is implemented in Rust (`native/crates/nc-code-workflow`) and integrated into `native/crates/nc-code-host/src/workflow_service/`. TS compiler/sandbox may still be used in some paths per design; see `specs/disabled-features.md §3.3`. **Always audit code, not just docs.**
Scope: `native/crates/zcode-host` v4 surface — the four workflow commands and the
seven `workflowRun*` read methods, plus `discardSharedContext`.

## Context

Two v4 capability groups have no implementation in the native host:

- **dynamic workflow**: `startSavedWorkflow`, `resumeWorkflowRun`,
  `amendWorkflowRunSettings`, `cancelBackgroundWork`, and the reads
  `v4/conversation/workflowRuns|workflowRunEvents|workflowRunArtifacts|
  workflowRunArtifactData|workflowRunArtifactRead|workflowRunWorkspace|
  workflowRunNodeResult`.
- **shared context**: `discardSharedContext` (import provenance is produced by the
  Desktop share flow, not by this host).

The TS workflow runtime (`apps/zcode-cli/packages/dynamic-workflow-runtime`,
~20k LOC) compiles and executes workflow scripts through the **TypeScript compiler
API** (≈11.6k LOC of that dependency). There is no offline Rust equivalent, and a
native re-implementation would change the workflow file format — a different
product, not a port.

## Decision

1. The native host is the **conversation runtime**: sessions, turns, tools, hooks,
   goals, file ledger/rewind, queue, interactions, read model. That surface must be
   complete, tested and benchmarked — it is.
2. The **workflow execution engine core is ported to Rust** (`native/crates/nc-code-workflow`) and integrated into `nc-code-host` workflow service. Some compilation/sandbox paths may still use TS/Node components by design; see `specs/disabled-features.md §3.3`. The TS workflow stack may remain the live path until the native host is default per rollout policy.
3. The affected commands stay in `commands::UNROUTED_COMMANDS` and answer
   `fault.command.unsupported:<command>` with the missing-subsystem message. The
   affected methods stay in `rpc::V4_METHODS_UNIMPLEMENTED` and answer a JSON-RPC
   method-not-found whose message names the reason. Both inventories are
   complement-tested, so this boundary cannot silently drift.
4. `discardSharedContext` follows the same rule for the same reason: without a
   producer the command could only ever answer "not pending", which is a fake
   surface.

## Consequences

- **No fake states.** UI never sees a run that does not exist, a queue disposition
  that was not applied, or a "pending" context that cannot be discarded.
- **Honest availability.** `snapshot.availability` and `commands::route` derive
  from one table; unwired workflow work is never advertised as allowed.
- **Resumption path.** If the product later decides to ship a workflow sidecar
  (Node process speaking the journal protocol), the integration contract is:
  the host owns run identity (`runId ≡ workId`), proxies `cancel/resume/amend/
  start` to the sidecar with idempotent command ids, and projects
  `backgroundWorks`/`workflowRuns` **only** from sidecar journal events. Until
  that contract exists, this decision stands.
- **Explicitly out of scope for "port complete".** Claiming full parity while this
  boundary exists would be false. The crate README (items 41) and
  `RUST-PORT-LOG.md` ENTRY 063 state this without hedging.
