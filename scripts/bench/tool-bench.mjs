// TS side tool micro-benchmark: directly call the built-in tool handler in **production build** (packages/*/dist),
// Stuffed in the context is the production Node file system adapter (createNodeFileSystemAdapter),
// Not a self-written simplified stand-in. Output JSON (median time/mean of each operation) for comparison script to merge.
//
// Usage: pnpm exec tsx scripts/bench/tool-bench.mjs
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { performance } from "node:perf_hooks";

const root = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const core = (p) => pathToFileURL(join(root, "apps/zcode-cli/packages/core/dist", p)).href;
const adapters = (p) => pathToFileURL(join(root, "apps/zcode-cli/packages/adapters/dist", p)).href;

const { readToolEntry } = await import(core("tool/handlers/read.js"));
const { writeToolEntry } = await import(core("tool/handlers/write.js"));
const { editToolEntry } = await import(core("tool/handlers/edit.js"));
const { globToolEntry } = await import(core("tool/handlers/glob.js"));
const { grepToolEntry } = await import(core("tool/handlers/grep.js"));
const { cronCreateToolEntry, cronListToolEntry, cronUpdateToolEntry, cronDeleteToolEntry } =
  await import(core("tool/handlers/cron.js"));
const { createNodeFileSystemAdapter } = await import(adapters("fs/index.js"));
const { bashToolEntry } = await import(core("tool/handlers/bash.js"));
// Bash runs **production** execution port and shell parsing (parsed to git-bash under win32, the same shell as the Rust side):
// Do not replace the fake runner in the benchmark, otherwise the same code path will not be measured.
const { createNodeExecutionAdapter, resolveEffectiveBashShellSelection } = await import(
  adapters("exec/index.js")
);
const { skillToolEntry } = await import(core("tool/handlers/skill.js"));
const { createNodeSkillAdapter } = await import(adapters("skills/index.js"));// Todo goes to **production storage**: `todoWriteToolEntry.handler` requires `context.sessionStore`,
// What is given here is the real SqliteSessionStore (`todo` table has `references session(id)` and
// `pragma foreign_keys = on`, so we need to actually create a session line). Without changing the memory substitute,
// Otherwise, the measurement is not the durability cost of producing each TodoWrite.
const { todoWriteToolEntry, todoReadToolEntry } = await import(core("tool/handlers/todo.js"));
const { createSqliteSessionStore } = await import(adapters("storage/session-store.js"));
const services = (p) => pathToFileURL(join(root, "packages/services/dist", p)).href;
const { createInProcessNodeReplExecutor } = await import(
  pathToFileURL(join(root, "apps/zcode-cli/packages/node-repl-host/src/server.ts")).href
);
const { AutomationService } = await import(services("session/automationService.js"));
const { AutomationRepo } = await import(services("session/automationRepo.js"));

const WORKSPACE = join(tmpdir(), `zcode-bench-ts-${process.pid}`);
let BENCH_AUTOMATION;
const FILES = 400;
const AUTOMATION_DB = join(WORKSPACE, "..", `zcode-bench-automation-${process.pid}.sqlite`);
const SESSION_DB = join(WORKSPACE, "..", `zcode-bench-session-${process.pid}.sqlite`);
const BENCH_SESSION_ID = "bench-session";
let BENCH_SESSION_STORE;
const EDIT_LINES = Array.from({ length: 64 }, (_, index) => `value-v1-${index}`).join("\n") + "\n";

function buildFixture() {
  rmSync(WORKSPACE, { recursive: true, force: true });
  mkdirSync(join(WORKSPACE, "src"), { recursive: true });
  for (let index = 0; index < FILES; index += 1) {
    const pad = `${index}`.padStart(4, "0");
    const body = Array.from({ length: 20 }, (_, line) => `export const v${pad}_${line} = ${line};`);
    if (index % 10 === 0) body.push("// grep-needle");
    writeFileSync(join(WORKSPACE, "src", `mod-${pad}.ts`), body.join("\n") + "\n");
  }writeFileSync(join(WORKSPACE, "target.txt"), "line v1\n".repeat(2048));
  // Skill's fixture: the same SKILL.md as the Rust side (same frontmatter and body).
  mkdirSync(join(WORKSPACE, ".agents", "skills", "bench-skill"), { recursive: true });
  writeFileSync(
    join(WORKSPACE, ".agents", "skills", "bench-skill", "SKILL.md"),
    "---\nname: bench-skill\ndescription: benchmark skill fixture\n---\n" +
      "Benchmark skill body.\n".repeat(64),
  );
  writeFileSync(join(WORKSPACE, "edit.txt"), EDIT_LINES);
}

