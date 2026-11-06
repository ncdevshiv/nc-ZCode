# nc-code Workflow Script (NWS) — language reference

NWS is the workflow authoring language the nc-code backend compiles and runs in Rust. It is
TypeScript-shaped on purpose: an author who knows TypeScript writes it immediately. It is not
TypeScript: there are no user-defined functions, no arbitrary JavaScript, no imports, and no
Node APIs, because the compiler must be able to see every facade call and give it a journal
identity.

Everything below is the language as implemented in `native/crates/zcode-workflow/src/script/`
and executed by its in-process interpreter. `specs/nc-code-workflow-rust.md` records why the
language exists and what it replaced.

## Program shape

```
schema <Name> {
  field: string
  count: number
  flag: boolean
  items: string[]
  nested: OtherSchema
  optionalField?: string
}

workflow "<name>" {
  args { repo: string, limit: number }

  // statements
}
```

- Zero or more `schema` declarations, then exactly one `workflow`. A program without a
  `workflow` is a parse error.
- `schema` names must be unique, fields unique within a schema, and the schema tree must be
  finite: a schema may not refer to itself, directly or through an array element (`nws_recursive_schema`).
- `args { … }` is contextual: written as a block right after the workflow opens, it declares the
  run's arguments; anywhere else `args` is the value those arguments are delivered in.
- Semicolons end statements (`phase "x";`, `let a = …;`, `return x;`). Blocks `{ … }` do not.

## Types

| NWS | JSON Schema it compiles to |
| --- | --- |
| `string` | `{"type":"string"}` |
| `number` | `{"type":"number"}` |
| `boolean` | `{"type":"boolean"}` |
| `T[]` | `{"type":"array","items":…}` |
| `SchemaName` | a closed object: `{"type":"object","properties":{…},"required":[…],"additionalProperties":false}` |

An optional field (`field?: string`) is omitted from `required`, never expressed as a nullable
type. Objects are always closed: a result carrying a key the schema does not declare is a
violation, which turns a misunderstood ask into one clear repair prompt.

## Statements

| statement | meaning |
| --- | --- |
| `phase "plan";` | opens a named stage. A marker only: no site id, no journal key of its own; it stamps the instances born after it |
| `let x = …;` / `let x: T = …;` | one value per declaration. Redeclaring in the same block is an error; shadowing an outer block is legal |
| `if (c) { … } else if (c) { … } else { … }` | conditional |
| `while (c) { … }` | loop |
| `for (const item of seq) { … }` | iterates an array by element, an object by its keys, a string by its characters |
| `parallel { { … } { … } }` | structured concurrency: each `{ …}` group is one strand, all strands run together, the block ends when every strand has |
| `report(item);` / `report(item, "board");` | one journaled progress finding, optionally routed to a declared card |
| `return expr;` / `return;` | the run's output value. Not legal inside a `parallel` strand (`nws_return_in_parallel`) |
| `{ … }` | a statement group sharing one declaration scope |
| `try { … } catch (err) { … }` | recovers from one rejected facade call; `err` is the engine's structured error as JSON |
| `await artifact.markdown(…);` | an expression statement whose effect is a facade call |

## Facades

Only these exist. Unknown identifiers are compile errors — NWS has no globals beyond these names.

### Actors and asks

```
let planner = agent("planner", "You write short, concrete plans.");
let plan: Plan = await planner.ask("Draft a plan for " + args.repo);
let one: Plan = await planner.ask("Draft it", Plan);
```

- `agent(name?, persona?)` — synchronous. `persona` is a string (system prompt) or an object
  `{ system: "…" }`. The name must be unique within a run; it is the identity an amended re-run
  matches its imported cache against.
- `<actor>.ask(instruction)` — asynchronous, one instruction string. The receiver must be an
  actor; `.ask` on a JSON value is `nws_ask_receiver_not_an_actor`.
- **A typed ask is either annotated** (`let plan: Plan = await …`) **or given the schema as the
  second argument** (`await …ask("…", Plan)`). The second form exists because a `Promise.all`
  item has no annotation to read from. An unannotated ask is untyped and is answered by the
  actor's final text.

### World reads

```
let readme = await files.read("README.md");
let paths  = await files.glob("src/**");
let hits   = await files.grep("TODO", "src/**");
let dirty  = await git.status();
let diff   = await git.diff("HEAD", "src/app.ts");
let recent = await git.log(10);
```

| call | arguments |
| --- | --- |
| `files.glob(pattern)` | 1 |
| `files.read(path)` | 1 |
| `files.grep(pattern, glob?)` | 1–2 |
| `git.changedFiles(base?)` | 0–1 |
| `git.diff(base?, path?)` | 0–2 |
| `git.status()` | 0 |
| `git.log(count?)` | 0–1 |

`git.log` is a world read and the progress function `log(…)` is a different thing; the compiler
tells them apart by their container, not by their name.

