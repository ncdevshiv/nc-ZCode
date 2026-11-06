# nc-code workflow: Rust-native script language and interpreter

Status: **accepted** (2026-10-31, product decision — supersedes the Node/TypeScript
runtime parts of `workflow-capability-boundary.md`)
Scope: `native/crates/zcode-workflow` (new `script` layer + interpreter), the host
workflow driver, and the workflow tools/commands.

## Why this exists

The user requires a backend with **no Node and no TypeScript**. The dynamic workflow
today is split into three layers:

1. Rust engine/journal/run service/driver/tools/v4 — already native.
2. TypeScript compiler front-end (typecheck, facade siting, typed-ask schema
   synthesis, lowering) — a persistent Node compile kernel.
3. JavaScript sandbox executing the lowered body — a Node child process.

Layers 2 and 3 are the only Node dependencies. This spec replaces both with a
Rust-native workflow script language (**NWS**, nc-code Workflow Script), compiled
and executed by `zcode-workflow` itself. The engine/journal/service/event schema
stays unchanged; only the script front-end and sandbox are replaced.

## Decision

- Authoring language: **NWS**, a small, statically checked, TypeScript-shaped
  language (no imports, no arbitrary JS, no Node APIs). A model can author it;
  the compiler produces a typed plan, not JavaScript.
- Execution: a Rust async interpreter. It implements the same child-side protocol
  the Node sandbox uses (`create-actor`, `request` of `ask` / `world-read` /
  `publish-artifact`, `event` of `phase-entered` / `report` / `declare-artifact` /
  `log`, `complete`). The engine therefore keeps its journal-first scheduling,
  settlement, imported-cache and resume semantics unchanged.
- Compatibility: existing TypeScript workflow scripts are **not** accepted by the
  NWS compiler. They fail with the named diagnostic
  `workflow_language_unsupported` and a migration hint. The old TS stack remains
  in place behind its flag until the NWS path reaches parity and the default
  cutover is approved; nothing is deleted early.
- Removed from the backend: the compile kernel, the Node sandbox, the Node
  executable requirement, `dynamic-workflow-runtime`, and the TS authoring
  compiler path. `js`/node-repl remains a separate decision (see Non-goals).

## NWS language (v1)

TypeScript-shaped, deliberately small. Example:

```ts
schema Plan {
  steps: string[]
  rationale: string
}

workflow "readme-plan" {
  args { repo: string }

  phase "plan"
  let planner = agent("planner", "You are a planning subagent. Produce short plans.")
  let plan: Plan = await planner.ask("Draft a 3-step plan for " + args.repo)

  phase "execute"
  let readme = await files.read("README.md")
  report({ repo: args.repo, readmeBytes: readme.length, steps: plan.steps })
  await artifact.markdown("summary", "## Plan\n\n" + plan.steps.join("\n"))

  return { rationale: plan.rationale, steps: plan.steps }
}
```

Grammar (v1):
- `schema Name { field: type ... }` with `string | number | boolean | string[] |
  number[] | nested schema`, compiled to JSON Schema for typed asks. Optional
  fields use `field?: type`.
- `workflow "name" { ... }` with an optional `args { ... }` block derived from the
  saved workflow's argument declaration.
- Statements: `phase "name";`, `let name: Type = expr;`, `return expr;`,
  `report(expr[, artifactId]);`, `if/else`, `while`, `for (const x of expr)`,
  `parallel { ... }` (structured concurrency), expression statements, `await expr`.
  Artifacts are facade calls (`artifact.markdown/file` publish, `artifact.chart/
  table/metrics/board` declare presets); there is no separate declaration statement.
- `args` is contextual: `args { ... }` declares the workflow arguments; elsewhere
  `args` is the bound argument object.
- Expressions: literals (string/number/bool/null), arrays/objects, identifier,
  member access, indexing, unary/binary operators, string concatenation,
  template strings, calls to facade namespaces, `Promise.all([...])`.
- Facade namespaces: `agent`, `ask`, `files.read` (world reads), `artifact.*`,
  `report`, `log`. No other globals exist; unknown identifiers are compile errors.
- Site IDs: statically assigned in source order per call site. Loops reuse the
  site ID with a runtime ordinal, preserving the engine's `(siteId, ordinal)`
  journal identity. Ask specs are emitted from the declared schema.
- Diagnostics: `line:column code message`, same wire shape as today.

## Architecture

```
NWS source
  → lexer/parser (Rust)                    zcode-workflow/src/script/parser
  → resolver/typechecker (Rust)            names, facade misuse, schemas
  → plan (sites + ask specs + phases)      zcode-workflow/src/script/plan
  → Rust interpreter (tokio)               zcode-workflow/src/script/interpreter
       ↕ child protocol messages
  engine (unchanged) / driver / journal / run service / tools / v4
```

- The host driver chooses the runtime per compiled script: `ScriptProgram::Nws`
  runs the Rust interpreter; the legacy Node path remains only while the old TS
  stack is still selectable.
