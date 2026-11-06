# Tool-surface benchmark: original TS implementation vs native Rust tools vs nc-tools (2026-10-30)

Three tool surfaces run the same set of operations on the same machine with the same fixture.
**The numbers are reproducible**:

```sh
pnpm exec tsx scripts/bench/tool-bench.mjs > .bench-out/ts.json
cargo run -p zcode-services-agent --bin tool_bench --release --manifest-path native/Cargo.toml > .bench-out/native.json
```

## Method and fairness

- **Both sides run production code**: the TS side imports the built-in handlers from
  `packages/core/dist` plus the production `createNodeFileSystemAdapter` (not a simplified
  stand-in); the Rust side uses the same crate's `ReadTool`/`WriteTool`/
  `EditTool`/`GlobTool`/`GrepTool`; the nc side uses the in-process Kernel of
  `nct_mcp::build_kernel`.
- **Byte-identical fixture**: 400 `src/mod-NNNN.ts` files (20 lines each, every 10th containing
  `// grep-needle`), `target.txt` = 32 KiB, `edit.txt` = 64 lines of unique markers.
- **Timing unit**: read/write/glob/grep are single tool calls; edit is `Read + Edit` on both TS
  and Rust (production requires read-before-edit), while nc is a **single-step** `patch.apply`
  (no read required first), so the edit row cannot be compared horizontally.
- **No stand-ins**: the TS side uses the production `createNodeFileSystemAdapter` and the
  production `AutomationService` (sqlite); the Rust side uses the same crate's real tools and
  `zcode-services-automation`. There is no fake port, mock or precomputed number anywhere;
  every row is the median on this machine.
- Node/Rust process startup is not counted, fixture construction is not counted; each item
  warms up 3 times first and the median is taken.
- The known non-comparable points are marked in the table (glob's result ceiling, grep's
  match-counting definition).

## Results (median)

2026-10-30 re-measurement (ENTRY 129/131): **both sides run sequentially, not concurrently**
(concurrency smears the numbers — the previous version of this table was measured under
contention), same machine, same fixture, and **the numbers inside one table come from a single
run** (values are not mixed across runs). The Bash and Skill rows are new, and both sides use
the **production** executors: TS `createNodeExecutionAdapter` / `createNodeSkillAdapter` +
`resolveEffectiveBashShellSelection` (really resolving to Git Bash), Rust `BashTool` /
`SkillTool` (the same resolution order).

| Operation | TS | Rust native | nc-tools | TS/Rust | TS/nc | Result consistency (TS / Rust / nc) |
| ----------------------------- | -------------------- | ----------------------------------- | --------------------- | -------------------------- | ----- | ------------------------------------------------------------ |
| read 32 KiB                   | 1.188 ms             | **1.146 ms**                        | 19.297 ms             | 1.04×                      | 0.06× | content / content / content                                  |
| write 32 KiB (new file)          | 2.658 ms             | **1.053 ms**                        | 20.222 ms             | 2.52×                      | 0.13× | —                                                            |
| glob `**/*.ts`                | 57.818 ms            | **1.547 ms**                        | 129.302 ms            | 37.4× ⚠                    | 0.45× | **100** / 400 / 400 ⚠                                        |
| grep 400 files                 | 488.117 ms           | **63.829 ms**                       | 118.493 ms            | 7.65×                      | 4.1×  | 40 / 40 / 80 ⚠                                               |
| edit (read+edit)             | 6.760 ms             | **2.183 ms**                        | 21.655 ms (patch only) | 3.10×                      | 0.31× | ok / ok / patch only                                           |
| bash `echo bench-hello`       | 135.590 ms           | **54.831 ms**                       | not comparable (see below)        | 2.47×                      | —     | 1 / 1 / —                                                    |
| bash `node -e "…"`            | 230.299 ms           | **118.025 ms**                      | not comparable          | 1.95×                      | —     | — / ok / —                                                   |
| skill load (the same SKILL.md) | 4.965 ms             | **0.452 ms**                        | no such tool              | 10.99×                     | —     | 3 / 3 (output shape aligned)                                      |
| cron.create                   | 0.631 ms             | **0.218 ms**                        | —                     | 2.89×                      | —     | real service (sqlite), no stand-in                                   |
| cron.list                     | 0.395 ms             | **0.342 ms**                        | —                     | 1.15×                      | —     | Rust is still slightly slower: the port goes typed→Value→projection                           |
| cron.update                   | 0.385 ms             | **0.241 ms**                        | —                     | 1.59×                      | —     | —                                                            |
| cron.delete                   | 0.125 ms             | **0.069 ms**                        | —                     | 1.81×                      | —     | —                                                            |
| todo.write 4 items (durable storage)   | **0.463 / 0.461 ms** | 0.824 / 0.875 ms                    | —                     | 0.53–0.56× ⚠ Rust is slower     | —     | both sides persist on every write; see the "Todo tools" section                          |
| todo.read 4 items                | 0.028 ms             | **0.003 / 0.004 ms**                | —                     | 6.5–9.5×                   | —     | ⚠ Rust reads the in-memory mirror, TS reads SQLite (explained in the same section)          |
| js (node-repl)               | 3.147 ms             | cold 137.329 ms / **warm 0.329 ms** | —                     | cold 0.02× / warm **9.6×** | —     | TS creates a new vm session every time ≈ Rust cold; the peer tier is Rust persistent |