### Commands

```
let outcome = await world.run("cargo test", ["-q"], { timeoutMs: 120000 });
```

`world.run(cmd, args?, opts?)` — the command must be a **string literal** (`nws_literal_required`):
the set of commands a run may execute is fixed at submit time and confirmed with the user, so a
computed command cannot be authorized. The result is `{ stdout, stderr, code }`.

### Artifacts

Content family — effects, awaited, resolve to `{ id, version }`, reject catchably:

```
let ref = await artifact.file("report", "out/report.pdf", { title: "Report", primary: true });
await artifact.markdown("summary", "## Plan\n\n" + plan.rationale);
```

Preset family — declarations, synchronous, one per id, declared once (conventionally at the top),
then fed by `report(item, "<id>")`:

```
artifact.board("summary", { key: "repo", status: "state", columns: ["planned", "done"] });
artifact.table("progress", { columns: [{ field: "name", label: "File" }] });
artifact.metrics("throughput", { metrics: [{ field: "found", label: "Found" }] });
artifact.chart("trend", { x: { field: "round" }, y: [{ field: "found" }], type: "line" });
```

| call | arguments |
| --- | --- |
| `artifact.file(id, path, opts?)` | 2–3 |
| `artifact.markdown(id, content, opts?)` | 2–3 |
| `artifact.chart(id, spec)` | 2 |
| `artifact.table(id, spec)` | 2 |
| `artifact.metrics(id, spec)` | 2 |
| `artifact.board(id, spec)` | 2 |

Every artifact `id` is a literal of 1–64 characters from `[A-Za-z0-9_.-]`, and one id belongs to
exactly one member: using the same id for `markdown` and then `file` is `nws_artifact_kind_mismatch`,
and declaring it as two different presets is `nws_artifact_redeclared`. Caps: 32 ids per run, 16
versions per id, 20 MiB per file, 256 KB per markdown, 120 characters of title, 500 of description.

### Progress

```
log("indexed 128 files");                                   // text only, not journaled
report({ file: name, findings: v.findings }, "progress");    // journaled, survives a failed run
```

`report(item, tag)`'s tag must name a preset declared somewhere in the script
(`nws_undeclared_artifact_tag`); drop the tag to report into the plain results list. A run keeps
at most 256 reports and 32 KB per item.

### Joins

```
let results = await Promise.all([
  alpha.ask("review one"),
  beta.ask("review two"),
  files.read("README.md")
]);
```

`Promise.all([...])` takes exactly one **array literal** of async facade calls, every item of
which must be such a call (`nws_join_item_not_async`, `nws_join_needs_array_literal`). Results
come back in written order. An empty list is refused (`nws_empty_join`) — it waits for nothing.
`Promise.race` does not exist: a race has no deterministic journal identity.

## Values and operators

Values are JSON: strings, numbers, booleans, null, arrays, objects. An actor handle is a value
too, but only ever reaches `.ask` through a binding the compiler proved holds one.

| operator | semantics |
| --- | --- |
| `+` | concatenates when either side is text, adds otherwise (`"a" + 1` → `"a1"`, `1 + 2` → `3`) |
| `- * / %` | numbers only; a text operand is a named runtime error rather than a coercion |
| `== !=` | deep JSON equality; `1 == 1.0` |
| `< <= > >=` | two numbers or two strings; mixed comparison is refused |
| `&& \|\|` | short-circuit, and return an operand, not a boolean — `let t = args.title \|\| "untitled"` works |
| `! -` | boolean negation; numeric negation |
| `x.length` | array count, string character count |
| `obj.key`, `arr[i]`, `str[i]` | a missing key or an out-of-range index is `null`, not an error |
| `` `text ${expr}` `` | interpolation; every part renders as text |

There is no assignment statement: a slot holds what its `let` gave it, which is what lets a
`parallel` strand take a copy of the frame instead of sharing it.

## Awaiting rules

A facade call that returns a value the script does not have yet — `ask`, a world read, a
`world.run`, a content publish, `Promise.all` — must be awaited at the call, or be an item of a
`Promise.all`. Everything else is already a value: `await 1` is `nws_await_misplaced`.

This is stricter than the TypeScript dialect, where `Node<T>` was a real promise you could store
and await later. NWS keeps the rule because a value the compiler cannot see being awaited is a
call whose completion order it cannot reason about, and the journal is keyed on that order. The
ways to be concurrent are `Promise.all([…])` and `parallel { … }`, both of which state their
concurrency in the source.

## Recovery

```
try {
  await artifact.file("report", "out/report.pdf");
} catch (err) {
  let written = await writer.ask("Write out/report.pdf, then tell me it exists");
  await artifact.file("report", "out/report.pdf");
}
```