// **Real** automation port: directly connected to the production `AutomationService` + sqlite warehouse (with the Rust side
// `AutomationToolPort` symmetric), without any fake ports.
function realAutomationPort(dbPath, workspace) {
  const service = new AutomationService(new AutomationRepo(dbPath));
  const scope = { workspacePath: workspace, workspaceIdentity: workspace };
  const port = {
    async create(input, options) {
      const model = options?.model?.split("/");
      return await service.create({
        title: input.title,
        cronExpr: input.cron ?? "",
        relativeDelayMinutes: input.delayMinutes,
        prompt: input.prompt,
        recurring: input.delayMinutes === undefined ? (input.recurring ?? true) : false,
        maxRuns: input.delayMinutes === undefined ? input.maxRuns : undefined,
        intervalUnit: input.intervalUnit,
        interval: input.interval,
        workspacePath: workspace,
        workspaceIdentity: workspace,
        ...(model && model.length === 2
          ? { modelSelection: { providerId: model[0], modelId: model[1] } }
          : {}),
      });
    },
    async list() {
      return await service.list(scope);
    },
    async update(input) {
      const { id, cron, ...rest } = input;
      return await service.update(
        ID,
        { ...rest, ...(cron !== undefined ? { cronExpr: cron } : {}) },scope,
      );
    },
    async delete(input) {
      return await service.delete(input.id, scope);
    },
  };
  return { port, deleteById: (id) => service.delete(id, scope) };
}

// **Real** session store: production `SqliteSessionStore` (run full migration, `pragma foreign_keys = on`).
// `todo.session_id` declares `references session(id)`, so create a real session line first; this step
// Except for timing, it is the same premise as "the session directory already exists" on the Rust side.
async function realSessionStore(dbPath, workspace) {
  const store = createSqliteSessionStore({ dbPath });
  await store.createSession({
    id: BENCH_SESSION_ID,
    projectID: "bench-project",
    slug: "bench",
    directory: workspace,
    title: "bench",
    version: "bench",
  });
  return store;
}

// The port and shell are only parsed once: In production, there is also one execution port for each session, and counting the structure into the sample will cause distortion.
const BENCH_EXECUTION_PORT = createNodeExecutionAdapter();
const BENCH_SKILL_PORT = createNodeSkillAdapter();
const BASH_SHELL_SELECTION = resolveEffectiveBashShellSelection({
  env: process.env,
  platform: process.platform,
}).selection;

function context() {
  return {
    workspaceRoot: WORKSPACE,
    workingDirectory: WORKSPACE,
    abortSignal: new AbortController().signal,
    toolCallId: "bench-call",
    traceId: "bench-trace",
    sessionId: BENCH_SESSION_ID,
    turnId: "bench-turn",
    runtimeScope: "main",
    fileSystemPort: createNodeFileSystemAdapter(),
    executionPort: BENCH_EXECUTION_PORT,
    bashShellSelection: BASH_SHELL_SELECTION,
    skillPort: BENCH_SKILL_PORT,
    automationPort: BENCH_AUTOMATION.port,
    sessionStore: BENCH_SESSION_STORE,
    model: { providerId: "anthropic", modelId: "test-model" },
    // Each operation is independent: the read-before-edit state of Read is not reused across samples.
    readFileState: new Map(),
  };
}

