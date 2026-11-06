#!/usr/bin/env node
// Tool surface parity gate: Turn "TS exists, Rust exists" into a recalculable fact.
//
// Why it is needed: The README once said "TS CLI has about 114 tool handlers" - that is the handlers directory
// **The number of files**, not the number of tools (the actual built-in tools are 41, see below). This kind of number will rot once it is written into the document.
// And it would make people mistakenly think that there are more than a hundred tools waiting to be ported. This script only reads the source code, not the documentation:
// - TS side: `builtInTools` array of `handlers/index.ts` (the one actually registered at runtime),
// Each entry constant is resolved to the `metadata.name` of the file in which it is located;
// - Rust side: `with_defaults` (synchronization tools) of `tools/mod.rs` + `lib.rs`
// `with_async_tool` (Async tool: Task/AskUserQuestion/ExitPlanMode).
// Compare with `tool-parity.table.json`: a name that is not in the table appears on any side, or the table is marked ported
// But the Rust side cannot find it, or the boundary is marked but there is no spec source, so it exits with non-0.
//
// Usage: node native/tools/check-tool-parity.mjs [--json]

import { existsSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const TS_HANDLERS = "apps/zcode-cli/packages/core/src/tool/handlers";
const TS_INDEX = `${TS_HANDLERS}/index.ts`;
const CONTRACTS_DIR = "apps/zcode-cli/packages/contracts/src";
const RUST_REGISTRY = "native/crates/nc-code-host/src/tools/mod.rs";
const RUST_LIB = "native/crates/nc-code-host/src/lib.rs";
const TABLE = "native/tools/tool-parity.table.json";

const read = (relative) => readFileSync(join(root, relative), "utf8");

/** `export const X_TOOL_NAME = "..."` (contracts and core local are both counted). */
function toolNameConstants() {
  const constants = new Map();
  const files = [...walk(`${CONTRACTS_DIR}`), ...walk("apps/zcode-cli/packages/core/src")];
  for (const file of files) {
    const text = read(file);
    for (const match of text.matchAll(
      /(?:export\s+)?const\s+([A-Z0-9_]*TOOL_NAME)\s*=\s*"([^"]+)"/g,
    )) {
      constants.set(match[1], match[2]);
    }
  }
  // Constants on the Rust side (such as `pub const SUBAGENT_TOOL_NAME: &str = "Task"`) must also be parsable,// Otherwise the name of the asynchronous tool can only be guessed.
  for (const file of walk("native/crates/nc-code-host/src", ".rs")) {
    const text = read(file);
    for (const match of text.matchAll(
      /pub const ([A-Z0-9_]*(?:TOOL_NAME|TOOL_ALIAS)): &str = "([^"]+)"/g,
    )) {
      if (!constants.has(match[1])) constants.set(match[1], match[2]);
    }
  }
  return constants;
}

function walk(relativeDir, extension = ".ts") {
  const { readdirSync, statSync } = fsModule();
  const out = [];
  const visit = (relative) => {
    for (const entry of readdirSync(join(root, relative))) {
      const child = `${relative}/${entry}`;
      const stat = statSync(join(root, child));
      if (stat.isDirectory()) {
        if (entry === "node_modules" || entry === "dist" || entry === "out") continue;
        visit(child);
      } else if (entry.endsWith(extension)) {
        out.push(child);
      }
    }
  };
  visit(relativeDir);
  return out;
}

function fsModule() {
  // Delay fs fetching: The import at the top of the script only retains readFileSync, and directory traversal is introduced separately.
  return process.getBuiltinModule("node:fs");
}

