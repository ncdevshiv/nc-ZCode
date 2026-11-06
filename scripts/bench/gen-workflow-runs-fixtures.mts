/* eslint-disable max-lines -- the only generator of reduction corpus: branches, boundaries and deterministic random walks must share the same set of fold/record auxiliaries. Splitting the file will make "which event covers which branch" spread out. */
// Generates the Rust `workflowRuns` reducer fixtures from the REAL TypeScript reducer.
//
// Usage (from the repo root):
// pnpm exec tsx scripts/bench/gen-workflow-runs-fixtures.mts
//
// Writes `native/crates/nc-code-host/tests/fixtures/workflow_runs_fixtures.json` as
// [{ name, previous: state | null, envelope, expected: state | null }]
// where every `expected` is the untouched output of `reduceWorkflowRunsState` (JSON-normalized):
// `null` means the TS reducer returned `null` (no semantic change). The Rust port
// (`native/crates/nc-code-host/src/workflow_projection.rs`) is asserted against this corpus.
//
// Note on the watermark: `lastEventSequence` is monotonic, so any event carrying a higher
// sequence produces a new state even when its payload is ignored. `assertIgnored` therefore
// asserts “only the watermark moved”; `assertNull` is reserved for the real `null` paths
// (missing runId / eventType, and a verbatim replay of an already-applied envelope).

import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

import {
  reduceWorkflowRunsState,
  type WorkflowRunProgressEnvelope,
} from "../../packages/shared/src/zcode-protocol-v4/workflow-runs-reducer.js";
import type { WorkflowRunsState } from "../../packages/shared/src/zcode-protocol-v4/workflow-runs.js";

interface FixtureCase {
  name: string;
  previous: WorkflowRunsState | null;
  envelope: WorkflowRunProgressEnvelope;
  expected: WorkflowRunsState | null;
}

type Payload = Record<string, unknown>;

const cases: FixtureCase[] = [];
const coveredEventTypes = new Set<string>();

function normalize<T>(value: T): T {  return value === null || value === undefined ? value : JSON.parse(JSON.stringify(value));
}

function reduce(previous: WorkflowRunsState | null, envelope: WorkflowRunProgressEnvelope) {
  return reduceWorkflowRunsState(previous ?? undefined, envelope);
}

/** Record one case; returns the oracle output (null = no change). */
function pushCase(
  name: string,
  previous: WorkflowRunsState | null,
  envelope: WorkflowRunProgressEnvelope,
): WorkflowRunsState | null {
  const prior = normalize(previous);
  const expected = normalize(reduce(prior, envelope));
  cases.push({ name, previous: prior, envelope: normalize(envelope), expected });
  coveredEventTypes.add(envelope.eventType ?? "<missing>");
  return expected;
}

/** Fold without recording (drives a setup state forward). */
function foldState(
  previous: WorkflowRunsState | null,
  envelope: WorkflowRunProgressEnvelope,
): WorkflowRunsState | null {
  const out = reduce(previous, envelope);
  return out === null ? previous : normalize(out);
}

function foldAll(
  events: WorkflowRunProgressEnvelope[],
  start: WorkflowRunsState | null = null,
): WorkflowRunsState | null {
  return events.reduce<WorkflowRunsState | null>(
    (state, envelope) => foldState(state, envelope),
    start,
  );
}

function makeRun(runId: string) {
  let sequence = 0;
  const event = (
    eventType: string,
    payload: Payload = {},
    extra: Record<string, unknown> = {},
  ): WorkflowRunProgressEnvelope => {
    sequence += 1;
    return { runId, eventType, sequence, payload, ...extra };
  };
  const step = (
    name: string,
    previous: WorkflowRunsState | null,
    envelope: WorkflowRunProgressEnvelope,
  ): WorkflowRunsState | null => {
    const expected = pushCase(name, previous, envelope);
    return expected ?? previous;
  };
  return { event, step, fold: foldState };
}

/** True `null`: the reducer bails out (missing identity) or the envelope is a verbatim replay. */
function assertNull(
  name: string,
  previous: WorkflowRunsState | null,
  envelope: WorkflowRunProgressEnvelope,
): WorkflowRunsState | null {
  const out = pushCase(name, previous, envelope);
  if (out !== null) throw new Error(`${name}: expected null, got a new state`);
  return previous;
}

/** The payload was ignored: only `lastEventSequence` may have moved. */
function assertIgnored(
  name: string,
  previous: WorkflowRunsState | null,
  envelope: WorkflowRunProgressEnvelope,
): WorkflowRunsState | null {
  const expected = pushCase(name, previous, envelope);
  if (expected === null) throw new Error(`${name}: expected the watermark to advance`);
  const runId = envelope.runId;
  const before = previous?.runs.find((run) => run.runId === runId);
  const after = expected.runs.find((run) => run.runId === runId);
  if (before === undefined || after === undefined) {
    throw new Error(`${name}: run ${String(runId)} missing from the state`);
  }
  const strip = (run: WorkflowRunsState["runs"][number]) =>
    JSON.stringify({ ...run, lastEventSequence: undefined });
  if (strip(before) !== strip(after)) {
    throw new Error(`${name}: run body changed\nbefore=${strip(before)}\nafter=${strip(after)}`);
  }
  return expected;
}

/** Apply the same envelope twice: the first must change state, the replay must return null. */
function replayCase(
  name: string,
  previous: WorkflowRunsState | null,
  envelope: WorkflowRunProgressEnvelope,
): WorkflowRunsState | null {
  const first = pushCase(`${name}/first`, previous, envelope);
  if (first === null) {
    throw new Error(`${name}: first application unexpectedly produced no change`);
  }
  assertNull(`${name}/replay`, first, envelope);
  return first;
}

