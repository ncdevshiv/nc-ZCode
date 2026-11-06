// Canonical parity/benchmark fixture: linear run — one actor, one typed ask,
// one world read, one report, one markdown artifact, one phase marker.
// Both runtimes (TS dynamic-workflow and native zcode-workflow) must compile this
// to the same site table:
//   actor#1      agent("planner", …)
//   ask#1        planner.ask<Plan>(…)
//   world-read#1 files.read("README.md")
//   report#1     report(…)
//   artifact#1   artifact.markdown(…)
// phase("plan") has no site id (marker only).
// The typed ask schema comes from the Plan interface (typed ask → submit_result).

interface Plan {
  steps: string[];
  rationale: string;
}

phase("plan");

const planner = agent("planner", "You are a planning subagent. Produce short, concrete plans.");
const plan = await planner.ask<Plan>(
  "Draft a 3-step plan for documenting this repository. Fields: steps (3 short strings), rationale (one sentence).",
);

phase("execute");

const readme = await files.read("README.md");
report({ readmeBytes: (readme as { length: number }).length, planSteps: plan.steps.length });

// Content-family artifact publishes return a promise (verified: markdown is a content op),
// so the script awaits it — an unhandled rejection would kill the sandbox child.
await artifact.markdown(
  "summary",
  "## Plan\n\n" + plan.steps.map((s, i) => `${i + 1}. ${s}`).join("\n"),
);

return { rationale: plan.rationale, steps: plan.steps };