/** Count N times; return ns/op statistics. `prepare` is run before each sample (untimed). */
async function measure(label, iterations, run, options = {}) {  const { prepare, verify, cleanup } = options;
  for (let index = 0; index < 3; index += 1) {
    if (prepare) await prepare();
    await run(context());
    if (cleanup) await cleanup();
  }
  const samples = [];
  let result;
  for (let index = 0; index < iterations; index += 1) {
    if (prepare) await prepare();
    const ctx = context();
    const started = performance.now();
    result = await run(ctx);
    samples.push((performance.now() - started) * 1e6);
    if (cleanup) await cleanup();
  }
  samples.sort((a, b) => a - b);
  const mean = samples.reduce((sum, value) => sum + value, 0) / samples.length;
  return {
    tool: label,
    impl: "ts",
    iterations,
    meanNs: Math.round(mean),
    medianNs: Math.round(samples[Math.floor(samples.length / 2)]),
    p95Ns: Math.round(samples[Math.floor(samples.length * 0.95)]),
    check: verify ? verify(result) : "",
  };
}

const countAll = (text, needle) => text.split(needle).length - 1;

buildFixture();
BENCH_AUTOMATION = realAutomationPort(AUTOMATION_DB, WORKSPACE);
BENCH_SESSION_STORE = await realSessionStore(SESSION_DB, WORKSPACE);
const content = Array.from({ length: 2048 }, (_, line) => `row ${line}`).join("\n") + "\n";
const rows = [];

rows.push(
  await measure(
    "read",
    200,
    (ctx) => readToolEntry.handler({ file_path: join(WORKSPACE, "target.txt") }, ctx),
    { verify: (r) => `content:${Boolean(r && r.content && r.content.length > 0)}` },
  ),
);

rows.push(
  await measure(
    "write",
    100,
    (ctx) => writeToolEntry.handler({ file_path: join(WORKSPACE, "w.txt"), content }, ctx),
    { prepare: () => rmSync(join(WORKSPACE, "w.txt"), { force: true }) },
  ),
);

rows.push(
  await measure("glob", 40, (ctx) => globToolEntry.handler({ pattern: "**/*.ts" }, ctx), {
verify: (r) => String(countAll(JSON.stringify(r ?? ""), "mod-")),
  }),
);

rows.push(
  await measure(
    "grep",
    20,
    (ctx) => grepToolEntry.handler({ pattern: "grep-needle", output_mode: "content" }, ctx),
    { verify: (r) => String(countAll(JSON.stringify(r ?? ""), "grep-needle")) },
  ),
);

// Edit timing unit = read (create read-before-edit state) + edit; prepare to reset the file.
// TodoWrite requires SessionStorePort (provided by the session library in production), which is not an IO hotspot and is not tested in this benchmark.
rows.push(
  await measure(
    "edit(read+edit)",
    100,
    async (ctx) => {
      await readToolEntry.handler({ file_path: join(WORKSPACE, "edit.txt") }, ctx);
      return editToolEntry.handler(
        {
          file_path: join(WORKSPACE, "edit.txt"),
          old_string: "value-v1-32",
          new_string: "value-v2-32",
        },
        ctx,
      );
    },
    {
      prepare: () => writeFileSync(join(WORKSPACE, "edit.txt"), EDIT_LINES),
      verify: () => "ok",
    },
  ),
);

// Bash: Really spawn a shell child process. The two commands deliberately separate "process built-in vs. start a new process",
// Because the spawn cost dominates the absolute value; the difference reflects the cost of the tool layer (parsing/output orchestration/state side effects).
rows.push(
  await measure(
    "bash(echo)",
    25,
    (ctx) => bashToolEntry.handler({ command: "echo bench-hello" }, ctx),
    {
      verify: (r) => String(countAll(JSON.stringify(r ?? ""), "bench-hello")),
    },
  ),
);
rows.push(
  await measure("bash(node -e)", 10, (ctx) =>
    bashToolEntry.handler({ command: 'node -e "console.log(1+1)"' }, ctx),
  ),
);

// Skill: Both sides read the same real SKILL.md (the TS side also runs the production root analysis + frontmatter verification +
//Plugin manifest search, that is the production path, not cut).
rows.push(
  await measure("skill", 100, (ctx) => skillToolEntry.handler({ skill: "bench-skill" }, ctx), {
    verify: (r) => String(countAll(JSON.stringify(r ?? ""), "bench-skill")),
  }),
);