A facade call the engine refuses — an ask rejected after its repair attempts, a content publish
whose file is missing, a world read over its cap — rejects that one call. Inside `try` the
rejection is caught, and the binding is the engine's own error shape as JSON:
`{ code, message, violations? }`, so a script branches on `err.code` (`"ValidationFailed"`,
`"ArtifactSourceMissing"`) instead of on prose. Outside a `try` the same rejection ends the run
with that code intact.

Three limits, all deliberate:

- **A `return` inside a try body is not an error.** It unwinds the workflow and the catch body
  does not run, so a successful path can never be mistaken for a failure.
- **No `throw`, no `finally`.** NWS has no user-defined exceptions to throw, and nothing in the
  language holds a resource that needs releasing.
- **`catch` does not resume a stopped run.** After a cancel or an external settle, later calls
  keep failing with `Cancelled`; the catch body sees that code and has no work to continue into.

## Site identity

Every facade call gets a static site id from a per-kind counter in source order:

```
actor#1  ask#1  world-read#1  report#1  artifact#1  join#1
```

A call inside a loop or a re-run phase produces one site executed many times, and the journal
keys each execution by `(siteId, ordinal)` — `ask#1@1`, `ask#1@2`, … That identity is what makes
resume replay completed work instead of re-asking the model, and it is assigned by the compiler,
never at run time, so a resumed run matches the journal its predecessor wrote.

`phase` markers and `log` consume no counter and no id: adding one to a script must not move any
other site's id.

## Diagnostics

Diagnostics are `L<line>:C<column> <code> <message>`, and any diagnostic refuses the run — no run
row is created for a script that is not clean.

| code | means |
| --- | --- |
| `nws_lex_error`, `nws_parse_error` | the text is not NWS |
| `nws_unknown_identifier` | a name that is neither a local, `args`, nor a facade |
| `nws_unknown_args_field` | `args.<field>` where the workflow does not declare that field |
| `nws_unknown_schema`, `nws_recursive_schema`, `nws_empty_schema`, `nws_duplicate_schema_field` | schema declaration problems |
| `nws_duplicate_declaration` | two `let` of one name in one block |
| `nws_not_callable` | calling something that is not a facade — NWS has no user-defined functions |
| `nws_facade_not_callable`, `nws_facade_member_not_called` | a facade container used as a value, or a member taken without calling it (no aliases, because each call carries a site id) |
| `nws_unknown_facade_member` | a member that container does not offer |
| `nws_arity_mismatch` | wrong argument count |
| `nws_await_required`, `nws_await_misplaced` | the await rules above |
| `nws_join_item_not_async`, `nws_join_needs_array_literal`, `nws_empty_join` | `Promise.all` shape |
| `nws_ask_receiver_not_an_actor` | `.ask` on a value the compiler cannot prove is an actor |
| `nws_literal_required`, `nws_artifact_id_invalid`, `nws_artifact_kind_mismatch`, `nws_artifact_redeclared`, `nws_undeclared_artifact_tag`, `nws_schema_argument_required` | values the compiler must see at compile time |
| `nws_duplicate_actor_name`, `nws_fanout_actor_name` | a literal actor name used twice, or a fixed name inside a per-element body — the run would fail with `DuplicateActorName` |
| `nws_phase_name_empty`, `nws_return_in_parallel`, `nws_statement_has_no_effect`, `nws_facade_returns_nothing` | statement-level mistakes |

## Worked examples

A linear run — plan, read, report, publish, return:

```
schema Plan {
  steps: string[]
  rationale: string
}

workflow "readme-plan" {
  args { repo: string }

  phase "plan";
  let planner = agent("planner", "You produce short, concrete plans.");
  let plan: Plan = await planner.ask("Draft a 3-step plan for " + args.repo);

  phase "execute";
  let readme = await files.read("README.md");
  report({ repo: args.repo, steps: plan.steps, readmeLength: readme.length });
  await artifact.markdown("summary", "## Plan\n\n" + plan.rationale);

  return { steps: plan.steps, rationale: plan.rationale };
}
```

Fan-out with a join, a loop and two strands:

```
schema Verdict {
  file: string
  findings: string[]
}

workflow "review" {
  args { files: string[] }

  artifact.table("progress", { columns: [{ field: "name", label: "File" }] });

  phase "review";
  let first = agent("first-reviewer");
  let second = agent("second-reviewer");
  let verdicts = await Promise.all([
    first.ask("Review the first file", Verdict),
    second.ask("Review the second file", Verdict)
  ]);
  report({ name: "reviewed", note: verdicts.length }, "progress");

  phase "collect";
  let worker = agent("worker");
  for (const name of args.files) {
    let note = await worker.ask("Summarize " + name);
    report({ name: name, note: note }, "progress");
  }

  phase "close";
  return { reviewed: verdicts.length, collected: args.files.length };
}
```

The `tests/fixtures/bench_linear.nws` and `bench_fanout.nws` files are runnable versions of these
two shapes and are what `specs/nc-code-workflow-benchmark.md` measures.