function mulberry32(seed: number) {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

// ─────────────────────────────────────────────────────────────────────────────
// A. One realistic linear run, folded event by event from the empty state.
// ─────────────────────────────────────────────────────────────────────────────
const linear = makeRun("run-linear");
let state: WorkflowRunsState | null = null;

state = linear.step(
  "linear/01-run-launched",
  state,
  linear.event("run-launched", {
    inputId: "input-1",
    phaseNames: ["intake", "work", "review"],
    phaseAlongside: [[], [], [0]],
  }),
);
state = linear.step(
  "linear/02-run-started",
  state,
  linear.event(
    "run-started",
    {
      caps: { maxConcurrency: 6 },
      concurrencyCeiling: 12,
      subagentModel: "anthropic/claude-sonnet-4-5$high",
    },
    { toolCallId: "tool-call-1" },
  ),
);
state = linear.step(
  "linear/03-phase-entered-intake",
  state,
  linear.event("phase-entered", { name: "intake", ordinal: 1 }),
);
state = linear.step(
  "linear/04-actor-created",
  state,
  linear.event(
    "actor-created",
    { actor: { siteId: "agent#1", ordinal: 0 }, name: "scout", phaseName: "intake" },
    { actorSessionId: "actor-session-1" },
  ),
);
state = linear.step(
  "linear/05-node-queued",
  state,
  linear.event("node-queued", {
    instance: { siteId: "agent#1", ordinal: 0 },
    actor: { siteId: "agent#1", ordinal: 0 },
    kind: "ask",
    phaseName: "intake",
    instructionsHead: "Read the reducer and list every branch it handles",
  }),
);
state = linear.step(
  "linear/06-node-dispatched",
  state,
  linear.event("node-dispatched", {
    instance: { siteId: "agent#1", ordinal: 0 },
    actor: { siteId: "agent#1", ordinal: 0 },
  }),
);
state = linear.step(
  "linear/07-node-executing",
  state,
  linear.event("node-executing", {
    instance: { siteId: "agent#1", ordinal: 0 },
    actor: { siteId: "agent#1", ordinal: 0 },
  }),
);
state = linear.step(
  "linear/08-node-progress-first",
  state,
  linear.event("node-progress", {
    instance: { siteId: "agent#1", ordinal: 0 },
    turn: 1,
    toolCalls: 0,
  }),
);
state = linear.step(
  "linear/09-node-progress-tool",
  state,
  linear.event("node-progress", {
    instance: { siteId: "agent#1", ordinal: 0 },
    turn: 1,
    toolCalls: 1,
    lastTool: {
      name: "Read",
      target: "packages/shared/src/zcode-protocol-v4/workflow-runs-reducer.ts",
    },
  }),
);
state = linear.step(
  "linear/10-node-waiting",
  state,
  linear.event("node-waiting", {
    instance: { siteId: "agent#1", ordinal: 0 },
    actor: { siteId: "agent#1", ordinal: 0 },
  }),
);
state = linear.step(
  "linear/11-node-executing-again",
  state,
  linear.event("node-executing", {
    instance: { siteId: "agent#1", ordinal: 0 },
    actor: { siteId: "agent#1", ordinal: 0 },
  }),
);
state = linear.step(
  "linear/12-report-object",
  state,
  linear.event("report", {
    instance: { siteId: "report#1", ordinal: 0 },
    item: { step: "scan", files: 12, ok: true },
  }),
);
state = linear.step(
  "linear/13-node-settled",
  state,
  linear.event("node-settled", {
    instance: { siteId: "agent#1", ordinal: 0 },
    actor: { siteId: "agent#1", ordinal: 0 },
    outcome: "ok",
  }),
);
state = linear.step(
  "linear/14-actor-created-second",
  state,
  linear.event("actor-created", { actor: { siteId: "agent#2", ordinal: 0 }, name: "writer" }),
);
state = linear.step(
  "linear/15-node-queued-world-read",
  state,
  linear.event("node-queued", {
    instance: { siteId: "world#1", ordinal: 0 },
    kind: "world-read",
  }),
);
state = linear.step(
  "linear/16-node-settled-world-read",
  state,
  linear.event("node-settled", {
    instance: { siteId: "world#1", ordinal: 0 },
    outcome: "ok",
    cached: true,
  }),
);
state = linear.step(
  "linear/17-phase-entered-work",
  state,
  linear.event("phase-entered", { name: "work", ordinal: 1 }),
);
state = linear.step(
  "linear/18-usage-updated",
  state,
  linear.event("usage-updated", { spentTokens: 4096 }),
);
state = linear.step(
  "linear/19-concurrency-changed",
  state,
  linear.event("concurrency-changed", {
    next: 4,
    previous: 6,
    key: "anthropic/claude-sonnet-4-5",
    cooldownMs: 1500,
    reason: "throttled",
    ceiling: 99,
  }),
);
state = linear.step(
  "linear/20-escalation-raised",
  state,
  linear.event("escalation-raised", {
    qid: "q-1",
    question: "Should I rewrite the reducer or port it?",
    actor: { siteId: "agent#2", ordinal: 0 },
    actorName: "writer",
    context: "two paths look plausible",
    askedAt: 1_760_000_000_000,
  }),
);
state = linear.step(
  "linear/21-escalation-resolved",
  state,
  linear.event("escalation-resolved", { qid: "q-1", answer: "port it" }),
);
state = linear.step(
  "linear/22-artifact-published-chart",
  state,
  linear.event("artifact-published", {
    artifact: {
      id: "chart-1",
      kind: "chart",
      title: "Throughput",
      version: 1,
      contentType: "application/json",
      bytes: 512,
      primary: true,
      spec: { ignored: true },
      publishedAt: 1_760_000_000_000,
    },
  }),
);
state = linear.step(
  "linear/23-report-tagged",
  state,
  linear.event("report", {
    instance: { siteId: "report#2", ordinal: 0 },
    item: { at: 1, value: 42 },
    artifactId: "chart-1",
  }),
);
state = linear.step(
  "linear/24-artifact-published-v2",
  state,
  linear.event("artifact-published", {
    artifact: { id: "chart-1", kind: "chart", title: "Throughput v2", version: 2 },
  }),
);
state = linear.step(
  "linear/25-artifact-failed",
  state,
  linear.event("artifact-failed", { artifactId: "file-1", reason: "missing file" }),
);
state = linear.step("linear/26-log", state, linear.event("log", { line: "still running" }));
state = linear.step("linear/27-compaction", state, linear.event("compaction", { tokens: 100 }));
state = linear.step(
  "linear/28-run-settled-completed",
  state,
  linear.event("run-settled", { status: "completed", resumable: false }),
);

// ─────────────────────────────────────────────────────────────────────────────
// B. run-started: resume re-arm, the run's own concurrency limit, subagent model.
// ─────────────────────────────────────────────────────────────────────────────
const arm = makeRun("run-arm");
let armState = foldAll([
  arm.event("run-started", { caps: { maxConcurrency: 3 }, concurrencyCeiling: 6 }),
  arm.event("concurrency-changed", {
    next: 2,
    previous: 3,
    key: "provider/model",
    cooldownMs: 500,
    reason: "throttled",
  }),
  arm.event("escalation-raised", {
    qid: "q-arm",
    question: "Which branch?",
    actor: { siteId: "agent#1", ordinal: 0 },
  }),
  arm.event("run-settled", {
    status: "stopped",
    stopReason: "superseded",
    supersededBy: "run-next",
    resumable: true,
    error: { message: "stopped by amend" },
  }),
]);
armState = arm.step(
  "run-started/re-arm-clears-settlement",
  armState,
  arm.event("run-started", {
    caps: { maxConcurrency: 3 },
    concurrencyCeiling: 6,
    resumedFrom: "run-prev",
    subagentModel: "openai/gpt-5-mini",
  }),
);
armState = arm.step(
  "run-started/without-ceiling-keeps-known",
  armState,
  arm.event("run-started", { caps: { maxConcurrency: 2 } }),
);
armState = arm.step(
  "run-started/ceiling-too-large-ignored",
  armState,
  arm.event("run-started", { concurrencyCeiling: 1025 }),
);
armState = arm.step(
  "run-started/ceiling-at-bound-kept",
  armState,
  arm.event("run-started", { concurrencyCeiling: 1024, caps: { maxConcurrency: 8 } }),
);
armState = arm.step(
  "run-started/limit-equals-ceiling-not-recorded",
  armState,
  arm.event("run-started", { concurrencyCeiling: 8, caps: { maxConcurrency: 8 } }),
);
armState = arm.step(
  "run-started/limit-below-ceiling-recorded",
  armState,
  arm.event("run-started", { concurrencyCeiling: 8, caps: { maxConcurrency: 5 } }),
);
armState = arm.step(
  "run-started/subagent-model-trimmed",
  armState,
  arm.event("run-started", { subagentModel: "  openai/gpt-5-mini$low  " }),
);
armState = arm.step(
  "run-started/subagent-model-too-long-keeps-known",
  armState,
  arm.event("run-started", { subagentModel: "x".repeat(300) }),
);
armState = arm.step(
  "run-started/subagent-model-at-bound",
  armState,
  arm.event("run-started", { subagentModel: "m".repeat(256) }),
);
arm.step(
  "run-started/subagent-model-empty-dropped",
  armState,
  arm.event("run-started", { subagentModel: "   " }),
);

const freshStart = makeRun("run-start-variants");
const freshStartState = foldAll([
  freshStart.event("run-started", { caps: { maxConcurrency: 4 }, concurrencyCeiling: 8 }),
]);
freshStart.step(
  "run-started/without-conditions-keeps-ceiling",
  freshStartState,
  freshStart.event("run-started", { concurrencyCeiling: 8 }),
);

// ─────────────────────────────────────────────────────────────────────────────
// C. actor-created.
// ─────────────────────────────────────────────────────────────────────────────
const actors = makeRun("run-actors");
let actorState = foldAll([
  actors.event("run-started", { caps: { maxConcurrency: 4 }, concurrencyCeiling: 8 }),
]);
actorState = actors.step(
  "actor-created/minimal-ref",
  actorState,
  actors.event("actor-created", { actor: { siteId: "agent#1", ordinal: 0 } }),
);
actorState = actors.step(
  "actor-created/full-fields",
  actorState,
  actors.event(
    "actor-created",
    { actor: { siteId: "agent#1", ordinal: 1 }, name: "reader", phaseName: "intake" },
    { actorSessionId: "session-actor-1" },
  ),
);
actorState = actors.step(
  "actor-created/duplicate-upserts-name",
  actorState,
  actors.event("actor-created", { actor: { siteId: "agent#1", ordinal: 1 }, name: "reader-v2" }),
);
actorState = actors.step(
  "actor-created/empty-strings-absent",
  actorState,
  actors.event("actor-created", {
    actor: { siteId: "agent#3", ordinal: 0 },
    name: "",
    phaseName: "",
  }),
);
actorState = actors.step(
  "actor-created/name-truncated-128",
  actorState,
  actors.event("actor-created", {
    actor: { siteId: "agent#4", ordinal: 0 },
    name: "n".repeat(200),
  }),
);
actorState = actors.step(
  "actor-created/phase-name-truncated-128",
  actorState,
  actors.event("actor-created", {
    actor: { siteId: "agent#5", ordinal: 0 },
    phaseName: "p".repeat(200),
  }),
);
actorState = assertIgnored(
  "actor-created/missing-ref-ignored",
  actorState,
  actors.event("actor-created", { name: "ghost" }),
);
actorState = assertIgnored(
  "actor-created/empty-site-id-ignored",
  actorState,
  actors.event("actor-created", { actor: { siteId: "", ordinal: 0 } }),
);
actorState = assertIgnored(
  "actor-created/string-ordinal-ignored",
  actorState,
  actors.event("actor-created", { actor: { siteId: "agent#6", ordinal: "0" } }),
);
assertIgnored(
  "actor-created/array-ref-ignored",
  actorState,
  actors.event("actor-created", { actor: [{ siteId: "agent#6", ordinal: 0 }] }),
);

const terminalActors = makeRun("run-terminal-actors");
const runningTerminal = foldAll([
  terminalActors.event("run-started", { caps: { maxConcurrency: 2 }, concurrencyCeiling: 4 }),
  terminalActors.event("actor-created", { actor: { siteId: "agent#1", ordinal: 0 } }),
  terminalActors.event("node-queued", {
    instance: { siteId: "agent#1", ordinal: 0 },
    actor: { siteId: "agent#1", ordinal: 0 },
    kind: "ask",
  }),
  terminalActors.event("node-dispatched", {
    instance: { siteId: "agent#1", ordinal: 0 },
    actor: { siteId: "agent#1", ordinal: 0 },
  }),
  terminalActors.event("node-executing", {
    instance: { siteId: "agent#1", ordinal: 0 },
    actor: { siteId: "agent#1", ordinal: 0 },
  }),
]);
const withRunningActor = terminalActors.step(
  "actor-created/running-derived-from-executing-node",
  runningTerminal,
  terminalActors.event("actor-created", { actor: { siteId: "agent#2", ordinal: 0 } }),
);
const terminalState = foldState(
  withRunningActor,
  terminalActors.event("run-settled", { status: "completed" }),
);
terminalActors.step(
  "actor-created/terminal-run-completes-everyone",
  terminalState,
  terminalActors.event("actor-created", { actor: { siteId: "agent#3", ordinal: 0 } }),
);

// ─────────────────────────────────────────────────────────────────────────────
// D. node-*: phases, kind/outcome/cached carry, actor ref carry, step counting.
// ─────────────────────────────────────────────────────────────────────────────
const nodes = makeRun("run-nodes");
let nodeState = foldAll([
  nodes.event("run-started", { caps: { maxConcurrency: 4 }, concurrencyCeiling: 8 }),
  nodes.event("actor-created", { actor: { siteId: "agent#1", ordinal: 0 }, name: "worker" }),
]);
nodeState = nodes.step(
  "node/queued",
  nodeState,
  nodes.event("node-queued", {
    instance: { siteId: "agent#1", ordinal: 0 },
    actor: { siteId: "agent#1", ordinal: 0 },
    kind: "ask",
    phaseName: "intake",
    instructionsHead: "Do the thing",
  }),
);
nodeState = nodes.step(
  "node/dispatched",
  nodeState,
  nodes.event("node-dispatched", {
    instance: { siteId: "agent#1", ordinal: 0 },
    actor: { siteId: "agent#1", ordinal: 0 },
  }),
);
nodeState = nodes.step(
  "node/dispatched-replay-new-sequence",
  nodeState,
  nodes.event("node-dispatched", {
    instance: { siteId: "agent#1", ordinal: 0 },
    actor: { siteId: "agent#1", ordinal: 0 },
  }),
);
nodeState = nodes.step(
  "node/executing",
  nodeState,
  nodes.event("node-executing", { instance: { siteId: "agent#1", ordinal: 0 } }),
);
nodeState = nodes.step(
  "node/waiting",
  nodeState,
  nodes.event("node-waiting", { instance: { siteId: "agent#1", ordinal: 0 } }),
);
nodeState = nodes.step(
  "node/repairing",
  nodeState,
  nodes.event("node-repairing", { instance: { siteId: "agent#1", ordinal: 0 } }),
);
nodeState = nodes.step(
  "node/nudged",
  nodeState,
  nodes.event("node-nudged", { instance: { siteId: "agent#1", ordinal: 0 } }),
);
nodeState = nodes.step(
  "node/settled-invalid-outcome-and-kind-ignored",
  nodeState,
  nodes.event("node-settled", {
    instance: { siteId: "agent#1", ordinal: 0 },
    outcome: "maybe",
    cached: false,
    kind: "not-a-kind",
  }),
);
nodeState = nodes.step(
  "node/settled-failed",
  nodeState,
  nodes.event("node-settled", {
    instance: { siteId: "agent#1", ordinal: 0 },
    outcome: "failed",
  }),
);
nodeState = nodes.step(
  "node/requeued-resets-progress",
  nodeState,
  nodes.event("node-queued", {
    instance: { siteId: "agent#1", ordinal: 0 },
    kind: "ask",
    instructionsHead: "A revised instruction",
    phaseName: "work",
  }),
);
nodeState = nodes.step(
  "node/redispatch-counts-again",
  nodeState,
  nodes.event("node-dispatched", {
    instance: { siteId: "agent#1", ordinal: 0 },
    actor: { siteId: "agent#1", ordinal: 0 },
  }),
);
nodeState = nodes.step(
  "node/settled-cached-newborn",
  nodeState,
  nodes.event("node-settled", {
    instance: { siteId: "agent#1", ordinal: 1 },
    actor: { siteId: "agent#1", ordinal: 0 },
    cached: true,
    outcome: "ok",
    kind: "ask",
    phaseName: "work",
  }),
);
nodeState = nodes.step(
  "node/phase-name-carried",
  nodeState,
  nodes.event("node-executing", { instance: { siteId: "agent#1", ordinal: 1 } }),
);
nodeState = nodes.step(
  "node/instructions-head-truncated-240",
  nodeState,
  nodes.event("node-queued", {
    instance: { siteId: "agent#1", ordinal: 2 },
    kind: "ask",
    instructionsHead: "i".repeat(400),
  }),
);
nodeState = nodes.step(
  "node/actor-ref-carried-from-previous",
  nodeState,
  nodes.event("node-executing", { instance: { siteId: "agent#1", ordinal: 2 } }),
);
nodeState = nodes.step(
  "node/actor-ref-replaced",
  nodeState,
  nodes.event("node-executing", {
    instance: { siteId: "agent#1", ordinal: 2 },
    actor: { siteId: "agent#1", ordinal: 0 },
  }),
);
nodeState = nodes.step(
  "node/world-read-kind",
  nodeState,
  nodes.event("node-queued", { instance: { siteId: "world#1", ordinal: 0 }, kind: "world-read" }),
);
nodeState = nodes.step(
  "node/invalid-kind-keeps-previous",
  nodeState,
  nodes.event("node-dispatched", { instance: { siteId: "world#1", ordinal: 0 }, kind: "ask" }),
);
assertIgnored(
  "node/missing-instance-ignored",
  nodeState,
  nodes.event("node-executing", { actor: { siteId: "agent#1", ordinal: 0 } }),
);

const nodeReplay = makeRun("run-node-replay");
let replayState = foldAll([
  nodeReplay.event("run-started", { caps: { maxConcurrency: 2 }, concurrencyCeiling: 4 }),
  nodeReplay.event("node-queued", { instance: { siteId: "n#1", ordinal: 0 }, kind: "ask" }),
]);
replayState = replayCase(
  "node/dispatched-replay-same-envelope",
  replayState,
  nodeReplay.event("node-dispatched", { instance: { siteId: "n#1", ordinal: 0 } }),
);
nodeReplay.step(
  "node/settled-after-replay",
  replayState,
  nodeReplay.event("node-settled", {
    instance: { siteId: "n#1", ordinal: 0 },
    outcome: "cancelled",
  }),
);

// nodesUsed: a node born on `node-dispatched` (no queued) still counts as one step.
const countRun = makeRun("run-count");
let countState = foldAll([
  countRun.event("run-started", { caps: { maxConcurrency: 2 }, concurrencyCeiling: 4 }),
]);
countState = countRun.step(
  "usage/dispatched-born-counts-one",
  countState,
  countRun.event("node-dispatched", { instance: { siteId: "n#1", ordinal: 0 } }),
);
countState = countRun.step(
  "usage/second-dispatched-not-counted",
  countState,
  countRun.event("node-dispatched", { instance: { siteId: "n#1", ordinal: 0 } }),
);
countState = countRun.step(
  "usage/settled-node-not-counted",
  countState,
  countRun.event("node-settled", { instance: { siteId: "n#1", ordinal: 0 }, outcome: "ok" }),
);
countState = countRun.step(
  "usage/requeue",
  countState,
  countRun.event("node-queued", { instance: { siteId: "n#1", ordinal: 0 }, kind: "ask" }),
);
countRun.step(
  "usage/redispatch-counts-again",
  countState,
  countRun.event("node-dispatched", { instance: { siteId: "n#1", ordinal: 0 } }),
);

// ─────────────────────────────────────────────────────────────────────────────
// E. node-progress.
// ─────────────────────────────────────────────────────────────────────────────
const progress = makeRun("run-progress");
let progressState = foldAll([
  progress.event("run-started", { caps: { maxConcurrency: 2 }, concurrencyCeiling: 4 }),
  progress.event("node-queued", { instance: { siteId: "n#1", ordinal: 0 }, kind: "ask" }),
]);
progressState = progress.step(
  "node-progress/turn-and-tools",
  progressState,
  progress.event("node-progress", {
    instance: { siteId: "n#1", ordinal: 0 },
    turn: 2,
    toolCalls: 7,
    lastTool: { name: "Grep", target: "workflow" },
  }),
);
progressState = progress.step(
  "node-progress/only-tool-calls-keeps-turn",
  progressState,
  progress.event("node-progress", { instance: { siteId: "n#1", ordinal: 0 }, toolCalls: 8 }),
);
progressState = progress.step(
  "node-progress/zero-tool-calls-valid",
  progressState,
  progress.event("node-progress", { instance: { siteId: "n#1", ordinal: 0 }, toolCalls: 0 }),
);
progressState = assertIgnored(
  "node-progress/zero-turn-ignored",
  progressState,
  progress.event("node-progress", { instance: { siteId: "n#1", ordinal: 0 }, turn: 0 }),
);
progressState = assertIgnored(
  "node-progress/fractional-turn-ignored",
  progressState,
  progress.event("node-progress", { instance: { siteId: "n#1", ordinal: 0 }, turn: 1.5 }),
);
progressState = assertIgnored(
  "node-progress/last-tool-without-name-ignored",
  progressState,
  progress.event("node-progress", {
    instance: { siteId: "n#1", ordinal: 0 },
    lastTool: { target: "somewhere" },
  }),
);
progressState = assertIgnored(
  "node-progress/unknown-instance-ignored",
  progressState,
  progress.event("node-progress", { instance: { siteId: "n#9", ordinal: 0 }, turn: 3 }),
);
progressState = progress.step(
  "node-progress/last-tool-bounds",
  progressState,
  progress.event("node-progress", {
    instance: { siteId: "n#1", ordinal: 0 },
    lastTool: { name: "t".repeat(100), target: "p".repeat(200) },
  }),
);
progress.step(
  "node-progress/carried-by-lifecycle-event",
  progressState,
  progress.event("node-executing", { instance: { siteId: "n#1", ordinal: 0 } }),
);

// ─────────────────────────────────────────────────────────────────────────────
// F. report.
// ─────────────────────────────────────────────────────────────────────────────
const reports = makeRun("run-reports");
let reportState = foldAll([
  reports.event("run-started", { caps: { maxConcurrency: 2 }, concurrencyCeiling: 4 }),
]);
reportState = reports.step(
  "report/string-item",
  reportState,
  reports.event("report", { instance: { siteId: "r#1", ordinal: 0 }, item: "plain text" }),
);
reportState = reports.step(
  "report/object-item-pretty-json",
  reportState,
  reports.event("report", {
    instance: { siteId: "r#2", ordinal: 0 },
    item: { nested: { list: [1, 2, 3] }, flag: false },
  }),
);
reportState = reports.step(
  "report/null-item",
  reportState,
  reports.event("report", { instance: { siteId: "r#3", ordinal: 0 }, item: null }),
);
reportState = reports.step(
  "report/missing-item-empty-preview",
  reportState,
  reports.event("report", { instance: { siteId: "r#4", ordinal: 0 } }),
);
reportState = reports.step(
  "report/preview-truncated-2048",
  reportState,
  reports.event("report", { instance: { siteId: "r#5", ordinal: 0 }, item: "x".repeat(3000) }),
);
reportState = reports.step(
  "report/duplicate-instance-upserts",
  reportState,
  reports.event("report", { instance: { siteId: "r#1", ordinal: 0 }, item: "edited" }),
);
reportState = reports.step(
  "report/tagged-without-artifact-keeps-reports",
  reportState,
  reports.event("report", {
    instance: { siteId: "r#6", ordinal: 0 },
    item: "point",
    artifactId: "unknown-artifact",
  }),
);
assertIgnored(
  "report/missing-instance-ignored",
  reportState,
  reports.event("report", { item: "orphan" }),
);

const ranked = makeRun("run-report-ranked");
let rankedState = foldAll([
  ranked.event("run-started", { caps: { maxConcurrency: 2 }, concurrencyCeiling: 4 }),
  ranked.event("artifact-published", { artifact: { id: "board-1", kind: "board", version: 1 } }),
]);
rankedState = ranked.step(
  "report/tagged-increments-item-count",
  rankedState,
  ranked.event("report", {
    instance: { siteId: "r#1", ordinal: 0 },
    item: { x: 1 },
    artifactId: "board-1",
  }),
);
rankedState = ranked.step(
  "report/tagged-replay-same-instance-no-double-count",
  rankedState,
  ranked.event("report", {
    instance: { siteId: "r#1", ordinal: 0 },
    item: { x: 1 },
    artifactId: "board-1",
  }),
);
ranked.step(
  "report/untagged-never-touches-artifacts",
  rankedState,
  ranked.event("report", { instance: { siteId: "r#2", ordinal: 0 }, item: "plain" }),
);

// ─────────────────────────────────────────────────────────────────────────────
// G. artifact-published / artifact-failed.
// ─────────────────────────────────────────────────────────────────────────────
const artifacts = makeRun("run-artifacts");
let artifactState = foldAll([
  artifacts.event("run-started", { caps: { maxConcurrency: 2 }, concurrencyCeiling: 4 }),
]);
artifactState = artifacts.step(
  "artifact/file-with-metadata",
  artifactState,
  artifacts.event("artifact-published", {
    artifact: {
      id: "file-1",
      kind: "file",
      title: "report.csv",
      version: 1,
      contentType: "text/csv",
      bytes: 2048,
      primary: true,
      description: "dropped",
      sourcePath: "/workspace/report.csv",
      uri: "zcode-artifact://file-1/1",
      publishedAt: 1_760_000_000_000,
    },
  }),
);
artifactState = artifacts.step(
  "artifact/version-clamped-to-16",
  artifactState,
  artifacts.event("artifact-published", {
    artifact: { id: "file-2", kind: "markdown", version: 20 },
  }),
);
artifactState = artifacts.step(
  "artifact/same-id-new-version-replaces",
  artifactState,
  artifacts.event("artifact-published", {
    artifact: { id: "file-1", kind: "file", title: "report-v2.csv", version: 2, bytes: 4096 },
  }),
);
artifactState = artifacts.step(
  "artifact/id-title-content-type-truncated",
  artifactState,
  artifacts.event("artifact-published", {
    artifact: {
      id: "i".repeat(100),
      kind: "table",
      title: "t".repeat(200),
      version: 1,
      contentType: "c".repeat(200),
    },
  }),
);
artifactState = artifacts.step(
  "artifact/primary-false-absent",
  artifactState,
  artifacts.event("artifact-published", {
    artifact: { id: "table-2", kind: "metrics", version: 1, primary: false },
  }),
);
assertIgnored(
  "artifact/missing-id-ignored",
  artifactState,
  artifacts.event("artifact-published", { artifact: { kind: "file", version: 1 } }),
);
assertIgnored(
  "artifact/unknown-kind-ignored",
  artifactState,
  artifacts.event("artifact-published", { artifact: { id: "x", kind: "video", version: 1 } }),
);
assertIgnored(
  "artifact/zero-version-ignored",
  artifactState,
  artifacts.event("artifact-published", { artifact: { id: "x", kind: "file", version: 0 } }),
);
assertIgnored(
  "artifact/fractional-version-ignored",
  artifactState,
  artifacts.event("artifact-published", { artifact: { id: "x", kind: "file", version: 1.5 } }),
);
assertIgnored(
  "artifact/array-payload-ignored",
  artifactState,
  artifacts.event("artifact-published", {
    artifact: [{ id: "x", kind: "file", version: 1 }],
  }),
);

const failed = makeRun("run-artifact-failed");
const failedState = foldAll([
  failed.event("run-started", { caps: { maxConcurrency: 2 }, concurrencyCeiling: 4 }),
]);
replayCase(
  "artifact-failed/only-advances-watermark",
  failedState,
  failed.event("artifact-failed", { artifactId: "file-1", reason: "missing" }),
);

// ─────────────────────────────────────────────────────────────────────────────
// H. usage-updated.
// ─────────────────────────────────────────────────────────────────────────────
const usage = makeRun("run-usage");
let usageState = foldAll([
  usage.event("run-started", { caps: { maxConcurrency: 2 }, concurrencyCeiling: 4 }),
]);
usageState = usage.step(
  "usage-updated/sets-spent-tokens",
  usageState,
  usage.event("usage-updated", { spentTokens: 1234 }),
);
usageState = assertIgnored(
  "usage-updated/string-ignored",
  usageState,
  usage.event("usage-updated", { spentTokens: "1234" }),
);
usageState = assertIgnored(
  "usage-updated/missing-ignored",
  usageState,
  usage.event("usage-updated", {}),
);
usage.step(
  "usage-updated/zero-valid",
  usageState,
  usage.event("usage-updated", { spentTokens: 0 }),
);

// ─────────────────────────────────────────────────────────────────────────────
// I. escalation-raised / escalation-resolved.
// ─────────────────────────────────────────────────────────────────────────────
const escalations = makeRun("run-escalations");
let escalationState = foldAll([
  escalations.event("run-started", { caps: { maxConcurrency: 2 }, concurrencyCeiling: 4 }),
]);
escalationState = escalations.step(
  "escalation/full-record",
  escalationState,
  escalations.event("escalation-raised", {
    qid: "q-1",
    question: "Which file should win?",
    actor: { siteId: "agent#1", ordinal: 2 },
    actorName: "reader",
    context: "both exist",
    askedAt: 1_760_000_000_123,
  }),
);
escalationState = escalations.step(
  "escalation/minimal-record",
  escalationState,
  escalations.event("escalation-raised", { qid: "q-2", question: "And this one?" }),
);
escalationState = escalations.step(
  "escalation/question-truncated-2048",
  escalationState,
  escalations.event("escalation-raised", { qid: "q-3", question: "q".repeat(3000) }),
);
escalationState = escalations.step(
  "escalation/context-truncated-2048",
  escalationState,
  escalations.event("escalation-raised", {
    qid: "q-4",
    question: "short",
    context: "c".repeat(3000),
  }),
);
escalationState = escalations.step(
  "escalation/duplicate-qid-upserts",
  escalationState,
  escalations.event("escalation-raised", { qid: "q-2", question: "And this one, revised?" }),
);
escalationState = escalations.step(
  "escalation/invalid-actor-ref-still-recorded",
  escalationState,
  escalations.event("escalation-raised", {
    qid: "q-5",
    question: "No actor here",
    actor: { siteId: "", ordinal: 0 },
  }),
);
escalationState = assertIgnored(
  "escalation/missing-question-ignored",
  escalationState,
  escalations.event("escalation-raised", { qid: "q-6" }),
);
escalationState = assertIgnored(
  "escalation/missing-qid-ignored",
  escalationState,
  escalations.event("escalation-raised", { question: "orphan" }),
);
escalationState = escalations.step(
  "escalation/resolve-middle",
  escalationState,
  escalations.event("escalation-resolved", { qid: "q-2", answer: "this one" }),
);
escalationState = assertIgnored(
  "escalation/resolve-unknown-ignored",
  escalationState,
  escalations.event("escalation-resolved", { qid: "q-missing" }),
);
escalationState = assertIgnored(
  "escalation/resolve-missing-qid-ignored",
  escalationState,
  escalations.event("escalation-resolved", {}),
);
escalationState = escalations.step(
  "escalation/resolve-q-1",
  escalationState,
  escalations.event("escalation-resolved", { qid: "q-1" }),
);
escalationState = escalations.step(
  "escalation/resolve-q-3",
  escalationState,
  escalations.event("escalation-resolved", { qid: "q-3" }),
);
escalationState = escalations.step(
  "escalation/resolve-q-4",
  escalationState,
  escalations.event("escalation-resolved", { qid: "q-4" }),
);
escalations.step(
  "escalation/resolve-last-drops-key",
  escalationState,
  escalations.event("escalation-resolved", { qid: "q-5" }),
);

// ─────────────────────────────────────────────────────────────────────────────
// J. concurrency-changed.
// ─────────────────────────────────────────────────────────────────────────────
const concurrency = makeRun("run-concurrency");
let concurrencyState = foldAll([
  concurrency.event("run-started", { caps: { maxConcurrency: 4 }, concurrencyCeiling: 16 }),
]);
concurrencyState = concurrency.step(
  "concurrency/with-key-and-cooldown",
  concurrencyState,
  concurrency.event("concurrency-changed", {
    next: 3,
    previous: 4,
    key: "provider/model",
    cooldownMs: 250,
    reason: "throttled",
  }),
);
concurrencyState = concurrency.step(
  "concurrency/without-previous",
  concurrencyState,
  concurrency.event("concurrency-changed", { next: 2 }),
);
concurrencyState = concurrency.step(
  "concurrency/key-too-long-omitted",
  concurrencyState,
  concurrency.event("concurrency-changed", { next: 2, key: "k".repeat(300) }),
);
concurrencyState = concurrency.step(
  "concurrency/key-exactly-256-kept",
  concurrencyState,
  concurrency.event("concurrency-changed", { next: 2, key: "k".repeat(256) }),
);
concurrencyState = concurrency.step(
  "concurrency/idle-reset-clears-cooldown",
  concurrencyState,
  concurrency.event("concurrency-changed", { next: 16, previous: 2, reason: "idle_reset" }),
);
concurrencyState = concurrency.step(
  "concurrency/cooldown-zero",
  concurrencyState,
  concurrency.event("concurrency-changed", { next: 15, cooldownMs: 0 }),
);
concurrencyState = assertIgnored(
  "concurrency/zero-next-ignored",
  concurrencyState,
  concurrency.event("concurrency-changed", { next: 0 }),
);
concurrencyState = assertIgnored(
  "concurrency/fractional-next-ignored",
  concurrencyState,
  concurrency.event("concurrency-changed", { next: 1.5 }),
);
concurrencyState = assertIgnored(
  "concurrency/string-next-ignored",
  concurrencyState,
  concurrency.event("concurrency-changed", { next: "4" }),
);
concurrency.step(
  "concurrency/ceiling-monotonic",
  concurrencyState,
  concurrency.event("concurrency-changed", { next: 1, previous: 1 }),
);

// ─────────────────────────────────────────────────────────────────────────────
// K. phase-entered.
// ─────────────────────────────────────────────────────────────────────────────
const phases = makeRun("run-phases");
let phaseState = foldAll([
  phases.event("run-started", { caps: { maxConcurrency: 2 }, concurrencyCeiling: 4 }),
]);
phaseState = phases.step(
  "phase/new",
  phaseState,
  phases.event("phase-entered", { name: "intake", ordinal: 1 }),
);
phaseState = phases.step(
  "phase/rounds-take-max",
  phaseState,
  phases.event("phase-entered", { name: "intake", ordinal: 3 }),
);
phaseState = assertIgnored(
  "phase/lower-rounds-ignored",
  phaseState,
  phases.event("phase-entered", { name: "intake", ordinal: 2 }),
);
phaseState = phases.step(
  "phase/second",
  phaseState,
  phases.event("phase-entered", { name: "work" }),
);
phaseState = phases.step(
  "phase/name-truncated-128",
  phaseState,
  phases.event("phase-entered", { name: "z".repeat(200), ordinal: 1 }),
);
phaseState = assertIgnored(
  "phase/empty-name-ignored",
  phaseState,
  phases.event("phase-entered", { name: "", ordinal: 1 }),
);
phaseState = assertIgnored(
  "phase/non-string-name-ignored",
  phaseState,
  phases.event("phase-entered", { name: 42, ordinal: 1 }),
);
phases.step(
  "phase/non-integer-ordinal-defaults-1",
  phaseState,
  phases.event("phase-entered", { name: "review", ordinal: 1.5 }),
);

// ─────────────────────────────────────────────────────────────────────────────
// L. run-launched.
// ─────────────────────────────────────────────────────────────────────────────
const launched = makeRun("run-launched");
let launchedState = foldAll([
  launched.event("run-started", { caps: { maxConcurrency: 2 }, concurrencyCeiling: 4 }),
]);
launchedState = launched.step(
  "run-launched/names-and-alongside",
  launchedState,
  launched.event("run-launched", {
    inputId: "input-2",
    phaseNames: ["a", "b", "c"],
    phaseAlongside: [[2], [], [0, 1, 0, 3, -1, 1.5, "x"]],
  }),
);
launchedState = launched.step(
  "run-launched/alongside-sanitized",
  launchedState,
  launched.event("run-launched", {
    phaseNames: ["a", "b"],
    phaseAlongside: [[1, 1, 0], [0], "nope"],
  }),
);
launchedState = launched.step(
  "run-launched/alongside-absent",
  launchedState,
  launched.event("run-launched", { phaseNames: ["only"] }),
);
launchedState = launched.step(
  "run-launched/filters-invalid-names",
  launchedState,
  launched.event("run-launched", { phaseNames: ["ok", 42, "", null, "tail"] }),
);
launchedState = assertIgnored(
  "run-launched/missing-phase-names-ignored",
  launchedState,
  launched.event("run-launched", { inputId: "input-3" }),
);
launched.step(
  "run-launched/empty-names-ignored",
  launchedState,
  launched.event("run-launched", { phaseNames: [] }),
);

// ─────────────────────────────────────────────────────────────────────────────
// M. run-settled.
// ─────────────────────────────────────────────────────────────────────────────
const settle = makeRun("run-settle");
const runningBase = foldAll([
  settle.event("run-started", { caps: { maxConcurrency: 4 }, concurrencyCeiling: 8 }),
  settle.event("actor-created", { actor: { siteId: "agent#1", ordinal: 0 }, name: "worker" }),
  settle.event("node-queued", {
    instance: { siteId: "agent#1", ordinal: 0 },
    actor: { siteId: "agent#1", ordinal: 0 },
    kind: "ask",
  }),
  settle.event("node-dispatched", {
    instance: { siteId: "agent#1", ordinal: 0 },
    actor: { siteId: "agent#1", ordinal: 0 },
  }),
  settle.event("node-executing", {
    instance: { siteId: "agent#1", ordinal: 0 },
    actor: { siteId: "agent#1", ordinal: 0 },
  }),
]);
settle.step(
  "run-settled/completed-with-message",
  runningBase,
  settle.event("run-settled", {
    status: "completed",
    error: { message: "all done" },
    resumable: true,
  }),
);
settle.step(
  "run-settled/errored-without-message",
  runningBase,
  settle.event("run-settled", { status: "errored" }),
);
settle.step(
  "run-settled/stopped-user",
  runningBase,
  settle.event("run-settled", { status: "stopped", stopReason: "user" }),
);
settle.step(
  "run-settled/stopped-superseded-carries-pointer",
  runningBase,
  settle.event("run-settled", {
    status: "stopped",
    stopReason: "superseded",
    supersededBy: "run-2",
  }),
);
settle.step(
  "run-settled/superseded-without-pointer",
  runningBase,
  settle.event("run-settled", { status: "stopped", stopReason: "superseded" }),
);
settle.step(
  "run-settled/stopped-unknown-stop-reason",
  runningBase,
  settle.event("run-settled", { status: "stopped", stopReason: "exploded" }),
);
settle.step(
  "run-settled/unknown-status-becomes-errored",
  runningBase,
  settle.event("run-settled", { status: "cancelled" }),
);
settle.step(
  "run-settled/unknown-status-keeps-message",
  runningBase,
  settle.event("run-settled", { status: "cancelled", error: { message: "legacy word" } }),
);
settle.step(
  "run-settled/missing-status-becomes-errored",
  runningBase,
  settle.event("run-settled", { resumable: true }),
);
settle.step(
  "run-settled/error-not-a-record",
  runningBase,
  settle.event("run-settled", { status: "errored", error: "boom" }),
);
settle.step(
  "run-settled/error-message-truncated-2048",
  runningBase,
  settle.event("run-settled", { status: "errored", error: { message: "e".repeat(3000) } }),
);
settle.step(
  "run-settled/empty-message-kept",
  runningBase,
  settle.event("run-settled", { status: "errored", error: { message: "" } }),
);
settle.step(
  "run-settled/resumable-only-when-true",
  runningBase,
  settle.event("run-settled", { status: "completed", resumable: false }),
);
const settleEnvelope = settle.event("run-settled", { status: "completed" });
const settledOnce = settle.step("run-settled/completed-replay-setup", runningBase, settleEnvelope);
assertNull("run-settled/completed-replay-null", settledOnce, settleEnvelope);

// Settling a run that still carries pending questions and a cooldown.
const dirty = makeRun("run-settle-dirty");
const dirtyState = foldAll([
  dirty.event("run-started", { caps: { maxConcurrency: 4 }, concurrencyCeiling: 8 }),
  dirty.event("concurrency-changed", {
    next: 2,
    previous: 4,
    key: "provider/model",
    cooldownMs: 900,
    reason: "throttled",
  }),
  dirty.event("escalation-raised", { qid: "q-1", question: "Blocked?" }),
  dirty.event("escalation-raised", { qid: "q-2", question: "Also blocked?" }),
]);
const dirtySettle = dirty.event("run-settled", {
  status: "errored",
  error: { message: "gave up" },
});
const settledDirty = dirty.step(
  "run-settled/clears-questions-and-cooldown",
  dirtyState,
  dirtySettle,
);
assertNull("run-settled/clears-questions-and-cooldown-replay", settledDirty, dirtySettle);

// ─────────────────────────────────────────────────────────────────────────────
// N. Bounds: maxRuns, maxActors, maxNodes, maxReports, maxPendingQuestions, maxPhases.
// ─────────────────────────────────────────────────────────────────────────────
let runsSequence = 0;
const runEvent = (runId: string, eventType: string, payload: Payload = {}) => {
  runsSequence += 1;
  return { runId, eventType, sequence: runsSequence, payload } as WorkflowRunProgressEnvelope;
};
let runsState: WorkflowRunsState | null = null;
for (let index = 0; index < 9; index += 1) {
  runsState = foldState(
    runsState,
    runEvent(`run-bound-${index}`, "run-started", {
      caps: { maxConcurrency: 2 },
      concurrencyCeiling: 4,
    }),
  );
}
if (runsState === null || runsState.runs.length !== 8) {
  throw new Error("maxRuns eviction setup failed");
}
if (runsState.runs[0].runId !== "run-bound-1") {
  throw new Error(
    `maxRuns kept the wrong runs: ${runsState.runs.map((run) => run.runId).join(",")}`,
  );
}
pushCase(
  "bounds/max-runs-watermark-on-existing-run",
  runsState,
  runEvent("run-bound-0", "log", { line: "late" }),
);
const evicted = pushCase(
  "bounds/max-runs-evicts-oldest",
  runsState,
  runEvent("run-bound-9", "run-started", {}),
);
if (evicted === null || evicted.runs[0].runId !== "run-bound-2") {
  throw new Error("maxRuns eviction did not drop the oldest run");
}

// maxActors / maxNodes share the same bound.
const actorBound = makeRun("run-actor-bound");
let actorBoundState = foldAll([
  actorBound.event("run-started", { caps: { maxConcurrency: 2 }, concurrencyCeiling: 4 }),
]);
for (let index = 0; index < 256; index += 1) {
  actorBoundState = foldState(
    actorBoundState,
    actorBound.event("actor-created", {
      actor: { siteId: `agent#${index}`, ordinal: 0 },
      name: `actor-${index}`,
    }),
  );
}
if (actorBoundState === null || actorBoundState.runs[0].actors.length !== 256) {
  throw new Error("maxActors setup failed");
}
actorBoundState = actorBound.step(
  "bounds/max-actors-257th-rejected",
  actorBoundState,
  actorBound.event("actor-created", { actor: { siteId: "agent#overflow", ordinal: 0 } }),
);
actorBoundState = actorBound.step(
  "bounds/max-actors-existing-update-still-accepted",
  actorBoundState,
  actorBound.event("actor-created", { actor: { siteId: "agent#0", ordinal: 0 }, name: "renamed" }),
);
if (actorBoundState === null || actorBoundState.runs[0].truncated !== true) {
  throw new Error("maxActors did not mark truncated");
}

const nodeBound = makeRun("run-node-bound");
let nodeBoundState = foldAll([
  nodeBound.event("run-started", { caps: { maxConcurrency: 2 }, concurrencyCeiling: 4 }),
]);
for (let index = 0; index < 256; index += 1) {
  nodeBoundState = foldState(
    nodeBoundState,
    nodeBound.event("node-queued", {
      instance: { siteId: `node#${index}`, ordinal: 0 },
      kind: "ask",
    }),
  );
}
if (nodeBoundState === null || nodeBoundState.runs[0].nodes.length !== 256) {
  throw new Error("maxNodes setup failed");
}
nodeBoundState = nodeBound.step(
  "bounds/max-nodes-257th-rejected",
  nodeBoundState,
  nodeBound.event("node-queued", {
    instance: { siteId: "node#overflow", ordinal: 0 },
    kind: "ask",
  }),
);
nodeBound.step(
  "bounds/max-nodes-existing-update-still-accepted",
  nodeBoundState,
  nodeBound.event("node-executing", { instance: { siteId: "node#0", ordinal: 0 } }),
);

const reportBound = makeRun("run-report-bound");
let reportBoundState = foldAll([
  reportBound.event("run-started", { caps: { maxConcurrency: 2 }, concurrencyCeiling: 4 }),
]);
for (let index = 0; index < 64; index += 1) {
  reportBoundState = foldState(
    reportBoundState,
    reportBound.event("report", {
      instance: { siteId: `report#${index}`, ordinal: 0 },
      item: index,
    }),
  );
}
if (reportBoundState === null || reportBoundState.runs[0].reports?.length !== 64) {
  throw new Error("maxReports setup failed");
}
reportBoundState = reportBound.step(
  "bounds/max-reports-65th-rejected",
  reportBoundState,
  reportBound.event("report", {
    instance: { siteId: "report#overflow", ordinal: 0 },
    item: "late",
  }),
);
reportBound.step(
  "bounds/max-reports-existing-update-still-accepted",
  reportBoundState,
  reportBound.event("report", { instance: { siteId: "report#0", ordinal: 0 }, item: "edited" }),
);

const questionBound = makeRun("run-question-bound");
let questionBoundState = foldAll([
  questionBound.event("run-started", { caps: { maxConcurrency: 2 }, concurrencyCeiling: 4 }),
]);
for (let index = 0; index < 32; index += 1) {
  questionBoundState = foldState(
    questionBoundState,
    questionBound.event("escalation-raised", { qid: `q-${index}`, question: `question ${index}` }),
  );
}
if (questionBoundState === null || questionBoundState.runs[0].pendingQuestions?.length !== 32) {
  throw new Error("maxPendingQuestions setup failed");
}
questionBoundState = questionBound.step(
  "bounds/max-questions-33rd-rejected",
  questionBoundState,
  questionBound.event("escalation-raised", { qid: "q-overflow", question: "late" }),
);
questionBound.step(
  "bounds/max-questions-existing-update-still-accepted",
  questionBoundState,
  questionBound.event("escalation-raised", { qid: "q-0", question: "revised" }),
);

const phaseBound = makeRun("run-phase-bound");
let phaseBoundState = foldAll([
  phaseBound.event("run-started", { caps: { maxConcurrency: 2 }, concurrencyCeiling: 4 }),
]);
for (let index = 0; index < 32; index += 1) {
  phaseBoundState = foldState(
    phaseBoundState,
    phaseBound.event("phase-entered", { name: `phase-${index}`, ordinal: 1 }),
  );
}
if (phaseBoundState === null || phaseBoundState.runs[0].phases?.length !== 32) {
  throw new Error("maxPhases setup failed");
}
phaseBoundState = phaseBound.step(
  "bounds/max-phases-33rd-rejected",
  phaseBoundState,
  phaseBound.event("phase-entered", { name: "phase-overflow", ordinal: 1 }),
);
if (phaseBoundState === null || phaseBoundState.runs[0].currentPhase !== "phase-overflow") {
  throw new Error("currentPhase must follow the control flow even past the bound");
}
phaseBound.step(
  "bounds/max-phases-existing-update-still-accepted",
  phaseBoundState,
  phaseBound.event("phase-entered", { name: "phase-0", ordinal: 2 }),
);

// ─────────────────────────────────────────────────────────────────────────────
// O. Envelope validity and unknown events.
// ─────────────────────────────────────────────────────────────────────────────
const envelopeRun = makeRun("run-envelope");
const envelopeState = foldAll([
  envelopeRun.event("run-started", { caps: { maxConcurrency: 2 }, concurrencyCeiling: 4 }),
]);
assertNull("envelope/missing-run-id", envelopeState, {
  eventType: "run-started",
  sequence: 99,
  payload: {},
});
assertNull("envelope/empty-run-id", envelopeState, {
  runId: "",
  eventType: "run-started",
  sequence: 99,
  payload: {},
});
assertNull("envelope/missing-event-type", envelopeState, {
  runId: "run-envelope",
  sequence: 99,
  payload: {},
});
assertNull("envelope/non-string-event-type", envelopeState, {
  runId: "run-envelope",
  eventType: 42 as unknown as string,
  sequence: 99,
  payload: {},
});
const ignoredUnknown = assertIgnored(
  "envelope/unknown-event-type-only-watermark",
  envelopeState,
  envelopeRun.event("brand-new-event", { whatever: true }),
);
assertIgnored("envelope/missing-payload", ignoredUnknown, {
  runId: "run-envelope",
  eventType: "log",
  sequence: 100,
});
pushCase("envelope/array-payload", envelopeState, {
  runId: "run-envelope",
  eventType: "log",
  sequence: 101,
  payload: [1, 2, 3] as unknown as Payload,
});
pushCase("envelope/missing-sequence", envelopeState, {
  runId: "run-envelope",
  eventType: "log",
  payload: {},
});
const freshLog = pushCase("envelope/unknown-run-creates-pending", null, {
  runId: "run-fresh",
  eventType: "log",
  sequence: 1,
  payload: { line: "hello" },
});
if (freshLog === null || freshLog.runs[0].status !== "pending") {
  throw new Error("an unknown run must be materialized as pending");
}
pushCase("envelope/new-run-usage", null, {
  runId: "run-fresh-2",
  eventType: "usage-updated",
  sequence: 1,
  payload: { spentTokens: 5 },
});
// Same runId started twice with the identical envelope: the second is a verbatim replay.
const startedReplay = makeRun("run-started-replay");
const startedEnvelope = startedReplay.event("run-started", {
  caps: { maxConcurrency: 3 },
  concurrencyCeiling: 6,
});
const startedState = foldState(null, startedEnvelope);
assertNull("run-started/verbatim-replay-null", startedState, startedEnvelope);

// ─────────────────────────────────────────────────────────────────────────────
// P. A deterministic randomized walk: catches ordering/last-write-wins drift.
// ─────────────────────────────────────────────────────────────────────────────
const random = mulberry32(0x5eed);
const walk = makeRun("run-walk");
let walkState: WorkflowRunsState | null = null;
const walkSites = ["agent#1", "agent#2", "world#1"];
const walkPhases = [
  "node-queued",
  "node-dispatched",
  "node-executing",
  "node-waiting",
  "node-settled",
];
for (let tick = 0; tick < 60; tick += 1) {
  const site = walkSites[Math.floor(random() * walkSites.length)]!;
  const ordinal = Math.floor(random() * 2);
  const ref = { siteId: site, ordinal };
  const roll = random();
  let envelope: WorkflowRunProgressEnvelope;
  if (tick === 0) {
    envelope = walk.event("run-started", { caps: { maxConcurrency: 4 }, concurrencyCeiling: 8 });
  } else if (roll < 0.15) {
    envelope = walk.event("actor-created", { actor: ref, name: `actor-${tick}` });
  } else if (roll < 0.4) {
    envelope = walk.event(walkPhases[Math.floor(random() * walkPhases.length)]!, {
      instance: ref,
      actor: ref,
      kind: "ask",
      outcome: "ok",
    });
  } else if (roll < 0.55) {
    envelope = walk.event("node-progress", {
      instance: ref,
      turn: 1 + Math.floor(random() * 3),
      toolCalls: Math.floor(random() * 10),
    });
  } else if (roll < 0.7) {
    envelope = walk.event("report", { instance: ref, item: { tick } });
  } else if (roll < 0.8) {
    envelope = walk.event("phase-entered", { name: `phase-${Math.floor(random() * 4)}` });
  } else if (roll < 0.88) {
    envelope = walk.event("usage-updated", { spentTokens: tick * 100 });
  } else if (roll < 0.96) {
    envelope = walk.event("concurrency-changed", { next: 1 + Math.floor(random() * 8) });
  } else {
    envelope = walk.event("artifact-published", {
      artifact: { id: `art-${tick % 3}`, kind: "chart", version: 1 + (tick % 5) },
    });
  }
  walkState = walk.step(
    `walk/${String(tick).padStart(2, "0")}-${envelope.eventType}`,
    walkState,
    envelope,
  );
}
walk.step(
  "walk/settle",
  walkState,
  walk.event("run-settled", { status: "completed", resumable: true }),
);

// ─────────────────────────────────────────────────────────────────────────────
// Q. Edge cases the linear corpus does not reach.
// ─────────────────────────────────────────────────────────────────────────────
const edges = makeRun("run-edges");
let edgeState = foldAll([
  edges.event("run-started", { caps: { maxConcurrency: 2 }, concurrencyCeiling: 4 }),
]);
edgeState = edges.step(
  "edge/actor-session-empty-ignored",
  edgeState,
  edges.event("actor-created", { actor: { siteId: "a1", ordinal: 0 } }, { actorSessionId: "" }),
);
edgeState = edges.step(
  "edge/node-progress-before-cached-flag",
  edgeState,
  edges.event("node-queued", {
    instance: { siteId: "n1", ordinal: 0 },
    kind: "ask",
    instructionsHead: "head",
  }),
);
edgeState = edges.step(
  "edge/node-readings",
  edgeState,
  edges.event("node-progress", {
    instance: { siteId: "n1", ordinal: 0 },
    turn: 3,
    toolCalls: 9,
    lastTool: { name: "Bash", target: "pnpm test" },
  }),
);
edgeState = edges.step(
  "edge/cached-flag-on-dispatched-keeps-progress",
  edgeState,
  edges.event("node-dispatched", { instance: { siteId: "n1", ordinal: 0 }, cached: true }),
);
edgeState = edges.step(
  "edge/cached-settle-drops-counters-keeps-head",
  edgeState,
  edges.event("node-settled", {
    instance: { siteId: "n1", ordinal: 0 },
    cached: true,
    outcome: "ok",
  }),
);
edgeState = edges.step(
  "edge/float-ordinal-queued",
  edgeState,
  edges.event("node-queued", { instance: { siteId: "n2", ordinal: 1.5 }, kind: "ask" }),
);
edgeState = edges.step(
  "edge/float-ordinal-progress",
  edgeState,
  edges.event("node-progress", { instance: { siteId: "n2", ordinal: 1.5 }, turn: 1 }),
);
edgeState = edges.step(
  "edge/report-empty-artifact-id-untagged",
  edgeState,
  edges.event("report", {
    instance: { siteId: "r1", ordinal: 0 },
    item: "x",
    artifactId: "",
  }),
);
edgeState = edges.step(
  "edge/escalation-asked-at-string-ignored",
  edgeState,
  edges.event("escalation-raised", { qid: "q-a", question: "q", askedAt: "now" }),
);
edgeState = edges.step(
  "edge/escalation-asked-at-fraction-kept",
  edgeState,
  edges.event("escalation-raised", { qid: "q-b", question: "q", askedAt: 1.5 }),
);
edgeState = edges.step(
  "edge/last-tool-empty-name-ignored",
  edgeState,
  edges.event("node-progress", {
    instance: { siteId: "n2", ordinal: 1.5 },
    lastTool: { name: "" },
  }),
);
edgeState = edges.step(
  "edge/artifact-negative-bytes-absent",
  edgeState,
  edges.event("artifact-published", {
    artifact: { id: "f1", kind: "file", version: 1, bytes: -1 },
  }),
);
edgeState = edges.step(
  "edge/artifact-fractional-bytes-absent",
  edgeState,
  edges.event("artifact-published", {
    artifact: { id: "f2", kind: "file", version: 1, bytes: 1.5 },
  }),
);
edges.step(
  "edge/usage-fraction-kept",
  edgeState,
  edges.event("usage-updated", { spentTokens: 1.5 }),
);

// Out-of-order events still apply, but never pull the watermark back.
const reordered = makeRun("run-reordered");
const reorderedState = foldAll([
  reordered.event("run-started", { caps: { maxConcurrency: 2 }, concurrencyCeiling: 4 }),
  reordered.event("usage-updated", { spentTokens: 10 }),
  reordered.event("usage-updated", { spentTokens: 20 }),
]);
pushCase("edge/late-event-keeps-watermark", reorderedState, {
  runId: "run-reordered",
  eventType: "usage-updated",
  sequence: 1,
  payload: { spentTokens: 5 },
});

// run-launched caps at 32 declared phases.
const manyNames = makeRun("run-many-names");
const manyNamesState = foldAll([manyNames.event("run-started", {})]);
manyNames.step(
  "edge/run-launched-32-names",
  manyNamesState,
  manyNames.event("run-launched", {
    phaseNames: Array.from({ length: 40 }, (_, index) => `p${index}`),
  }),
);

// run-started reads the limit only from a record-shaped caps and an integer ceiling.
const capsEdge = makeRun("run-caps-edge");
const capsState = foldAll([capsEdge.event("run-started", { concurrencyCeiling: 8 })]);
capsEdge.step(
  "edge/run-started-caps-array-limit-ignored",
  capsState,
  capsEdge.event("run-started", { concurrencyCeiling: 8, caps: [1, 2] }),
);
capsEdge.step(
  "edge/run-started-fractional-ceiling-ignored",
  capsState,
  capsEdge.event("run-started", { concurrencyCeiling: 8.5 }),
);

// A run unknown to the state is materialized as pending even by non-lifecycle events.
pushCase("edge/unknown-run-node-progress-pending", null, {
  runId: "run-ghost",
  eventType: "node-progress",
  sequence: 1,
  payload: { instance: { siteId: "x", ordinal: 0 }, turn: 1 },
});
pushCase("edge/unknown-run-artifact-failed-pending", null, {
  runId: "run-ghost-2",
  eventType: "artifact-failed",
  sequence: 1,
  payload: {},
});

// Updating an older run keeps the run order intact.
let twoRuns: WorkflowRunsState | null = foldState(null, runEvent("run-two-a", "run-started", {}));
twoRuns = foldState(twoRuns, runEvent("run-two-b", "run-started", {}));
pushCase(
  "edge/second-run-updated-in-place",
  twoRuns,
  runEvent("run-two-a", "usage-updated", { spentTokens: 9 }),
);

// Boundary cases: exactly-at-bound vs one-over, for every display bound.
const bounds = makeRun("run-bounds");
let boundsState = foldAll([
  bounds.event("run-started", { caps: { maxConcurrency: 2 }, concurrencyCeiling: 4 }),
]);
boundsState = bounds.step(
  "bounds/actor-name-exactly-128",
  boundsState,
  bounds.event("actor-created", {
    actor: { siteId: "agent#a", ordinal: 0 },
    name: "n".repeat(128),
  }),
);
boundsState = bounds.step(
  "bounds/actor-name-129-truncated",
  boundsState,
  bounds.event("actor-created", {
    actor: { siteId: "agent#b", ordinal: 0 },
    name: "n".repeat(129),
  }),
);
boundsState = bounds.step(
  "bounds/report-preview-exactly-2048",
  boundsState,
  bounds.event("report", {
    instance: { siteId: "r#a", ordinal: 0 },
    item: "y".repeat(2048),
  }),
);
boundsState = bounds.step(
  "bounds/report-preview-2049-truncated",
  boundsState,
  bounds.event("report", {
    instance: { siteId: "r#b", ordinal: 0 },
    item: "y".repeat(2049),
  }),
);
boundsState = bounds.step(
  "bounds/question-exactly-2048",
  boundsState,
  bounds.event("escalation-raised", { qid: "q-exact", question: "q".repeat(2048) }),
);
boundsState = bounds.step(
  "bounds/question-2049-truncated",
  boundsState,
  bounds.event("escalation-raised", { qid: "q-over", question: "q".repeat(2049) }),
);
boundsState = bounds.step(
  "bounds/artifact-id-exactly-64",
  boundsState,
  bounds.event("artifact-published", {
    artifact: { id: "i".repeat(64), kind: "file", version: 1 },
  }),
);
boundsState = bounds.step(
  "bounds/artifact-id-65-truncated",
  boundsState,
  bounds.event("artifact-published", {
    artifact: { id: "j".repeat(65), kind: "file", version: 1 },
  }),
);
boundsState = bounds.step(
  "bounds/instructions-head-exactly-240",
  boundsState,
  bounds.event("node-queued", {
    instance: { siteId: "n#a", ordinal: 0 },
    kind: "ask",
    instructionsHead: "h".repeat(240),
  }),
);
bounds.step(
  "bounds/instructions-head-241-truncated",
  boundsState,
  bounds.event("node-queued", {
    instance: { siteId: "n#b", ordinal: 0 },
    kind: "ask",
    instructionsHead: "h".repeat(241),
  }),
);

// Non-ASCII payloads survive the report preview serializer byte for byte.
const unicodeRun = makeRun("run-unicode");
let unicodeState = foldAll([
  unicodeRun.event("run-started", { caps: { maxConcurrency: 2 }, concurrencyCeiling: 4 }),
]);
unicodeState = unicodeRun.step(
  "unicode/report-string",
  unicodeState,
  unicodeRun.event("report", {
    instance: { siteId: "r#u1", ordinal: 0 },
    item: "Check completed ✅ — done",
  }),
);
unicodeState = unicodeRun.step(
  "unicode/report-object",
  unicodeState,
  unicodeRun.event("report", {
    instance: { siteId: "r#u2", ordinal: 0 },
    item: { title: "Check", 'quo"te': "back\\slash\nnewline", emoji: "🚀" },
  }),
);
unicodeState = unicodeRun.step(
  "unicode/report-array",
  unicodeState,
  unicodeRun.event("report", {
    instance: { siteId: "r#u3", ordinal: 0 },
    item: [1, "two", null, { three: 3 }],
  }),
);
unicodeRun.step(
  "unicode/escalation",
  unicodeState,
  unicodeRun.event("escalation-raised", {
    qid: "q-ü",
    question: "Which file should win?",
    context: "context",
  }),
);

//─────────────────────────────────────────────────────────────────────
// Coverage guard: every reducer branch must have at least one case.
//─────────────────────────────────────────────────────────────────────
const REQUIRED_EVENT_TYPES = [
  "run-started",
  "actor-created",
  "node-queued",
  "node-dispatched",
  "node-executing",
  "node-waiting",
  "node-repairing",
  "node-nudged",
  "node-settled",
  "node-progress",
  "report",
  "artifact-published",
  "artifact-failed",
  "usage-updated",
  "escalation-raised",
  "escalation-resolved",
  "concurrency-changed",
  "phase-entered",
  "run-launched",
  "run-settled",
  "log",
  "compaction",
  "<missing>",
];const missing = REQUIRED_EVENT_TYPES.filter((eventType) => !coveredEventTypes.has(eventType));
if (missing.length > 0) {
  throw new Error(`fixtures miss reducer branches: ${missing.join(", ")}`);
}
const uniqueNames = new Set(cases.map((entry) => entry.name));
if (uniqueNames.size !== cases.length) {
  throw new Error("fixture case names must be unique");
}

const root = resolve(import.meta.dirname, "..", "..");
const out = join(root, "native/crates/nc-code-host/tests/fixtures/workflow_runs_fixtures.json");
mkdirSync(dirname(out), { recursive: true });
writeFileSync(out, `${JSON.stringify(cases, null, 2)}\n`);

console.log(
  JSON.stringify(
    {
      out,
      cases: cases.length,
      eventTypes: [...coveredEventTypes].sort(),
      noChangeCases: cases.filter((entry) => entry.expected === null).length,
    },
    null,
    1,
  ),
);
