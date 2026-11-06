// Generate the shared workflow benchmark fixtures: compiles each author script ONCE through
// the real TypeScript front-end (built dist) and writes {lowered, siteIds, askSpecs,
// scriptHash} to tests/fixtures/<name>.json. Both runtimes (TS bench + Rust bench) execute
// the SAME lowered bytes, so run-time comparisons measure the runtime, not the front-end.
//
// Usage: node scripts/bench/gen-workflow-fixtures.mjs
// Requires: apps/zcode-cli/packages/dynamic-workflow/dist built (pnpm --filter @zcode/dynamic-workflow build)

import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

const root = resolve(import.meta.dirname, "..", "..");
const dwUrl = pathToFileURL(
  join(root, "apps/zcode-cli/packages/dynamic-workflow/dist/index.js"),
).href;
const {
  lowerWorkflowScript,
  synthesizeWorkflowSchemas,
  createWorkflowProgram,
  collectSites,
  buildAskSpecs,
} = await import(dwUrl);

const fixtures = ["workflow_linear", "workflow_fanout"];
const results = [];
for (const name of fixtures) {
  const path = join(root, `native/crates/nc-code-workflow/tests/fixtures/${name}.ts`);
  const scriptText = readFileSync(path, "utf8");
  const compiled = lowerWorkflowScript(scriptText);
  if (!compiled.ok || compiled.lowered === undefined) {
    console.error(`${name}: compile failed`, JSON.stringify(compiled.diagnostics, null, 1));
    process.exit(1);
  }
  const { schemas } = synthesizeWorkflowSchemas(scriptText);
  const table = collectSites(createWorkflowProgram(scriptText));
  const askSpecs = buildAskSpecs(table, schemas);
  const payload = {
    name,
    scriptHash: createHash("sha256").update(scriptText, "utf8").digest("hex"),
    // The fixture header pins the expected site table; storing it lets both benches assert.
    siteIds: compiled.lowered.siteIds,
    lowered: compiled.lowered.code,
    askSpecs: Object.fromEntries(
      [...askSpecs.entries()].map(([siteId, spec]) => [
        siteId,
        spec.typed ? { typed: true, schema: spec.schema } : { typed: false },
      ]),
    ),
    generatedBy: "scripts/bench/gen-workflow-fixtures.mjs",
  };
  const out = join(root, `native/crates/nc-code-workflow/tests/fixtures/${name}.json`);
  writeFileSync(out, JSON.stringify(payload, null, 2) + "\n");
  results.push({ name, siteIds: payload.siteIds, askSites: Object.keys(payload.askSpecs) });
}
console.log(JSON.stringify({ generated: results }, null, 1));
