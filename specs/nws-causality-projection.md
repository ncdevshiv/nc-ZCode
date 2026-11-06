# NWS causality projection — design

Status: implemented and gated 2026-11-01. `zcode-workflow`'s `script::graph`
(`mod.rs` records + `walk.rs` statements + `walk_expr.rs` expressions + `display.rs` renderers)
projects the triple; the host's `bound_causality_graph` renders it for the card. Evidence:
13 projection tests in `script/graph_tests.rs`, 2 host tests in `workflow_graph.rs` (the same caps
and the same phase vocabulary apply to a native graph), `nws_authoring_e2e.rs` 2/2 through the real
tool surface, `cargo test -p zcode-workflow` 117 lib + 12 + 9 + 2 integration, `cargo test -p
zcode-host --lib` 544, clippy and fmt clean. Numbers: `specs/nc-code-workflow-benchmark.md`
"Authoring projection". Governs the `CreateWorkflow` preview card for NWS scripts. See
[nc-code-workflow-rust.md](nc-code-workflow-rust.md) for the language and runtime.

## Why this is a rewrite and not a transcription

`CreateWorkflow`'s preview card is fed by a causality graph. The TypeScript path derives it
from a taint analysis of JavaScript data flow: variables are reassigned, functions are called
indirectly, `Promise` combinators nest arbitrarily, so "what may run before what" has to be
recovered from the AST and a heap model (~11.6k of the ~20k analysis LOC).

NWS states the same facts lexically. There is no assignment, no user function, no closure, and
concurrency is a block (`parallel { { … } { … } }`) rather than a combinator. Every facade call
already carries a site id and a span from the resolver. So the projection is **derived from the
lowered tree**, and the JS-shaped machinery (sweeps, fixpoints, taint witnesses) is not carried
over — copying it would import bugs NWS cannot have.

## Boundary: what NWS emits, what stays shared

NWS emits the same **analysis triple** the TypeScript analyzer emits:

```
{ "causality": { steps, regions, lanes, edges, sink?, phases?, phaseEdges? },
  "flow":      { phases?, phaseEdges? },
  "handoff":   { participants, handoffs } }
```

and nothing else. The bounding pass (`zcode-host`'s `workflow_graph::bound_causality_graph`, a
line-for-line port of `boundCausalityGraph`) stays the single implementation for both languages.
NWS gets no second copy of the caps, the truncation order, the all-or-nothing phase vocabulary
or the `unphased` fallback — those are the display contract, and duplicating them is how two
surfaces start disagreeing.

Consequence: `flow.nodes` / `flow.edges` (the step-level control flow, which the bounding pass
never reads) are **absent** for NWS. Recorded, not hidden: the preview card consumes the phase
quotient only. Anything that later wants the step-level control-flow view needs its own
projection and its own spec change.

## Step contract

`kind` is the contract's enum and nothing wider: **`ask` and `world-read`**. Actor creation,
`report`, artifact declaration and artifact publication are real sites with real journal keys,
but they are not steps — they shape lanes and cards, which is what the TypeScript path does too.