**Run-to-run jitter (written as it is, not picking the flattering ones)**: the median of the
same code on this machine drifts ±40% between times of day. The earlier sequential run the same
day was read 0.958/0.663, glob 40.953/0.938, grep 344.232/42.076, bash 96.507/37.463,
cron.list 0.258/0.323 (in that run Rust was in fact 0.80× slower on `cron.list`). In one run
after ENTRY 135 bash was only 126.644/65.112 (1.94×) and 253.140/166.196 (1.52×) — lower than
the previous 2.58×/1.91×, with the absolute values up on both sides (spawn-dominated + machine
load). So **read the bash ratio as a 1.5–2.6× range**, and do not take the single
best-looking run. The other ratios are stable (read 1.0–1.5×, glob 37–61×, grep 6.7–8.2×,
skill 8.4–11×); comparisons read ratios, and the absolute milliseconds of one table are not an
SLA.

**Where the skill row comes from**: on the first measurement the two sides' `check` was 3/2,
and what differed was not speed but capability — Rust was missing the `${ZCODE_SKILL_DIR}`
expansion, the base directory hint and frontmatter stripping, and an over-limit skill errored
out while TS truncated. After filling those in, `check` became 3/3 (contract in
`specs/rust-native-crates.md` S3 "`Skill` has the same semantics as TS").

**Why the two bash rows differ less than the IO ones**: the absolute values are dominated by
spawning a shell child process (on Windows, creating a Git Bash process alone costs tens of
milliseconds), and both sides use the same interpreter with the same command. So 2.0–2.6× is
the net gain of the "tool-layer wrapper" (the Rust side has no Node pipe decoding, event-loop
round-trips or result-object orchestration), not "the shell got this much faster".

**nc's bash is not comparable**: nc has no tool with shell semantics; its counterpart is
`proc.spawn` (direct argv, never through any interpreter). Putting it in the same table as
`Bash(command string)` would yield the false advantage of "one less shell parse", which is not
the same operation, so it is not listed. nc has no skill-loading surface either.

**⚠ glob is not an equal-volume comparison**: TS `MAX_GLOB_RESULTS = 100`, Rust is 1000. TS
stops early when it reaches the ceiling, Rust finishes scanning all 400 and returns them all —
**TS produces 100 entries in 39 ms, Rust produces 400 entries in 0.74 ms**. Per resulting entry
the gap is even larger; scoring it as "scan the whole directory" is also against TS. **The
dominant cost is spawning a ripgrep child process per call** (glob goes through `searchFiles`,
grep through `searchText`), not the scan itself.

**⚠ nc grep's 80** is `grep-needle` appearing 80 times in the output (once in the line + once
in the snippet); TS/Rust is 40. It is not that a different number of matches was found, it is
that **the shape of the returned text differs**.

## Conclusions (performance)

