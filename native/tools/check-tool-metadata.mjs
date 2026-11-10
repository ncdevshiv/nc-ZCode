#!/usr/bin/env node
// Tool **capability metadata** reconciliation: The fact that TS declares to the scheduler and permission gate, does the Rust native tool also declare the same.
//
// Why do this separately (and entry reconciliation are two halves of the same reason): `check-tool-schemas` is about "model capabilities"
//Which parameters are passed?", here is the "capability declared by the tool itself" - `readOnly/destructive/concurrentSafe/
// timeoutMs/maxOutputBytes/sideEffectScope`, plus **budget three-piece set** `maxModelBytes` (text into context
// The upper limit of TS is `min(inline, model)`)/`previewDirection`/`strategy`. They are not annotation-level decorations:
// - `readOnly` determines whether the plan mode will be released and whether headless will ask→Deny (TodoWrite of `ENTRY 137`
// It is declared as `read_only:false`, so the model in planning mode cannot even write its own task list);
// - `maxModelBytes` determines whether the model sees the full result or a truncated copy (10 MB allowed on the Bash line side of TS,
// There are only 30 000 B in the context - only `metadata.maxOutputBytes` will miss the truly effective one);
// - `timeoutMs` determines who is accidentally killed on the slow disk/large warehouse.
// This type of inconsistency is too large to read in the code (20 tools × 9 fields = 178 facts), so make it a recalculable floodgate.
//
// Get the **runtime truth** on both sides:
// - TS: `builtInTools`, `metadata` + `resultBudget` of `core/dist/tool/handlers/index.js`
// (Tools without resultBudget take effect according to TS's `DEFAULT_RESULT_BUDGET`);
// - Rust: `cargo run -p nc-code-host --bin tool_contracts` (produces registry declarations for each contract;
// Both synchronized tables and asynchronous tables must be dumped, otherwise Agent/AskUserQuestion/ExitPlanMode/SendMessage will be silently skipped).
// Differences should either be repaired in this round, or registered with reasons in `tool-metadata.baseline.json` (and pin the current values on both sides,
// If the value changes but the registration does not follow the change, it is considered a failure - anti-corrosion). TS Skip this item when a field is not declared (`undefined`).
//
// Usage: pnpm exec tsx native/tools/check-tool-metadata.mjs [--json] [--self-test]

import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const BASELINE = "native/tools/tool-metadata.baseline.json";
const MIN_REASON_CHARS = 24;
// The default budget of TS `result-serialization.ts:31`: Tools without resultBudget will take effect according to this.
const DEFAULT_RESULT_BUDGET = {
  maxInlineBytes: 100_000,
  maxModelBytes: 100_000,strategy: "truncate",
  preview: { direction: "head" },
};
const FIELDS = [
  "readOnly",
  "destructive",
  "concurrentSafe",
  "timeoutMs",
  "maxOutputBytes",
  "sideEffectScope",
  // The three-piece set of budget: **text** upper limit (`min(inline, model)`), whether to keep the head or tail when exceeding the budget, and strategy.
  // Only metadata.maxOutputBytes will miss the portion where TS actually takes effect - the line side of Bash is 10 MB,
  // Enter context is only 30 000 B; Write/Edit is 1 MB / 100 000 B.
  "maxModelBytes",
  "previewDirection",
  "strategy",
];

const format = (value) => (typeof value === "string" ? `"${value}"` : String(value));

