# nc-code workflow: old vs new benchmark and audit

Measured 2026-10-31 on one Windows 10 (10.0.26300) x64 host, release build, two quiet windows
back to back with nothing else running. Every number below was produced by the command in
§Method and copied out of its JSON output — none is estimated, and rows that could not be
measured are marked as such rather than filled in.

## What is being compared

| | old path (live today) | new path (NWS) |
| --- | --- | --- |
| authoring language | the TypeScript workflow dialect | nc-code Workflow Script (NWS) |
| front-end | resident Node compile kernel driving `apps/zcode-cli/packages/dynamic-workflow` dist | `zcode-workflow/src/script` (lexer → parser → resolver/lowering), in-process |
| script runtime | Node child process (`node/child_main.js`) over an NDJSON bridge | `zcode-workflow/src/script/interpret`, in-process tokio |
| engine / journal / run service | `zcode-workflow` | the same code, unchanged |
| Node required | yes, for both compile and run | no |

The engine is identical on both sides on purpose: the benchmark measures the two layers that
changed (front-end and script runtime) instead of re-litigating the state machine, whose cost
is reported separately as the engine-only row.

## Method

```
cd native
cargo build -p zcode-workflow --release --bin workflow_engine_bench
./target/release/workflow_engine_bench.exe 50      # window A
./target/release/workflow_engine_bench.exe 50      # window B
```

- Same fixtures, same driver, same `Caps{maxConcurrency:4}`, same in-memory journal, same
  scripted model answers. The sandbox rows execute the byte-identical lowered body that
  `scripts/bench/gen-workflow-fixtures.mjs` produced from `workflow_linear.ts` /
  `workflow_fanout.ts`; the NWS rows execute `bench_linear.nws` / `bench_fanout.nws`, which
  mirror those scripts action for action (one actor, one typed ask, one world read, one
  report, one markdown publish, one return; four joined asks plus one typed meta ask).
- 3 warmups discarded, then 50 timed runs; the report gives median **and** min separately.
- The only stand-in is the model: a scripted HTTP/SSE provider double, the same one on both
  sides and the same one the parity tests use. Everything else is production composition.
- `cargo test -p zcode-workflow --test workflow_parity_e2e` pins the old path's event
  sequences against the captured TypeScript-probe oracle, so both rows here are known-good.

## Results

### Full run, window A (50 runs)

| fixture | runtime | median ms | min ms | events | child process |
| --- | --- | --- | --- | --- | --- |
| `workflow_linear` | Node sandbox (old) | 69.452 | 59.460 | 13 | yes |
| `bench_linear` | NWS interpreter (new) | 1.303 | 1.131 | 13 | no |
| `workflow_fanout` | Node sandbox (old) | 69.543 | 62.382 | 23 | yes |
| `bench_fanout` | NWS interpreter (new) | 0.975 | 0.807 | 23 | no |

### Full run, window B (50 runs)

| fixture | runtime | median ms | min ms | events | child process |
| --- | --- | --- | --- | --- | --- |
| `workflow_linear` | Node sandbox (old) | 62.933 | 57.995 | 13 | yes |
| `bench_linear` | NWS interpreter (new) | 1.246 | 0.972 | 13 | no |
| `workflow_fanout` | Node sandbox (old) | 65.212 | 58.159 | 23 | yes |
| `bench_fanout` | NWS interpreter (new) | 0.906 | 0.706 | 23 | no |

Ratios, computed from the rows above rather than asserted: linear 53.3× (A) and 50.5× (B) on
median; fanout 71.3× (A) and 72.0× (B). The spread between windows is 6–9 ms on the old side
and 0.05 ms on the new side — the old path's floor is process startup, which varies with
whatever the OS is doing, while the new path stays within its own noise.

### Compile-once (the authoring surface: `CreateWorkflow` / `AmendWorkflow` check)

| window | runtime | warm compile median ms | cold / startup ms | Node needed |
| --- | --- | --- | --- | --- |
| A | NWS in-process | 0.061 | 0.068 (first compile in the process) | no |
| A | Node kernel | 66.149 | 387.847 (kernel spawn + handshake, paid once per host) | yes |
| B | NWS in-process | 0.044 | 0.075 | no |
| B | Node kernel | 57.660 | 421.234 | yes |

Both front-ends compile the same single script; the inputs differ by language, which is what
the comparison is about — an author of an NWS workflow pays ~0.05 ms and no process, an author
of a TypeScript workflow pays ~60 ms warm and, on the first script of the session, the kernel
startup too.

### Authoring projection (the preview card's graph, window B)

| case | compile + project median ms | min ms | Node needed |
| --- | --- | --- | --- |
| `bench_linear` + `bench_fanout` (2 and 5 steps) | 0.091 | 0.069 | no |
| 70 awaits on one actor (the capped case) | 0.656 for the whole script | — | no |