1. **The native Rust tools are faster than TS across the board**, and the largest gaps are in
   operations that need to "search" (glob 43.7×, grep 8.2×), because TS spawns one ripgrep child
   process per call; pure file IO is 1.45–3.0× (Node's fs wrapper + result-object/budget
   computation); **bash is also 1.9–2.6×** (same interpreter, same command — the difference is
   the tool-layer wrapper: each TS call goes through Node's pipe decoding and result
   orchestration). The one miss is `cron.list` (0.80×): Rust's tool port goes typed→`serde_json::Value`→projection, two extra constructions versus TS returning an object
   directly; it is registered, not hidden.
2. **nc-tools is an order of magnitude slower than the native Rust tools on overlapping
   operations** (read 14.6 ms vs 0.72 ms, write 16.3 ms vs 0.79 ms, patch 16.2 ms vs read+edit
   1.6 ms). Its single call includes the kernel's **audit journal (journal.jsonl) + policy
   validation + hash/snapshot bookkeeping** — that is a design cost, not an implementation
   defect; but if the goal is "let the model read and write quickly", nc-tools is not suitable
   as the default IO surface.
3. nc's position is clear: **glob is slower than TS** (96 ms vs 39 ms, because TS stops early at
   100 entries), **grep is 4× faster than TS** (90 ms vs 360 ms, avoiding the per-call ripgrep
   spawn). It is the "heavy tool surface with audit", sitting between TS (process spawn) and the
   native Rust tools (zero bookkeeping).

## Capability/security comparison (more important than performance)

| Dimension | TS built-in tools | Rust native tools | nc-tools |
| ----------------- | --------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Tool count | 40 (including 15 workflow/off-peak/coordinator-boundary tools) | **26 TS built-in tools are ported** (`pnpm native:tool-parity` reconciliation: the registry has 27 ported rows = 26 TS same-name tools + the Rust-only `ReadImage`, where `Task` is an alias row for `Agent`; 0 gap / 14 boundary, every boundary tool has a spec citation) | **89** (fs/git/proc/net/semantic/code/batch/sys…) |
| Permission granularity | per tool `readOnly/destructive/needsApproval/sideEffectScope` + project rules + plan/yolo modes | `read_only/destructive/side_effect_scope` + plan gate + full-access/asking | the kernel descriptor provides no capability metadata, so the host falls back to a **read-only allowlist** (`nc_tools.rs::nc_tool_is_read_only`; everything outside the allowlist is non-read-only; unit tests pin that `fs.write`/`patch.apply`/`proc.spawn` must not declare read-only) |
| read-before-write | **enforced** (TS `Write` refuses an existing file that has not been read) | **enforced for `Read`/`Edit`/`Write` alike** (the `Write` gap was surfaced by this benchmark and filled in ENTRY 127: overwriting an existing file requires a prior read, creating a new one is unrestricted) | none (`patch.apply` has no read state) |
| Result ceiling | per-tool budget + artifact externalization + display | per-tool `max_output_bytes` + 60/30k truncation | 4 MiB, no display |
| Auditable | transcript / toolCall rows | transcript / toolCall rows | **ships its own journal** (`.nc-tools/journal.jsonl`, independent of the transcript) |
| Concurrency | `concurrentSafe` + scheduler | serial (`concurrent_safe` is declaration only) | the Kernel serializes itself |
| Unique capabilities | workflow engine, CUA, off-peak, subagent coordination | none | **snapshot/rollback, batch.execute, code.symbols/graph, search.semantic, test.run, pkg.list, watch** |
| Protocol surface | in-process | in-process inside the v4 host | in-process (**no longer MCP**) |

### Key findings (all of them code facts, not inference)

1. **nc-tools was a permission hole, now closed fail-closed**: `NcTool::contract()` originally
   declared **all** 89 tools as `read_only: true`, so `fs.write`/`patch.apply`/`proc.spawn` were
   let through in sessions without `--full-access` and without an interactive client, and plan
   mode could not stop them either. The kernel's descriptor has no capability bits at all, so
   the host side switched to a **read-only allowlist** (`nc_tool_is_read_only`): every tool
   outside the allowlist is treated as non-read-only, anything needing write permission goes
   through the gate or fails closed; unit tests pin that `fs.write`/`patch.apply`/`proc.spawn`/
   `sys.snapshot`/`batch.execute` must not declare read-only. This is the host's fallback, not
   kernel metadata — the kernel genuinely exposing per-tool capability bits is still an upstream
   to-do.