async function tsMetadata() {
  const index = pathToFileURL(
    join(root, "apps/zcode-cli/packages/core/dist/tool/handlers/index.js"),
  ).href;
  const module = await import(index);
  const entries = module.builtInTools ?? module.default?.builtInTools;
  if (!Array.isArray(entries)) throw new Error("builtInTools is not an array (the exported shape has changed)");
  const out = new Map();
  const problems = [];
  for (const entry of entries) {
    const name = entry.metadata?.name ?? entry.name;
    if (!name) {
      problems.push("A certain builtInTools entry does not have metadata.name");
      continue;
    }
    const metadata = entry.metadata ?? {};
    const budget = { ...DEFAULT_RESULT_BUDGET, ...(entry.resultBudget ?? {}) };
    const row = {};
    for (const field of FIELDS) {
      if (budget[field] !== undefined) row[field] = budget[field];
    }
    // The remaining four fields are only **explicitly declared** by TS: default means that TS relies on `?.` to take the default semantics,
    // We are not qualified to compile a value for it as a fact.
    for (const field of [
      "readOnly",
      "destructive",
      "concurrentSafe",
      "timeoutMs",
      "maxOutputBytes",
      "sideEffectScope",
    ]) {
      if (metadata[field] !== undefined) row[field] = metadata[field];
    }
    // The text budget is the smaller of the two (TS `Math.min(maxModelBytes, maxInlineBytes)`);
    //The direction is preview.direction, which defaults to head.row.maxModelBytes = Math.min(budget.maxInlineBytes, budget.maxModelBytes);
    row.previewDirection = budget.preview?.direction ?? "head";
    out.set(name, row);
  }
  return { metadata: out, problems };
}

function rustMetadata() {
  const stdout = execFileSync(
    "cargo",
    ["run", "-q", "-p", "nc-code-host", "--bin", "tool_contracts"],
    { cwd: join(root, "native"), encoding: "utf8", maxBuffer: 32 * 1024 * 1024 },
  );
  const line = stdout
    .trim()
    .split("\n")
    .filter((row) => row.trim().startsWith("["))
    .pop();
  const rows = JSON.parse(line);
  const out = new Map();
  // The strategy on the Rust side is always truncate** (`tools::bound_model_result` is always marked with "truncate"):
  // The `artifact` strategy of TS is to externalize the complete output into a product file. This host does not have an object product externalization layer.
  // Written here instead of reading from the dump is to make the difference "TS=artifact and Rust=truncate"
  // Explicitly reconciled, rather than being silently skipped by a non-existent field.
  const RUST_STRATEGY = "truncate";
  for (const row of rows) {
    const missing = FIELDS.filter((field) => row[field] === undefined && field !== "strategy");
    if (missing. length > 0) {
      throw new Error(`Rust side ${row.name} is not declared ${missing.join("/")} (dump shape changed)`);
    }
    out.set(row.name, { ...row, strategy: RUST_STRATEGY });
  }
  return out;
}

/** Pure function: `--self-test` directly feeds fake data to prove that the check can really bite. */
function diffTool(name, ts, rust, baseline) {
  const problems = [];
  const notes = [];
  const allowed = baseline[name] ?? {};
  for (const field of FIELDS) {
    if (!(field in ts)) continue;
    if (ts[field] === rust[field]) {
      if (allowed[field]) {
        problems.push(
          `${name}.${field}: baseline entry is rotten (both sides are now ${format(ts[field])}, this registration should be deleted)`,
        );
      }
      continue;
    }
    const reason = allowed[field]?.reason;
    if (typeof reason !== "string" || reason.trim().length < MIN_REASON_CHARS) {
      problems.push(typeof reason === "string"
          ? `${name}.${field}: Registration reason is too short (at least ${MIN_REASON_CHARS} characters)`
          : `${name}.${field}: TS declaration ${format(ts[field])}, Rust declaration ${format(rust[field])}` +
              "(either align or register with a reason)",
      );
      continue;
    }
    const pinned = allowed[field];
    if (pinned.ts !== ts[field] || pinned.rust !== rust[field]) {
      problems.push(
        `${name}.${field}: baseline is pinned to ${format(pinned.ts)}/${format(pinned.rust)}, ` +
          `Now it is ${format(ts[field])}/${format(rust[field])} - the value changes, the registration must be changed accordingly`,
      );
    }
  }
  const extra = Object.keys(allowed).filter((field) => !(field in ts));
  for (const field of extra) {
    problems.push(`${name}.${field}: baseline entry is rotten (TS no longer declares this field)`);
  }
  if (!ts.destructive && rust.destructive) notes.push(`${name}: Only Rust side reports destructive`);
  return { problems, notes };
}