// Todo: TS side is producing `SqliteSessionStore` (`begin immediate` → delete → 4 insert →// touchSession → commit), the Rust side is `todos.json` (temp+rename) in the session directory. For both sides
// **The same 4-item list**, the same durable backend that "writes every time"; `verify` incidentally changes the output key name of TS
// Type out (oldTodos/summary/todos) to check the return shape of Rust side completion.
const TODO_LIST = [
  { content: "draft the plan", status: "completed", priority: "high" },
  { content: "implement the host", status: "in_progress", priority: "high" },
  { content: "wire the read model", status: "pending", priority: "medium" },
  { content: "benchmark every tool", status: "pending", priority: "low" },
];
rows.push(
  await measure("todo.write", 100, (ctx) => todoWriteToolEntry.handler({ todos: TODO_LIST }, ctx), {
    verify: (r) =>
      Object.keys(r ?? {})
        .sort()
        .join(","),
  }),
);
rows.push(
  await measure("todo.read", 200, (ctx) => todoReadToolEntry.handler({}, ctx), {
    verify: (r) => `todos:${r?.todos?.length ?? -1}`,
  }),
);

// Scheduled task: **real service** (AutomationService + sqlite). The service has a limit of "up to 20 items":
// Preset 19 items for list/update; create deletes the newly created item outside the timer in each round; delete before the timer in each round
//Add one. Clean up the service (not counting the tool layer time), and use the same set of orchestration as the Rust side.
const cronContext = context();
const createCron = async (index) =>
  JSON.stringify(
    await cronCreateToolEntry.handler(
      { title: `job ${index}`, prompt: "check", cron: "0 * * * *" },
      cronContext,
    ),
  );
for (let index = 0; index < 19; index += 1) await createCron(index);
const listedCron = await cronListToolEntry.handler({}, cronContext);
const firstCronId = listedCron.automations[0].automationId;

let createdForCleanup;
rows.push(
  await measure(
    "cron.create",
    60,
    async () => {
      const output = await createCron(1000);
      createdForCleanup = JSON.parse(output).automation.automationId;
      return output;
    },
    {
      verify: () => "",
      cleanup: () => BENCH_AUTOMATION.deleteById(createdForCleanup),
    },
  ),);
rows.push(
  await measure("cron.list", 200, async () =>
    JSON.stringify(await cronListToolEntry.handler({}, cronContext)),
  ),
);
rows.push(
  await measure("cron.update", 200, async () =>
    JSON.stringify(
      await cronUpdateToolEntry.handler({ id: firstCronId, title: "renamed" }, cronContext),
    ),
  ),
);
let pendingDeleteId;
rows.push(
  await measure(
    "cron.delete",
    100,
    async () =>
      JSON.stringify(await cronDeleteToolEntry.handler({ id: pendingDeleteId }, cronContext)),
    {
      prepare: async () => {
        const output = await createCron(2000);
        pendingDeleteId = JSON.parse(output).automation.automationId;
      },
    },
  ),
);

// js: TS's in-process executor **creates a new session each time** (same semantics as Rust's `Fresh` mode).
// Rust's persistent warmup mode has no equivalent here and is reported separately.
const executeJs = createInProcessNodeReplExecutor();
{
  const samples = [];
  let last;
  for (let index = 0; index < 15; index += 1) {
    const started = performance.now();
    last = await executeJs({ code: "1+1", requestMeta: {} });
    samples.push((performance.now() - started) * 1e6);
  }
  samples.sort((a, b) => a - b);
  const median = samples[Math.floor(samples.length / 2)];
  rows.push({
    tool: "js.cold",
    impl: "ts",
    iterations: samples.length,
    meanNs: Math.round(samples.reduce((sum, value) => sum + value, 0) / samples.length),
    medianNs: Math.round(median),
    p95Ns: Math.round(samples[Math.floor(samples.length * 0.95)]),
    check: String(last?.result ?? ""),
  });
}

rmSync(WORKSPACE, { recursive: true, force: true });
try {
  // The sqlite handle may still be open (released when the process exits): cleanup is best-effort, and failure does not affect the numbers.
  BENCH_SESSION_STORE.close();
  rmSync(AUTOMATION_DB, { force: true });
  rmSync(SESSION_DB, { force: true });
} catch {
  // ignore
}console.log(JSON.stringify({ impl: "ts", rows }, null, 2));