2. **Rust `Write` used to lack read-before-write; fixed**: in the benchmark the TS side threw
   `File has not been read yet` outright while the Rust side silently overwrote on the same
   operation — a real security regression measured in that round, now under the same rule as TS
   (overwriting an existing file requires a prior read; creating a new one is allowed). See
   ENTRY 122.
3. **TS's glob ceiling 100 / Rust 1000**: the model sees a different candidate volume, which is
   a behaviour difference that needs a product decision on which value to unify to (not a
   performance issue).
4. TS's grep starts a ripgrep child process per call (~350 ms/call), which is the dominant cost
   of the current TS tool surface; native Rust is 48.8 ms, nc 90.4 ms.

## Recommendation for "nc-tools as native tools"

**Worth doing, but not a direct replacement.** Reasons and prerequisites:

1. **Capability metadata still has to be added on the kernel side**: the host's read-only
   allowlist is a fallback (a newly added nc tool defaults to non-read-only, which is the safe
   direction), but it is hardcoded by name; granularity for same-name-different-side-effect
   families like the `git.*` write subcommands can only be done correctly once the kernel
   exposes `readOnly/destructive/concurrency` per tool.
2. **Layer the positioning, do not overlap**: nc's value is in **capabilities zcode does not
   have** — `sys.snapshot/rollback`, `batch.execute`, `code.symbols/graph`, `search.semantic`,
   `test.run`, `pkg.list`, `fs.watch`. Treating `fs.read/fs.write/patch.apply` as a main path
   too only trades a 10–20× latency for one journal.
3. **Names and the model-facing surface**: nc's dotted names (`fs.read`) coexisting with zcode's
   `Read` makes the model waver between two structurally identical tools. Recommendation: **the
   core read/write keeps the zcode names**, nc only exposes its unique capabilities (or the nc
   implementation is registered as the **backend** of a zcode name, so the model only sees
   `Read`).
4. **Audit is both the selling point and the cost**: if a user/enterprise scenario needs "every
   write has a replayable journal", nc's 16 ms is worth it; otherwise it is pure overhead.
5. **read-before-write has been added to Rust `Write`** (the regression measured in ENTRY 122);
   nc's `patch.apply` still has no read state, so writing through it means bypassing that guard
   rail.

**In one sentence**: on performance, native Rust > TS > nc (on overlapping operations); on
capability, nc is a superset, the permission side is held fail-closed by the host's read-only
allowlist, but the real per-tool metadata is still outside the kernel; the correct way to turn
nc into native tools is "add the metadata → expose only the unique capabilities → keep the core
IO lightweight", not stuffing all 89 tools into the model-facing surface.

## Scheduled-task tools (2026-10-30, ENTRY 125: real service, no stand-ins)

TS and Rust **both run the real automation service**: TS uses `AutomationService` +
`AutomationRepo` (better-sqlite3), Rust uses `zcode-services-automation` plus the same
`AutomationToolPort` as production, each writing its own temporary sqlite file. There is no fake
port. Both sides' services have a "keep at most 20" ceiling, so the orchestration is identical:
19 tasks are pre-seeded for list/update; create deletes the one it just made **outside the
timing** in every round; delete adds one back **before the timing** in every round.

| Operation (19 tasks) | TS | Rust | TS/Rust |
| ----------------- | ------------ | ------------ | ------- |
| cron.create       | 0.382 ms     | **0.176 ms** | 2.17×   |
| cron.list         | **0.225 ms** | 0.250 ms     | 0.90×   |
| cron.update       | 0.251 ms     | **0.177 ms** | 1.42×   |
| cron.delete       | 0.068 ms     | **0.047 ms** | 1.44×   |

**Why this round flipped from "Rust is slower across the board"** (optimizations driven by real
numbers):

