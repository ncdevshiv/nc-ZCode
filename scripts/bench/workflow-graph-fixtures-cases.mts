// Real-script half of the bound_causality_graph fixture corpus. Split from
// gen-workflow-graph-fixtures.mts to keep every linted file under the max-lines bound;
// the generator owns analysis + expectations, this module only lists scripts.

export interface RealCasesApi {
  MAX_STEPS: number;
  MAX_LANES: number;
  MAX_PHASES: number;
  /** Analyze a real script and add it to the corpus. */
  addCase: (name: string, script: string) => unknown;
  /** Read the relative path of the fixture in the warehouse. */
  readFixture: (relative: string) => string;
}

export function addRealCases(api: RealCasesApi): void {
  //─────────────────────────────────────────────────────────────────────
  // A. The two repo fixtures both runtimes already pin.
  //─────────────────────────────────────────────────────────────────────

  api.addCase(
    "fixture/workflow_linear",
    api.readFixture("native/crates/nc-code-workflow/tests/fixtures/workflow_linear.ts"),
  );
  api.addCase(
    "fixture/workflow_fanout",
    api.readFixture("native/crates/nc-code-workflow/tests/fixtures/workflow_fanout.ts"),
  );

  //─────────────────────────────────────────────────────────────────────
  // B. Phase vocabulary, loops, participants and hand-offs.
  //─────────────────────────────────────────────────────────────────────

  api.addCase(
    "phases-loops",
    `
phase("plan");
const a = agent("a", "A");
let r = await a.ask("plan");
phase("review");
await a.ask("review");
while (r. length > 3) {
  phase("revise");
  r = await a.ask("revise " + r);
  phase("review");
  await a.ask("re-review " + r);
}
phase("done");
await a.ask("done " + r);
return r;
`,
  );

  // The loop closes from the later phase back to the earlier one without a same-direction
  // forward edge: the quotient keeps a `back: true` edge.
  api.addCase(
    "phases-back-edge",
    `
phase("plan");const a = agent("a", "A");
let r = await a.ask("plan");
while (r.length > 3) {
  phase("review");
  await a.ask("review " + r);
  phase("revise");
  r = await a.ask("revise " + r);
}
phase("done");
await a.ask("done " + r);
return r;
`,
  );

  api.addCase(
    "handoff-carry",
    `
phase("cycle");
const a = agent("a", "A");
const b = agent("b", "B");
let r = await a.ask("seed");
while (r.length > 1) {
  r = await a.ask("a:" + r);
  r = await b.ask("b:" + r);
}
return r;
`,
  );

  // Two actors in one phase with no artifact passing: the hand-off exists but has no types.
  api.addCase(
    "handoff-sequence",
    `
const a = agent("a", "A");
const b = agent("b", "B");
await a.ask("first");
await b.ask("second");
return 1;
`,
  );

  api.addCase(
    "strand-alongside",
    `
phase("plan");
const p = (async () => {
  const a = agent("a", "A");
  return await a.ask("slow");
})();
phase("execute");
const r = await p;
return r;
`,
  );

  api.addCase(
    "zero-phase-world-read",
    `
const reader = agent("reader", "You read files.");
const doc = await files.read("README.md");
const summary = await reader.ask("Summarize: " + doc);
return summary;
`,
  );

  api.addCase(
    "may-set-lanes",
    `
const a = agent("a", "A");
const b = agent("b", "B");
const chosen = Math.random() > 0.5 ? a : b;
await chosen.ask("pick");
return 1;
`,
  );

  api.addCase(
    "empty-label-fallback",
    `
const r = await agent("", "A").ask("q");
return r;
`,
  );

  api.addCase(
    "family-members",
    `
const r = await Promise.all([1, 2, 3].map(async (i) => {
  const w = agent(\`w\${i}\`, "W");
  return await w.ask("q\${i}");
}));
return r;
`,
  );

  api.addCase(
    "family-many",
    `
const xs = [1, 2, 3];
xs.push(4);
const r = await Promise.all(xs.map(async (i) => {
  const w = agent(\`w\${i}\`, "W");
  return await w.ask("q\${i}");
}));
return r;
`,
  );

  //─────────────────────────────────────────────────────────────────────
  // C. Text bounds: JS `.length` is UTF-16, so a boundary may split a surrogate pair.
  //─────────────────────────────────────────────────────────────────────

  const EMOJI = "\u{1F600}";
  const longLaneName = `${"a".repeat(127)}${EMOJI}`; // 129 units → 127 units
  const exactLaneName = "b".repeat(128); // exactly at the limit, untouched
  const wideLaneName = `${"c".repeat(126)}${EMOJI}z`; // 129 units → 128 (pair kept)
  const longPhaseName = `${"p".repeat(127)}${EMOJI}`; // 129 units → 127 units
  const longPatternHead = `${"h".repeat(127)}${EMOJI}`; // 129 units → 127 units

  api.addCase(
    "names-utf16",
    `
phase("${longPhaseName}");
const n = [1];
const fam = n.map((x) => agent(\`${longPatternHead}\${x}tail\`, "W"));
const r1 = await agent("${longLaneName}", "A").ask("q1");
const r2 = await agent("${exactLaneName}", "B").ask("q2");
const r3 = await agent("${wideLaneName}", "C").ask("q3");
const r4 = await fam[0].ask("q4");
const r5 = await agent("", "A").ask("q5");
const r6 = await agent(\`researcher\${1}\`, "A").ask("q6");
return r1;
`,
  );

  //─────────────────────────────────────────────────────────────────────
  // D. Limit crosses (real scripts) and edges of the empty graph.
  //─────────────────────────────────────────────────────────────────────

  const manyAsks = Array.from(
    { length: api.MAX_STEPS + 6 },
    (_, index) => `r = await a.ask("q${index + 1}");`,
  ).join("\n");
  api.addCase(
    "over-steps",
    `
const a = agent("solo", "S");
let r = "";
${manyAsks}
return r;
`,
  );

  const manyActors = Array.from(
    { length: api.MAX_LANES + 8 },
    (_, index) =>
      `const a${index} = agent("actor-${index}", "A");\nawait a${index}.ask("q${index}");`,
  ).join("\n");
  api.addCase(
    "over-lanes",
    `
${manyActors}
return 1;
`,
  );

  const manyPhases = Array.from(
    { length: api.MAX_PHASES + 1 },
    (_, index) => `phase("p${index}");\nawait a.ask("q${index}");`,
  ).join("\n");
  api.addCase(
    "over-phases",
    `
const a = agent("solo", "S");
${manyPhases}
return 1;
`,
  );

  api.addCase("no-steps", `return 1;`);
  api.addCase("diagnostics", `const x: number = "no";`);
}