function selfTest() {
  const baseline = {
    Foo: {
      timeoutMs: {
        ts: 1000,
        rust: 500,
        reason: "The local implementation of this host does not require such a long waiting time, and the shortening is an intentional difference",
      },
    },
  };
  const cases = [
    [
      "Boolean inconsistency to be reported",
      diffTool("Foo", { readOnly: true }, { readOnly: false }, {}).problems.some((problem) =>
        problem.includes("readOnly"),
      ),
    ],
    [
      "Report if the values are inconsistent",
      diffTool("Foo", { maxOutputBytes: 8 }, { maxOutputBytes: 16 }, {}).problems.some((problem) =>
        problem.includes("maxOutputBytes"),
      ),
    ],
    [
      "Registered and still inconsistent, do not report",
      diffTool("Foo", { timeoutMs: 1000 }, { timeoutMs: 500 }, baseline).problems.length === 0,
    ],
    [
      "If the registered value does not match the reality, it will be reported as rotten",
      diffTool("Foo", { timeoutMs: 2000 }, { timeoutMs: 500 }, baseline).problems.some((problem) =>
        problem.includes("The value has changed"),
      ),
    ],
    [
      "The difference has disappeared and there are still registrations to report",diffTool("Foo", { timeoutMs: 500 }, { timeoutMs: 500 }, baseline).problems.some((problem) =>
        problem.includes("rotten"),
      ),
    ],
    [
      "The reason is too short to report",
      diffTool(
        "Foo",
        { timeoutMs: 1000 },
        { timeoutMs: 500 },
        { Foo: { timeoutMs: { ts: 1000, rust: 500, reason: "short" } } },
      ).problems.some((problem) => problem.includes("Too short")),
    ],
    [
      "TS undeclared fields skipped",
      diffTool("Foo", {}, { readOnly: false, destructive: false }, {}).problems.length === 0,
    ],
    [
      "Complete agreement will not be reported",
      diffTool("Foo", { destructive: true }, { destructive: true }, {}).problems.length === 0,
    ],
  ];
  const failures = cases.filter(([, ok]) => !ok).map(([label]) => label);
  console.log(
    failures.length === 0
      ? `[tool-metadata] self-test ok (${cases.length} positive and negative examples are as expected)`
      : `[tool-metadata] self-test FAILED: ${failures.join(" / ")}`,
  );
  process.exit(failures.length === 0 ? 0 : 1);
}

if (process.argv.includes("--self-test")) selfTest();

const baseline = JSON.parse(readFileSync(join(root, BASELINE), "utf8"));
const { metadata: ts, problems: shapeProblems } = await tsMetadata();
const rust = rustMetadata();
const problems = [...shapeProblems];
const notes = [];
let compared = 0;
let facts = 0;
for (const [name, metadata] of ts) {
  const mine = rust.get(name);
  if (!mine) continue; // TS-only tools are handled by the boundary table of tool-parity and are not repeatedly registered here.
  compared += 1;
  facts += FIELDS.filter((field) => field in metadata).length;
  const result = diffTool(name, metadata, mine, baseline);
  problems.push(...result.problems);
  notes.push(...result.notes);
}

if (process.argv.includes("--json")) {
  console.log(JSON.stringify({ compared, facts, problems, notes }, null, 2));
} else {
  console.log(`[tool-metadata] Reconciles ${compared} ${facts} capability facts for ${compared} tools with the same name on both sides (declared field by field for each tool)`,
  );
  for (const problem of problems) console.error(` ✗ ${problem}`);
  for (const note of notes) console.error(` · ${note}`);
  if (problems. length === 0)
    console.log("[tool-metadata] ok: The capability statement is consistent with TS field by field or there is a reason for registration");
}
process.exit(problems.length === 0 ? 0 : 1);