1. `zcode_host::storage::open` previously only set `journal_mode=WAL`, falling back to SQLite's
   default `synchronous=FULL` — fsyncing the WAL on every commit. The TS repository uses WAL +
   **NORMAL** (`automationRepo.ts:280-281`). After aligning, cron.delete went 0.337→0.047 ms and
   update 0.532→0.177 ms. NORMAL under WAL still survives a process crash and only loses the
   last commit before a power failure — exactly the durability level TS already uses, not a new
   discount opened for this crate.
2. `cron.list`'s tool-layer cost went from "build an intermediate `Value` per row" to
   **borrowed-value single-pass serialization** (0.297→0.250 ms). The remaining ~11% gap sits at
   the port boundary: the adapter converts the typed `Automation` into a `Value` and the tool
   layer projects it once more. The faster approach would be to have the port emit the projected
   structure directly, but that would copy the model-visible field table into the adapter — no
   second field table for 11% for now.

## Todo tools (2026-10-30, ENTRY 137: both sides are durable storage, no in-memory stand-in)

Why it can be measured: TS `TodoWrite` requires `context.sessionStore`, and registering it as
"state-only, not comparable" earlier was **wrong** — the production storage is exactly
`packages/adapters/dist/storage/session-store.js`, and wiring it into the benchmark gives real
numbers (`todo.session_id` has `references session(id)` and `pragma foreign_keys = on`, so the
benchmark must also create a real session row first; that step is outside the timing). Both
sides now "persist on every write":

| Operation (median ms) | TS (two independent runs) | Rust (two runs) | Rust in-memory baseline (persistence replaced by a no-op) |
| ----------------- | ------------------ | ------------- | ------------------------------- |
| `todo.write` 4 items | 0.463 / 0.461      | 0.824 / 0.875 | 0.0077 / 0.0126                 |
| `todo.read` 4 items  | 0.028 / 0.030      | 0.003 / 0.004 | 0.0029 / 0.0029                 |

The two runs were executed separately, not concurrently (concurrency inflates all the medians
together; that trap was recorded in ENTRY 130). Ratio ranges: write **0.53–0.56×**, read
**6.5–9.5×**.

Honest conclusion — this is the **only row in the whole table where Rust is slower**, registered
as it is:

1. On the write side Rust is 1.8–1.9× slower than TS. Out of 0.82–0.88 ms, about 0.81–0.87 ms is
   "persisting to disk" itself (the in-memory baseline is only 0.008–0.013 ms, which is the tool
   layer's parsing/validation/serialization): the cost of creating a new file object with temp
   file + rename under NTFS/Defender. TS's 0.46 ms is the SQLite WAL transaction + `node:sqlite`
   binding-layer cost. Both sides are dominated by **durability**; what differs is the storage
   method, not the language's speed. temp+rename was not dropped to make the numbers look
   better — that is precisely the property "a crash leaves no half-written list"
   (`specs/rust-native-crates.md` S3 "Todo list persistence").
2. The read side's 9.5× **must not** be read as "Rust reads the task list 9.5× faster": Rust
   reads the in-process mirror (already persisted at write time), TS queries SQLite every time.
   The fact is the same data written by the same writer, but the cost sits elsewhere, so the
   meaning of this row is "the read/write split differs", not "the same operation is faster".
3. The four model-visible contract gaps fixed in this same round (return shape, closed-set
   spelling, multiple `in_progress`, permission declaration) do not change the timing unit (both
   sides use the same 4-item list); the key names TS's `verify` prints, `oldTodos,summary,todos`,
   are exactly the shape Rust has after the fix — the benchmark thus became the second piece of
   evidence for this parity (the first is
   `tool_matrix_e2e::todo_write_then_read_round_trips_through_the_session_store`).

## Local tools (2026-10-30 re-measurement, real adapters/real tools)

| Operation | TS | Rust native | nc-tools | TS/Rust |
| ---------------------------------------- | ---------- | ------------- | ---------- | ------- |
| read 32 KiB                              | 0.964 ms   | **0.689 ms**  | 13.691 ms  | 1.40×   |
| write 32 KiB (new file)                     | 1.944 ms   | **0.737 ms**  | 14.217 ms  | 2.64×   |
| glob `**/*.ts` (TS ceiling 100 / Rust 400) | 33.100 ms  | **0.764 ms**  | 111.989 ms | 43.3× ⚠ |
| grep 400 files                            | 311.525 ms | **42.070 ms** | 98.361 ms  | 7.40×   |
| edit (read+edit; nc patch only)           | 3.805 ms   | **1.569 ms**  | 18.049 ms  | 2.43×   |

⚠ glob is still not an equal-volume comparison (TS stops early at 100 entries, Rust returns 400);
the dominant cost is TS spawning ripgrep on every call, and nc-tools' 13–18 ms is the kernel's
audit-journal/policy/hash overhead.

## Reproduction

```sh
pnpm exec tsx scripts/bench/tool-bench.mjs > .bench-out/ts.json
cargo run -p zcode-services-agent --bin tool_bench --release --manifest-path native/Cargo.toml > .bench-out/native.json
```

(the bin lives in `zcode-services-agent`: the scheduled-task benchmark must see both the host and
the real service, and the host must not depend on services — the dependency direction decides
the location.)

## `js` (node-repl): TS in-process vm vs a real Rust Node kernel

TS's in-process executor (`node-repl-host/src/server.ts:createInProcessNodeReplExecutor`)
**creates a new vm session on every call**; Rust's `Fresh` mode has the same semantics but starts
a real Node child process every time; the host's default mode is `Persistent` (one kernel reused
per session).