| field | NWS rule |
| --- | --- |
| `id` | the site id (`ask#3`, `world-read#1`) — the same key the runtime reports, so the live overlay joins on it |
| `label` | ask: the instruction literal (a template's hole-free prefix otherwise); world-read: the op with its first literal argument |
| `loc` | the site's span, 1-based line and column |
| `lane` | the actor lane the receiver resolves to, `workspace` for a world read, `unknown` when the receiver is not provably one actor |
| `lanes` | only on a may-set: the candidate actor lanes, `lane` = `lanes[0]` |
| `source` | only on a may-set copy: the site id the runtime actually reports |
| `region` | innermost enclosing region |
| `certainty` | `always`, or `maybe` inside an `if` arm, a loop body, or a `try`/`catch` arm. `parallel` strands are unconditional, so they do not make a step `maybe` |
| `phase` | when the script declares any phase: the innermost marker visible at that point, else `unphased`. Every step carries one |
| `repeat` | inside a repeating region: `stack` when a `parallel` block encloses it (instances coexist), else `serial` (an await barrier or one actor's mailbox orders the rounds) |

**Multi-phase steps.** A step can be reached under two different markers — one `phase` in each arm
of an `if`, both before a step after the block. Then it is emitted once per phase, sharing the
site `id`, because the partition has to stay total. The copies are distinguishable by `phase`,
and the overlay join stays valid because it keys on `source ?? id` narrowed by lane.

## Region contract

`RegionKind` stays the public union: `seq | parallel | loop | fanout | branch | shared`.

| construct | kind | id |
| --- | --- | --- |
| workflow body | `seq` | `root` |
| `if` / `else if` / `else`, `try` / `catch` | `branch` | `branch#k` |
| `while` | `loop` | `loop#k` |
| `for (const x of …)` | `loop`, or `fanout` when its body creates an actor | `loop#k` / `fanout#k` |
| `parallel` | `parallel` | `parallel#k` |

`k` counts per kind in first-visit order. `{ … }` blocks introduce a scope, not a region: they
change nothing about whether a step runs.

## Edge contract

`OrderEdge.kind` ∈ `data | control | seq | carry | fifo`. NWS produces three of the five, and the
two it does not produce are a fact about the language, not a gap in the projection:

- `seq` — two awaited steps in one strand of one region, in program order. An await is the
  barrier, so the edge means "settled before".
- `data` — a step's result reaches a later facade call's arguments, followed transitively through
  `let x = y` chains and member/index reads. `types` (surfaced on handoffs) is the static shape of
  what crosses: a typed ask contributes its schema name (`Plan`), an untyped ask `text`, a world
  read the read's product (`text`, `paths`, `diff`, `log`, `status`, `changed`, `outcome`), an
  artifact publication `ref`.
- `control` — a step that produced a value used in an `if`/`while` condition precedes every step
  of the construct that condition guards.
- **no `carry`** — a loop-carried data edge needs a value written in round *k* and read in round
  *k+1*. NWS slots are write-once per round and a body-local `let` is not visible outside its
  round, so that dependency cannot be expressed. Round-to-round ordering is carried by the
  control-flow `loop` edge instead.
- **no `fifo`** — first-in-first-out interleaving of a shared mailbox is not derivable lexically,
  and inventing it from source order would claim more than the language promises.

## Phase quotient (`flow`)

- `entry → first` and marker-to-marker in program order: `next`.
- a marker inside a loop or `for-of` body: `loop` from the last phase reached inside the body back
  to the first one inside it.
- `return` inside a phase: `jump` to `sink`.
- a phase declared inside an `if` arm whose code continues after the block: `jump` bypassing it.
- a `throw`/failure path: `throw` to `abort` (the bounding pass drops `abort` edges, and that is
  the contract's decision, not ours).
- `alongside` comes from **the existing derivation** (`derive_phase_alongside`, the same function
  that feeds the `run-launched` payload). Two sources of truth for "which stages run at the same
  time" is how a card ends up disagreeing with the sidebar, so the graph reuses it and a test
  asserts they agree.

## Handoff contract

- One participant card per `(phase, lane)` with at least one step:
  `phase#K:actor#N`, `phase#K:workspace`, `unphased:…`. `steps` in source order.
- A lane created inside a `for-of` body is a **family**. When the iterable is a literal array of
  at most `FAMILY_EXPANSION_LIMIT` (8) elements, the family expands to one card per member
  (`…[i]`, `member: { index, of }`). Otherwise one card with `many: true`.
- A handoff joins two cards that some edge crosses. `types` are the data edges' product types,
  deduplicated in contribution order; `back: true` only when every contributing edge is a
  loop-carried one.

## Id alignment with the run payload

`phase#N` numbering is first-reach order, which is the same order as the declared `phaseNames`
table the `run-launched` row carries. A test pins that: the graph's *N*-th phase must be the
table's *N*-th name, and the graph's `alongside` row *N* must equal `phaseAlongside[N]`.

## Non-goals

- Runtime counts. The projection is static; it never runs the plan.
- `unknown`-lane resolution beyond what the receiver's slot origin proves.
- Per-callsite specialization of a site (`ask#3/2~actor#1`). The contract reserves `source` for
  it; NWS does not mint those ids yet, and the join stays stable when it does.

## Acceptance

1. `cargo test -p zcode-workflow` covers the walk on real fixtures: linear (steps, lanes, one
   handoff, sink), `parallel` (no seq edge between strands, alongside phases), `for-of` with a
   literal array (member cards) and with a computed array (`many`), `if` (certainty `maybe`,
   bypass `jump`), `while` (`loop` edge, `serial` repeat), `try/catch` (recovery edge), a
   multi-phase step (two copies, same id), and a >64-step script (the bounded pass flags
   `truncated`).
2. Every emitted step id exists in the plan's site table and every lane id is an actor site id,
   `workspace` or `unknown` — a unit test that walks the JSON and checks it against `Plan::sites`.
3. The host path: `CreateWorkflow` on an NWS script carries a `causalityGraph` whose `steps` are
   non-empty, and `nws_authoring_e2e.rs` flips from asserting absence to asserting presence.
   The same e2e keeps asserting no `.zcode/workflow-runs/` module, so "the graph is real" cannot
   quietly mean "we called Node for it".
4. No TypeScript analysis module is consulted for an NWS run, and no new dependency is added.