/** TS built-in tool name: each entry constant in the builtInTools array. */
function tsToolNames(constants) {
  const index = read(TS_INDEX);
  const block = index.split("export const builtInTools: ToolEntry[] = [")[1]?.split("];")[0];
  if (!block) throw new Error(`cannot find builtInTools in ${TS_INDEX}`);
  const identifiers = [...block.matchAll(/^\s*([A-Za-z0-9_]+ToolEntry),/gm)].map((m) => m[1]);

  const imports = new Map();
  for (const match of index.matchAll(/import\s*\{([^}]+)\}\s*from\s*"\.\/([A-Za-z0-9_-]+)\.js"/g)) {
    for (const raw of match[1].split(",")) {
      const ident = raw.trim();
      if (ident) imports.set(ident, match[2]);
    }
  }

  const declaredIn = new Map();
  for (const relative of walk(TS_HANDLERS)) {
    for (const match of read(relative).matchAll(
      /export const ([A-Za-z0-9_]+ToolEntry)\s*(?::\s*ToolEntry\s*)?=/g,
    )) {
      declaredIn.set(match[1], relative);
    }
  }

  const names = [];
  for (const identifier of identifiers) {
    const file = imports.get(identifier)
      ? `${TS_HANDLERS}/${imports.get(identifier)}.ts`
      : declaredIn.get(identifier);
    if (!file) throw new Error(`cannot locate ${identifier}`);
    const text = read(file);
    const start = text.indexOf(`export const ${identifier}`);
    if (start < 0) throw new Error(`cannot locate body of ${identifier} in ${file}`);
    const body = text.slice(start);
    let name = resolveMetadataName(body, constants);
    if (!name) {
      // Factory form (`export const xToolEntry = createXToolEntry(...)`) or constant alias:
      // 1) Fall back to metadata.name in the entire file; 2) Fall back to camelCase → CONSTANT mapping.
      name = resolveMetadataName(text, constants) ?? constantFor(ident, constants);
    }
    if (!name) throw new Error(`no metadata.name for ${ident} (${file})`);
    names.push(name);
  }
  return names.sort();
}

