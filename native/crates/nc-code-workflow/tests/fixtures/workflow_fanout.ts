// Canonical parity/benchmark fixture: fan-out — 4 concurrent untyped asks on 4 named
// actors, joined with Promise.all, then one typed meta ask.
// Verified against the real front-end (2026-10-30, dist lowerWorkflowScript):
//   siteIds = ["actor#1","ask#1","report#1","actor#2","ask#2"]
// Site ids are STATIC per source call site: the agent()/ask() inside names.map() are each
// ONE site executed 4 times (ordinals ask#1@1..@4 on 4 distinct actors); the coordinator
// ask is ask#2 (typed). Pin these exact ids — the benchmark asserts the same table.
// Purpose: exercise per-actor FIFO, maxConcurrency gating, untyped → final-text results,
// typed → submit_result validation, and Promise.all join inside the sandbox.

const names = ["alpha", "beta", "gamma", "delta"] as const;

const workers = names.map((n) => agent(`${n}-worker`, `You are the ${n} worker.`));

const findings = await Promise.all(
  workers.map((w, i) =>
    w.ask(`Worker ${names[i]}: reply with exactly one short sentence describing task ${i + 1}.`),
  ),
);

report({ workers: findings.length });

interface Merge {
  merged: string;
  count: number;
}

const coordinator = agent("coordinator", "You merge worker findings.");
const merged = await coordinator.ask<Merge>(
  "Merge these findings into one sentence and report the count. findings=\n" +
    findings.map((f, i) => `${i}: ${f}`).join("\n"),
);

return { merged: merged.merged, count: merged.count };