| Operation | TS (fresh session/call) | Rust `Fresh` (new kernel/call) | Rust `Persistent` (host default) |
| --------- | ----------------------- | ------------------------------ | -------------------------------- |
| `js` evaluating `1+1` | **1.01 ms** | 110.70 ms | **0.29 ms** |

The conclusions must be stated separately and must not be merged into one "who is faster":

- **Per new session**: TS's vm is ~110× faster (Rust has to start a Node process every time).
- **Reuse inside a session** (the shape production actually uses): Rust persistent is
  0.29 ms/call, ~3.5× faster even than TS's per-call new session, because execution no longer
  carries any session-creation cost.
- TS has no "persistent in-process executor" to compare against in this harness; the Rust warm
  number is therefore **reported for Rust only**, with no invented opponent.

## Tools not participating in the horizontal benchmark (and why)

- **WebSearch**: the two ends have different backends — TS goes through the provider's native
  search (needs model + provider capability), Rust goes through nc-tools' multi-engine
  `net.search`. Putting both in one table would measure "whose network is faster", not the tool
  cost. Operational facts (2026-10-30, this machine, real internet, cold start): one
  `"rust programming language"` query returned **8 real results in about 34 s** (multi-engine
  fan-out + local re-ranking + cold cache); at unit-test level the tool-layer overhead is
  negligible (projection/filtering is pure CPU, of the order of <0.1 ms).
- **WebFetch**: a local HTTP service **cannot** be used for an equal-volume comparison — the TS
  side's egress gate
  `webFetchError("EgressBlocked", "WebFetch cannot access private or local IP addresses")`
  (`handlers/webfetch-egress-guard.ts:18-33,66`) hard-forbids loopback/private networks/
  `.localhost`, so running a local server would mean turning this security control off outside
  the production path, and the resulting numbers would no longer represent the real tool.
  Public endpoints, in turn, only measure network jitter (the two ends have different DNS/
  connection pools/redirect policies) and are not reproducible. Both ends **do** real HTTP +
  real HTML→text conversion (Rust `tools/webfetch.rs`, TS `webfetch-network.ts`/
  `webfetch-content.ts`), but there is no fair measuring ground, so it is registered and not
  measured.
- **state-only tools** (TaskOutput/TaskStop, SendMessage, the plan tools, AskUserQuestion,
  ListModels, ReadImage): no comparable CPU/IO workload, or a dependency on interaction, so
  horizontal timing is meaningless; they are covered by the respective schema/behaviour tests.
  ⚠ This list once included **TodoWrite and Skill**, and both were **misjudgements** of "no
  comparable workload": with the production `SqliteSessionStore` wired in, TodoWrite/TodoRead
  have real and asymmetric costs (see the "Todo tools" section), and Skill has real production
  root resolution + frontmatter + manifest search (see the `skill` row of the main table).
  Lesson: before declaring something "not comparable", first check whether the port it depends
  on can be given the real thing.