function resolveMetadataName(text, constants) {
  const match = text.match(/metadata:\s*\{[\s\S]*?name:\s*([A-Za-z0-9_]+|"[^"]+")/);
  if (!match) return undefined;
  const raw = match[1];
  return raw.startsWith('"') ? raw.slice(1, -1) : constants.get(raw);
}

/** `submitResultToolEntry` → `SUBMIT_RESULT_TOOL_NAME`. */
function constantFor(ident, constants) {
  const base = ident.replace(/ToolEntry$/, "");
  const constant = `${base.replace(/([A-Z])/g, "_$1").toUpperCase()}_TOOL_NAME`;
  return constants.get(constant);
}

/** Rust registry: synchronous tools with_defaults + asynchronous tools in lib.rs. */
function rustToolNames(constants) {
  const syncBlock = read(RUST_REGISTRY)
    .split("pub fn with_defaults() -> Self {")[1]
    ?.split("\n }")[0];
  if (!syncBlock) throw new Error(`cannot find with_defaults in ${RUST_REGISTRY}`);
  const sync = new Set();
  for (const match of syncBlock.matchAll(
    /"([A-Za-z0-9_.]+)"\.to_string\(\),\s*\n?\s*Arc::new\(/g,
  )) {
    sync.add(match[1]);
  }
  //Multi-line writing method: tools.insert("X".to_string(), Arc::new(...)) The same regular rule has been covered;
  // Constant name writing (such as plan::ENTER_PLAN_MODE_TOOL_NAME.to_string()) is parsed separately.
  for (const match of syncBlock.matchAll(
    /([A-Z_]+TOOL_NAME)\.to_string\(\),\s*\n?\s*Arc::new\(/g,
  )) {
    const resolved = constants.get(match[1]);
    if (!resolved) throw new Error(`unresolved rust tool-name constant ${match[1]}`);
    sync.add(resolved);
  }

  const asyncNames = new Set();
  // The async tools and compatibility aliases live in `tools/mod.rs`'s `with_defaults`
  // builder in the native port (they used to be assembled in `lib.rs`). Scan both
  // registration sites so moving the builder does not make the gate blind.
  const lib = `${read(RUST_REGISTRY)}\n${read(RUST_LIB)}`;
  // Tools built by the host and registered outside with_defaults (`.with_tool(Arc::new(X::new(..)))`):
  // The name is read from its `impl Tool for X`, the same rules as asynchronous tools.
  for (const match of lib.matchAll(
    /with_tool\(Arc::new\(([A-Za-z0-9_:]+?)(?:::new\([^)]*\))?\)\)/g,
  )) {
    const typeName = match[1].split("::").pop();
    const typeFile = findRustImpl(typeName, "Tool");
    if (!typeFile) throw new Error(`cannot locate Tool impl for ${typeName}`);
    const text = read(typeFile);
    const start = text.indexOf(`impl Tool for ${typeName}`);
    if (start < 0) throw new Error(`no Tool impl for ${typeName}`);
    const body = text.slice(start);
    const nameMatch = body.match(/name:\s*([A-Z_]+TOOL_NAME)\.to_string\(\)|name:\s*"([^"]+)"/);
    if (!nameMatch) throw new Error(`no contract name for ${typeName}`);
    const resolved = nameMatch[1] ? constants.get(nameMatch[1]) : nameMatch[2];
    if (!resolved) throw new Error(`unresolved tool name for ${typeName}`);
    asyncNames.add(resolved);
  }

  // Compatible aliases (with_alias("Task", "Agent")): Even if it can be parsed, it does not count as advertising.
  // Accept quoted literals as well as `CONST` / `path::CONST` arguments.
  for (const match of lib.matchAll(
    /with_alias\(\s*([A-Za-z0-9_:"']+)\s*,\s*([A-Za-z0-9_:"']+)\s*,?\s*\)/g,
  )) {
    for (const raw of [match[1], match[2]]) {
      const resolved = constants.get(raw.split("::").pop()) ?? raw.replace(/"/g, "");
      if (resolved) asyncNames.add(resolved);
    }
  }
  for (const match of lib.matchAll(
    /with_async_tool\(Arc::new\(([A-Za-z0-9_:]+?)(?:::(?:new|default)\([^)]*\))?\)\)/g,
  )) {
    const path = match[1].trim();
    const typeName = path.split("::").pop();
    const typeFile = findRustType(typeName);
    if (!typeFile) throw new Error(`cannot locate async tool type ${typeName}`);
    const text = read(typeFile);
    const start = text.indexOf(`impl AsyncTool for ${typeName}`);
    if (start < 0) throw new Error(`no AsyncTool impl for ${typeName}`);
    const body = text.slice(start);
    const nameMatch = body.match(/name:\s*([A-Z_]+TOOL_NAME)\.to_string\(\)|name:\s*"([^"]+)"/);
    if (!nameMatch) throw new Error(`no contract name for ${typeName}`);
    const resolved = nameMatch[1] ? constants.get(nameMatch[1]) : nameMatch[2];
    if (!resolved) throw new Error(`unresolved async tool name for ${typeName}`);
    asyncNames.add(resolved);
  }
  // workflow actor session tool (submit_result/escalate): the registration point is **driver injection by session**
  // (`with_async_tool(Arc::new(SubmitResultTool::new(..)))` of `driver.rs`), not in the production registry
  // Nor should they be called - the parent session's model must never call them. They are indeed model surface tools (visible to the workflow subagent),
  // So the gate recognizes this second registration point here; it requires impl and the construction point to exist at the same time to avoid counting dead code as a tool.
  const ACTOR_TOOLS = "native/crates/nc-code-host/src/workflow/actor_tools.rs";
  const ACTOR_DRIVER = "native/crates/nc-code-host/src/workflow/driver.rs";
  if (existsSync(join(root, ACTOR_TOOLS)) && existsSync(join(root, ACTOR_DRIVER))) {
    const actorText = read(ACTOR_TOOLS);
    const driverText = read(ACTOR_DRIVER);
    for (const match of actorText.matchAll(/impl AsyncTool for ([A-Za-z0-9_]+)/g)) {
      const typeName = match[1];
      if (!driverText.includes(`${typeName}::new(`)) continue; // Unassembled impl does not count as tool surface
      const body = actorText.slice(match.index);
      const nameMatch = body.match(/name:\s*([A-Z_]+TOOL_NAME)\.to_string\(\)|name:\s*"([^"]+)"/);
      if (!nameMatch) throw new Error(`no contract name for actor tool ${typeName}`);
      const resolved = nameMatch[1] ? constants.get(nameMatch[1]) : nameMatch[2];
      if (!resolved) throw new Error(`unresolved actor tool name for ${typeName}`);
      asyncNames.add(resolved);
    }
  }

  return { sync: [...sync].sort(), async: [...asyncNames].sort() };
}

function findRustImpl(typeName, traitName) {
  const { readdirSync } = fsModule();
  const candidates = [];
  const visit = (relative) => {
    for (const entry of readdirSync(join(root, relative))) {
      const child = `${relative}/${entry}`;
      if (entry === "target") continue;
      const isDir = fsModule().statSync(join(root, child)).isDirectory();
      if (isDir) visit(child);
      else if (entry.endsWith(".rs")) {
        if (read(child).includes(`impl ${traitName} for ${typeName}`)) candidates.push(child);
      }
    }
  };
  visit("native/crates/nc-code-host/src");
  return candidates[0];
}

function findRustType(typeName) {
  const { readdirSync } = fsModule();
  const candidates = [];
  const visit = (relative) => {
    for (const entry of readdirSync(join(root, relative))) {
      const child = `${relative}/${entry}`;
      if (entry === "target") continue;
      const isDir = fsModule().statSync(join(root, child)).isDirectory();
      if (isDir) visit(child);
      else if (entry.endsWith(".rs")) {
        if (read(child).includes(`impl AsyncTool for ${typeName}`)) candidates.push(child);
      }
    }
  };
  visit("native/crates/nc-code-host/src");
  return candidates[0];
}

/**
 * The ported line must point to a **actual** use case (`<file>::<fn>`), otherwise the exemption reason must be written.
 * "The tool has been ported" only means that it is in the registry; the target requires that each ported tool has e2e evidence, so this
 * You cannot rely on the registration form for self-certification - the file does not exist, the function name is written incorrectly (ghost reference), and there is no reason for exemption, it will all be considered a failure.
 * Pure function + injected exists/text, used by `--self-test` to prove that these branches really bite.
 */
function testCitationProblem(row, exists, text) {
  if (row.status !== "ported") return null;
  const exemption = typeof row.testExempt === "string" ? row.testExempt.trim() : "";
  if (exemption) {
    return exemption.length >= 40
      ? null
      : `${row.name}: testExempt must explain why e2e is not feasible (at least 40 words)`;
  }
  if (!row.test) return `${row.name}: ported must be given test(<file>::<fn>) or testExempt`;
  const match = /^(.+\.rs)::([A-Za-z0-9_]+)$/.exec(row.test);
  if (!match) return `${row.name}: test shape must be <file.rs>::<fn>, received ${row.test}`;
  const [, file, fn] = match;
  if (!exists(file)) return `${row.name}: The registered test file does not exist: ${file}`;
  if (!new RegExp(`fn\\s+${fn}\\s*[<(]`).test(text(file))) {
    return `${row.name}: There is no fn ${fn} in ${file} (the test was renamed or deleted, the table lags behind the code)`;
  }
  return null;
}

/** Counterexample checking for the gate itself: no failing gate is just decoration. */
function selfTest() {
  const exists = (file) => file === "real.rs";
  const text = () => "async fn the_real_test() {\n}\n";
  const cases = [
    [
      "Good references are not reported",
      { name: "X", status: "ported", test: "real.rs::the_real_test" },
      null,
    ],
    ["Missing test must be reported", { name: "X", status: "ported" }, /test must be given/],
    [
      "Ghost file to be reported",
      { name: "X", status: "ported", test: "gone.rs::the_real_test" },
      /does not exist/,
    ],
    [
      "Ghost function to be reported",
      { name: "X", status: "ported", test: "real.rs::renamed_away" },
      /there is no fn/,
    ],
    [
      "Report for incorrect shape",
      { name: "X", status: "ported", test: "real.rs.the_real_test" },
      /shape/,
    ],
    [
      "Empty exemption to report",
      { name: "X", status: "ported", testExempt: "too short" },
      /at least 40 words/,
    ],
    ["Long exemption release", { name: "X", status: "ported", testExempt: "x".repeat(50) }, null],
    ["boundary regardless of test", { name: "X", status: "boundary" }, null],
  ];
  const failures = [];
  for (const [label, row, expect] of cases) {
    const got = testCitationProblem(row, exists, text);
    const ok = expect === null ? got === null : got !== null && expect.test(got);
    if (!ok) failures.push(`${label}: Expect ${expect ?? "null"}, get ${got}`);
  }
  console.log(
    failures.length === 0
      ? `[tool-parity] self-test ok (${cases.length} positive and negative examples are as expected)`
      : `[tool-parity] self-test FAILED`,
  );
  for (const failure of failures) console.error(` ✗ ${failure}`);
  process.exit(failures.length === 0 ? 0 : 1);
}

if (process.argv.includes("--self-test")) selfTest();

/** Each row in the table: name + status + basis. See below for status closed sets. */
const ALLOWED_STATUS = new Set(["ported", "boundary", "gap"]);

const constants = toolNameConstants();
const ts = tsToolNames(constants);
const rust = rustToolNames(constants);
const rustAll = new Set([...rust.sync, ...rust.async]);
const table = JSON.parse(read(TABLE));
const rows = new Map(table.tools.map((row) => [row.name, row]));

const problems = [];
for (const name of ts) {
  const row = rows.get(name);
  if (!row) {
    problems.push(`TS tool ${name} is not in ${TABLE} (new tools must register status and basis)`);
    continue;
  }
  if (!ALLOWED_STATUS.has(row.status)) problems.push(`${name}: unknown status ${row.status}`);
  if (row.status === "ported" && !rustAll.has(name)) {
    problems.push(`${name}: marked ported but not in the Rust registry`);
  }
  if (row.status !== "ported" && row.status !== "gap" && !row.evidence) {
    problems.push(`${name}: ${row.status} must give evidence (spec path)`);
  }
  const citation = testCitationProblem(
    row,
    (file) => existsSync(join(root, file)),
    (file) => read(file),
  );
  if (citation) problems.push(citation);
}
for (const name of rustAll) {
  if (!rows.has(name))
    problems.push(`Rust tool ${name} is not in ${TABLE} (table lags behind code)`);
}
// Rust-specific ported tools (such as ReadImage) are not in the TS list, and the test source must also be checked.
for (const row of table.tools) {
  if (ts.includes(row.name)) continue;
  const citation = testCitationProblem(
    row,
    (file) => existsSync(join(root, file)),
    (file) => read(file),
  );
  if (citation) problems.push(citation);
}
// The inside and outside of the table are marked ported, but not on the TS side → Either it is unique to Rust (such as ReadImage/nc-tools), or the table is written incorrectly.
for (const row of table.tools) {
  if (row.ts === false) continue;
  if (!ts.includes(row.name))
    problems.push(`${row.name}: The table says that TS exists, but there is no builtInTools`);
}

const summary = {
  ts: ts.length,
  rustSync: rust.sync.length,
  rustAsync: rust.async.length,
  table: table.tools.length,
  problems,
};
if (process.argv.includes("--json")) {
  console.log(JSON.stringify({ ...summary, tsNames: ts, rust: rust }, null, 2));
} else {
  console.log(
    `[tool-parity] TS built-in tool ${summary.ts}; Rust synchronization ${summary.rustSync} + asynchronous ${summary.rustAsync}; table ${summary.table} row`,
  );
  for (const problem of problems) console.error(` ✗ ${problem}`);
  if (problems.length === 0)
    console.log(
      "[tool-parity] ok: The tool surfaces on both sides are consistent with the registration table",
    );
}
process.exit(problems.length === 0 ? 0 : 1);