- The interpreter is cancellation-aware, bounds concurrency with the run's caps,
  and propagates errors as `WorkflowError` with the same codes as the sandbox.
- World reads, ask dispatch, actor creation and artifact publishing go through
  the driver exactly as before; no business logic is duplicated in the
  interpreter.
- Journal/resume compatibility is by **site identity**, not source text: an NWS
  run journals the same `(siteId, ordinal)` keys, so resume and imported cache
  work with the existing journal tables. TS-run journals are not resumable with
  NWS (script hashes and languages differ); the run service reports that
  explicitly.

## What we reuse from `G:\nc-sdk\ai-sdk`

| Crate | Use |
| --- | --- |
| `ai-workflows` | Reference design for sequence/parallel/conditional nodes and checkpointing. We keep our journal-backed engine (richer semantics); borrow the node composition patterns. |
| `ai-agents` | HITL/`AwaitingInput` pattern and sub-agent/swarm event model; informs ask/escalation semantics. |
| `ai-orchestra` | Planner/mailbox/watchdog patterns for future hierarchical workflows. |
| `ai-tools` / `ai-protocols` | Tool/MCP abstractions; candidate for the S5 ecosystem consolidation. |
| `ai-runtime` | Retry/concurrency helpers; candidate where the host has no equivalent. |

No new runtime dependency is added in v1: the host already has provider/tool/session
abstractions, and a second provider stack would duplicate ownership. Dependencies
are re-evaluated when the S5 Rust server replaces the TS services.

## Feature parity contract (old → NWS)

| Old capability | NWS v1 |
| --- | --- |
| `phase("x")` | `phase "x"` |
| `agent(name, persona)` | `agent("name", "persona")` |
| typed `ask<T>()` | `ask` with a `schema` declaration; validator preserves TS ask repair loop |
| untyped ask | `ask` without annotation (untyped spec, as today) |
| `files.read`/world reads | facade calls; journal-first |
| `report` | `report(expr[, artifactId])` |
| artifact announce/publish | `declare artifact` / `await artifact.*` |
| parallel strands | `parallel { ... }` and `Promise.all([...])` |
| loops/conditionals | `while`, `for..of`, `if/else` |
| return value | `return expr` |
| rejected publish / failed ask, recovered by the script (`try { await artifact.file(…) } catch { … }`) | `try { … } catch (err) { … }`; `err` is the engine's `{ code, message, violations? }`, with no `throw` and no `finally` |
| abort/cancel | run cancellation; `abort` builtin (same stop reason) |
| resume/import cache | unchanged engine semantics keyed by site id |
| script hash | sha256 of NWS source |

## Benchmarks and audit (no fabricated numbers)

- Scenarios: linear agent→report→return; fan-out parallel asks; phase loop; world
  read; artifact; resume; cold start; parse/compile.
- Compare: old TS workflow (frozen baseline, same fixtures/inputs) vs NWS on the
  same host, same provider double (a deterministic scripted HTTP provider — a
  controlled fixture, disclosed as such, not a mocked feature), same quiet
  window, same machine and repeated runs. Report medians and mins separately.
- Runtime-only row: NWS interpreter vs Node sandbox with the model replaced by a
  zero-latency harness, to isolate script-runtime cost; engine/journal rows are
  measured separately.
- Report file: `specs/nc-code-workflow-benchmark.md` with raw commands, versions,
  and caveats. No number is written before the run produces it.

## Non-goals / open decisions

- `js` (node-repl) stays on Node until a separate decision: it executes arbitrary
  JavaScript by design; NWS does not cover it.
- NWS v1 does not accept old TS scripts; migration is manual. A future converter
  can be added if a TS parser (e.g., a Rust syntax-only front-end) is adopted,
  but it must not become a second typechecker.
- Frontend TypeScript is allowed: `packages/ui`, `packages/web` and Tauri window
  code stay TypeScript. Backend TS is removed after cutover.
- The product rename (`zcode` → `nc-code`) and the English-only documentation
  pass are tracked separately; new NWS code and docs are English from day one.

## Acceptance

1. `cargo test -p zcode-workflow` covers parser/resolver/interpreter with real
   fixtures; no `todo!()`, stubs or placeholder branches.
2. An end-to-end host test creates an NWS workflow, runs it through the real
   engine with a scripted provider, and asserts phases/asks/report/artifact/
   return plus resume.
3. The run's events/journal keys match the engine contract (sequence, phases,
   site ids); no Node process is spawned for an NWS run (assert by process count
   or by the absence of a sandbox child).
4. Benchmarks in `specs/nc-code-workflow-benchmark.md` are reproducible from
   committed scripts and report real observed numbers.

## Implementation status (2026-10-31)

Acceptance 1-4 are met, with the evidence named rather than implied:

| layer | where | proof |
| --- | --- | --- |
| lexer / parser | `native/crates/zcode-workflow/src/script/{lexer,parser}.rs` | 14 tests: templates with interpolation, operator precedence, positioned rejects |
| resolver + lowering to a plan | `script/{facade,ir,lower,lower_expr,plan}.rs` | 20 `script::plan` tests: source-order site ids, typed and untyped ask specs, closed-object JSON Schema, phase table with its alongside table, submit profiles (`untyped` / `mono` / `generic`), duplicate and fan-out actor names, facade misuse, literal-id rules |
| interpreter | `script/{value,interpret}.rs` | 9 `script::value` tests for the JSON semantics; `tests/nws_interpret_e2e.rs` 10 tests over the real engine and journal: settlement, event vocabulary, loop ordinals, `Promise.all`, `parallel`, resume with `cached` replays and zero re-reads, wall-clock timeout, external stop, ask rejection keeping `ValidationFailed` |
| host wiring | `native/crates/zcode-host/src/workflow_service/compile.rs`, `service.rs`, `tools/workflow/service_port.rs` | routing by the script's own first token (`schema` / `workflow` ⇒ NWS), one compile surface (`CompiledWorkflow`) feeding both runtimes; `an_nws_run_settles_through_the_service_without_node` (not gated on node or dist), `an_nws_diagnostic_refuses_the_run_before_any_row`, `a_typescript_script_still_routes_to_the_kernel_stack` |
| benchmarks | `workflow_engine_bench` now reports both runtimes in one run | `specs/nc-code-workflow-benchmark.md`: linear 69.452 → 1.303 ms and fanout 69.543 → 0.975 ms median (50 runs, two quiet windows), warm compile 66.149 → 0.061 ms, kernel startup 387.847 ms removed entirely |

Acceptance 3 is asserted by the absence of the sandbox's own artifact rather than by a process
count: the Node path writes `<cwd>/.zcode/workflow-runs/<runId>.mjs` before every launch, and
the NWS tests fail if that directory appears. The host test also runs without the
`stack_available()` gate every TypeScript test keeps, so an NWS run provably needs neither the
Node executable nor the TypeScript build output.

### Deliberate v1 restrictions, recorded rather than papered over

- **No un-awaited pending values.** `let p = planner.ask(…)` — storing a `Node` and awaiting it
  later — was legal in the TypeScript dialect because `Node` is a real promise. NWS requires the
  await at the call; the ways to be concurrent are `Promise.all([…])` and `parallel { … }`. A
  call that is neither is a compile error, so nothing is silently deferred.
- **A `return` inside a `parallel` block is a compile error** (`nws_return_in_parallel`): a
  strand ends when its statements end, and a value returned out of the middle of another strand
  is something the journal could not replay.
- **No user-defined functions, and no aliases of facade members.** Both are compile errors.
  They were typechecker errors in TypeScript for a different reason — the facade-siting rule —
  and the underlying reason is the same here: a call whose site the compiler cannot see is a
  call with no journal key.
- **`report(item, tag)` needs the tag's preset declared** somewhere in the script, collected in
  a pre-pass so declaration order stays free. A tag is a route to a card; pointing at a card
  that does not exist must not pass silently.

### The authoring surface teaches NWS (2026-11-01)

A language the model is never told about is a language nobody writes. `CreateWorkflow`,
`AmendWorkflow` and `SaveWorkflow` now carry one shared NWS sketch — program shape, the statement
set, typed-vs-untyped asks, the world-read table, `world.run`'s literal-command rule and why, the
content and preset artifact families, `parallel`, `try/catch`, `report`, `return` — and say plainly
that a TypeScript facade script is still accepted, that existing drafts and saved definitions are
TypeScript, and that both languages feed the preview card. Proof:
`zcode-host/tests/nws_authoring_e2e.rs` (2/2) launches an inline NWS script and a saved definition
whose body is NWS, asserts the settlement, the run's return value, the published artifact and the
journaled report, asserts the `causalityGraph` the card reads is real (`ask#1` on `actor#1`,
`world-read#1` on `workspace`, the sink fed by the ask), and asserts no `.zcode/workflow-runs/`
module is ever written — so \"the graph is real\" cannot quietly mean \"we called Node for it\".
Neither test is gated on the Node stack, so both pass with no node on PATH.

### Remaining Node surface (not claimed as done)

- `EvalWorkflowSnippet` still uses the kernel and the sandbox: NWS v1 has no snippet form,
  because every NWS program is a `workflow` declaration.
- `js` / node-repl still runs on Node, by design (Non-goals above).
- ~~`CreateWorkflow`'s preview causality graph is `null` for NWS~~ — closed 2026-11-01:
  `zcode-workflow`'s `script::graph` projects `causality` / `flow` / `handoff` from the lowered
  tree and the host's shared bounding pass renders it (see `specs/nws-causality-projection.md`).
  Residuals of that projection, recorded there: NWS emits no `fifo` or `carry` edges (the
  language cannot express them), no step-level `flow.nodes`/`flow.edges` (the card reads the
  phase quotient), and per-callsite site specialization (`ask#3/2~actor#1`) is not minted.