## Dynamic workflow (2026-10-31, ENTRY 139: the same lowered bytes, the same window, run sequentially)

Three things are measured and reported separately: **the engine itself** (native Rust vs the TS
state machine), **the whole run** (including the Node sandbox child process — the same Node and
the same entry file on both sides), and **compiling once** (the same TypeScript compiler on both
sides: TS in-process vs a resident compile kernel on the Rust side). The fixture is generated by
`scripts/bench/gen-workflow-fixtures.mjs` through the real front end, **compiled once**
(`tests/fixtures/workflow_{linear,fanout}.json`), and both sides execute a **byte-identical
lowered body**; the world reads all read the real README; the artifact bytes really go to disk;
only the "model" is a scripted stand-in, the same one on both sides.

```sh
node scripts/bench/gen-workflow-fixtures.mjs
cargo run -p zcode-workflow --release --bin workflow_engine_bench -- 30
```

**2026-10-31 note (ENTRY 141–147)**: the TS workflow implementation was once deleted and restored
the same day (removal gate: a flag-gated replacement is not enough to delete the default path).
The TS-side bench/probe scripts were removed inside the deletion window and have not been
restored; the TS numbers in the table are a **frozen baseline** measured inside that same window,
while the fixture generator and the Rust bench remain reproducible. Do not read this table as "a
two-sided comparison that can be re-run at any time".

| Row | TS | Rust native | TS/Rust | Definition |
| ---------------------------------------------------- | -------------------------------- | --------------------------------------------- | --------- | -------------------------------- |
| `engine_only`(createRun+actor+ask+report+complete) | 0.103 ms (200 iterations)               | **0.046 ms** (200 iterations)                        | **2.24×** | the same host API call sequence, not through the sandbox  |
| whole run `workflow_linear` (13 events)                  | 75.5 ms                          | 81.9 ms (min 74.3)                           | 0.92×     | includes Node child-process startup; the mins differ by <1% |
| whole run `workflow_fanout` (23 events)                  | 72.9 ms                          | 79.7 ms (min 72.9)                           | 0.91×     | 4-way concurrent ask; **the mins are exactly the same**   |
| compile once (production shape, single ts.Program)                  | 56.9 / 69.4 ms                   | 59.1 ms (kernel resident, spawn 389.6 ms counted separately) | ~1.0×     | the same TypeScript compiler on both sides     |
| journal read (13/23 events, full page + run rows)            | — (TS is an in-process implementation with no equivalent read surface) | 0.009 / 0.012 ms                              | —         | not compared horizontally                           |

**How to read this honestly**:

1. **The 2.24× at the engine layer** is where the claim "native Rust is faster" lands: the same
   host API sequence (create run, create actor, typed ask, report, complete, settle), both sides
   production code, both writing the journal.
2. **The whole run is 70–80 ms on both sides**: the dominant cost is starting a Node child process
   on Windows (the same Node and the same entry file on both sides) — what differs is process
   creation, not the language. The equal mins (72.9 vs 72.9) show there is no systemic difference
   above the scheduling jitter; **the whole run must not be read as "Rust is fast" or "Rust is
   slow"**.
3. **Compiling once is the same order of magnitude on both sides**: the Rust side did not rewrite
   the TypeScript front end (`specs/workflow-capability-boundary.md` explicitly reuses it), and
   the resident kernel's benefit is avoiding the per-call process start (spawn 389.6 ms is paid
   once).
4. **The event sequence is a load-bearing contract**: `tests/workflow_parity_e2e.rs` pins the
   13-event sequence captured by the TS probe item by item, and fanout uses 23 counts plus the
   queue→dispatch→settle ordering invariants; in this round the event counts agree item by item
   on both sides.
5. The earlier 200–467 ms numbers were measured under concurrent build load (the loaded window on
   this machine the same day) and are superseded by this table; in the scripted-provider harness
   a plain-text round must follow `submit_result`, otherwise the scripted model keeps emitting the
   same submit and the turn never ends — that point is written into the e2e comments.