Same fixtures, same process, same release build as the compile rows above; `median` sorts in
place so the minimum is read after it, never from the raw sample order (the first sample of a
fresh process is a cold cache, not a floor — an earlier draft of this row reported it as the
minimum and read 0.156 ms). Against the TypeScript authoring path this is the difference
between 0.09 ms and the kernel's ~60 ms warm compile for one script, and it is not a comparison
of the same work: the TypeScript `analyze` also runs a JavaScript taint analysis, which is
exactly the machinery the NWS projection replaces with a tree walk.

### Engine-only row (state machine + journal, no script runtime)

| window | runs | median ms |
| --- | --- | --- |
| A | 200 | 0.051 |
| B | 200 | 0.049 |

This is what the engine costs whichever language is used: `createRun` + `createActor` + typed
ask + report + `complete`, driven directly through the host API. It is also the floor the old
path could never reach, because a sandbox run always paid the child process on top.

## Audit against the recorded history

`specs/tool-benchmark.md` §workflow (recorded earlier in this port, same host, quiet window,
30-run medians) measured the **TypeScript engine** against the Rust engine over the same
lowered bytes:

- engine-only: Rust 0.046 ms vs TS 0.103 ms — **2.24×**;
- full run: 70–80 ms on both sides, min equal at 72.9 ms — recorded honestly as *parity, not
  a speedup*, because the Node child's startup dominated;
- compile-once: same order of magnitude on both sides, since both ran the same TypeScript
  compiler; the resident kernel paid its 389.6 ms spawn once.

Reading that history together with today's rows is the point: the engine port removed the
engine's own cost from the picture (2.24×, then buried under the child), and NWS removes the
child itself, which is where the remaining 60–70 ms lived. The old path's floor was process
startup; the new path has none.

## Fact-based checks beyond timing

| claim | evidence |
| --- | --- |
| the journal vocabulary is unchanged | event counts match per fixture (13 linear, 23 fanout) on both runtimes; `journaled_node_kinds_match_the_engine_vocabulary` and `compiles_the_site_table_in_source_order` |
| site identity is unchanged | `workflow_linear` site table `[actor#1, ask#1, world-read#1, report#1, artifact#1]` equals `bench_linear`'s (`tests/fixtures/bench_linear.nws` header records the mapping); per-kind counters and `phase` carrying no id are pinned by the resolver tests |
| resume still works | `a_completed_run_resumes_from_the_journal_without_re_running_the_world` — replayed nodes are marked `cached: true` and the resumed run issues **zero** world reads; `resuming_a_changed_script_is_refused_by_name` keeps the `ScriptHashMismatch` gate |
| no Node in the NWS path | `an_nws_run_creates_no_sandbox_child_entry` (no `.zcode/workflow-runs` directory), `an_nws_run_settles_through_the_service_without_node` in the host service tests — neither is gated on `node`/`dist` availability, unlike every TypeScript-path test, which keeps its `stack_available()` gate |
| the live TypeScript path is untouched | `a_typescript_script_still_routes_to_the_kernel_stack` (still writes the sandbox entry file), plus the existing suite: `cargo test -p zcode-host --lib -- workflow` 128 passed / 0 failed, `workflow_parity_e2e` 2 passed |
| typed asks still validate and repair | `an_ask_rejection_ends_the_run_with_the_engines_own_code` — a schema-failing submit goes through the engine's repair channel and settles `Errored` with `ValidationFailed` and its violations intact |
| the world surface is unchanged | `a_world_run_command_is_authorized_from_the_compiled_set` — the driver receives the literal command the compiler recorded in `declared_run_commands` |

## What this benchmark does not cover

- **The snippet surface** (`EvalWorkflowSnippet`) still uses the Node kernel and sandbox: the
  scratch facade takes a bare expression body, and NWS v1 has no snippet form (every NWS
  program is a `workflow` declaration). Making snippets Rust-native is separate work, not
  silently claimed here.
- **`js` / node-repl** is unchanged by design: it executes arbitrary JavaScript, which NWS
  does not aim to replace (see `specs/nc-code-workflow-rust.md` Non-goals).
- **The CreateWorkflow preview card's causality graph** is projected for NWS (the row above),
  derived from the lowered tree rather than from a JavaScript taint analysis. What still has no
  NWS counterpart is the *step-level* control-flow view (`flow.nodes` / `flow.edges`): the card
  reads the phase quotient, so emitting a quotient-only projection is honest, and anything that
  wants the finer view needs its own derivation.
- SQLite journal write cost per run is not in these rows (the bench uses the in-memory store,
  as the parity oracle does); it is measured separately in the run-service tests, which use
  the real SQLite journal.
- One host, one OS, release build, no `--repeat` variance study. Windows Defender and normal
  desktop noise are why the old side varies by 6–9 ms between windows.
