// Generates the Rust `bound_causality_graph` fixtures from the REAL TypeScript
// projection (`boundCausalityGraph`) over analysis JSON produced by the REAL analyzer.
//
// Usage (from the repo root):
//   pnpm exec tsx scripts/bench/gen-workflow-graph-fixtures.mts
//
// Writes `native/crates/nc-code-host/tests/fixtures/workflow_graph_fixtures.json` as
//   [{ name, analysis, expected }]
// where `analysis` is `{ diagnostics, ok, causality?, flow?, handoff? }` (the compile
// kernel's analyze blob, minus the non-graph `core` / `graph` payloads) and `expected` is
// the untouched output of `boundCausalityGraph` (JSON-normalized), or `null` when
// `analysis.causality` is absent — exactly `boundGraphOfAnalysis` returning undefined.
//
// The corpus is mostly real scripts: the two repo fixtures (linear / fanout), phase and
// loop shapes, participants/hand-offs (member, many, back, types), strands with
// `alongside`, may-set lanes with `source`, zero-phase and diagnostics scripts, UTF-16
// boundary names, and graphs exceeding the step / lane / phase limits. A few
// `synthetic/*` cases start from a real analysis and stretch one structure past a bound
// (participants, hand-offs, post-fold phase edges) or pollute references (unknown phases,
// self/duplicate alongside, over-long phase ids); their expectations still come from the
// real TS function, never from hand-written JSON.

import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import {
  analyzeWorkflowScript,
  type CausalityGraph,
  type ControlFlowGraph,
  type HandoffGraph,
} from "../../apps/zcode-cli/packages/dynamic-workflow/dist/index.js";
import { boundCausalityGraph } from "../../apps/zcode-cli/packages/core/src/tool/handlers/create-workflow-graph-bounds.js";

const REPO_ROOT = fileURLToPath(new URL("../..", import.meta.url));
const OUTPUT = resolve(
  REPO_ROOT,
  "native/crates/nc-code-host/tests/fixtures/workflow_graph_fixtures.json",
);

const MAX_STEPS = 64;
const MAX_LANES = 32;
const MAX_PARTICIPANTS = 64;
const MAX_HANDOFFS = 256;
const MAX_PHASES = 32;

interface AnalysisJson {
  diagnostics: unknown[];
  ok: boolean;
  causality?: CausalityGraph;
  flow?: ControlFlowGraph;
  handoff?: HandoffGraph;
}

interface FixtureCase {
  name: string;
  analysis: AnalysisJson;
  expected: unknown;
}

const cases: FixtureCase[] = [];

/** JSON round-trip: drops `undefined`, resolves class instances, and mirrors the wire blob. */
function json<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}

function analysisOfScript(script: string): AnalysisJson {
  const result = analyzeWorkflowScript(script);
  return json({
    diagnostics: result.diagnostics,
    ok: result.ok,
    ...(result.causality === undefined ? {} : { causality: result.causality }),
    ...(result.flow === undefined ? {} : { flow: result.flow }),
    ...(result.handoff === undefined ? {} : { handoff: result.handoff }),
  });
}

/**
 * The wrapper rule from `boundGraphOfAnalysis`: absent causality means no graph at all
 * (compile failed before the causal projection). Otherwise run the real projection.
 */
function expectedFor(analysis: AnalysisJson): unknown {
  if (analysis.causality === undefined) return null;
  return json(boundCausalityGraph(analysis.causality, analysis.flow, analysis.handoff));
}

function addCase(name: string, script: string): AnalysisJson {
  const analysis = analysisOfScript(script);
  cases.push({ name, analysis, expected: expectedFor(analysis) });
  return analysis;
}

function addAnalysisCase(name: string, analysis: AnalysisJson): void {
  cases.push({ name, analysis, expected: expectedFor(analysis) });
}

function requireCausality(analysis: AnalysisJson, name: string): CausalityGraph {
  if (analysis.causality === undefined) {
    throw new Error(`${name}: expected a causality graph from the analyzer`);
  }
  return analysis.causality;
}

function requireFlow(analysis: AnalysisJson, name: string): ControlFlowGraph {
  if (analysis.flow === undefined) {
    throw new Error(`${name}: expected a control-flow graph from the analyzer`);
  }
  return analysis.flow;
}

function requireHandoff(analysis: AnalysisJson, name: string): HandoffGraph {
  if (analysis.handoff === undefined) {
    throw new Error(`${name}: expected a hand-off graph from the analyzer`);
  }
  return analysis.handoff;
}

function readFixture(relative: string): string {
  return readFileSync(resolve(REPO_ROOT, relative), "utf8");
}

/** Deterministic PRNG for the synthetic phase-quotient batch (no crypto, no Math.random). */
function mulberry32(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

import { addRealCases } from "./workflow-graph-fixtures-cases.mjs";
import { addSyntheticCases } from "./workflow-graph-fixtures-synthetic.mjs";

addRealCases({ MAX_STEPS, MAX_LANES, MAX_PHASES, addCase, readFixture });

addSyntheticCases({
  MAX_PARTICIPANTS,
  MAX_HANDOFFS,
  MAX_PHASES,
  json,
  analysisOfScript,
  requireCausality,
  requireFlow,
  requireHandoff,
  addAnalysisCase,
  mulberry32,
  readFixture,
});


// ─────────────────────────────────────────────────────────────────────────────
// Corpus guardrails: fail the generator if a required shape silently disappears.
// ─────────────────────────────────────────────────────────────────────────────

function hasCase(predicate: (expected: Record<string, unknown>) => boolean): boolean {
  return cases.some(
    (item) =>
      item.expected !== null &&
      predicate(item.expected as Record<string, unknown>),
  );
}

if (!cases.some((item) => item.expected === null)) {
  throw new Error("corpus must contain a causality-absent (null) case");
}
if (!hasCase((graph) => graph.truncated === true)) {
  throw new Error("corpus must contain a truncated graph");
}
if (!hasCase((graph) => Array.isArray(graph.phases) && graph.phases.length > 0)) {
  throw new Error("corpus must contain a phase vocabulary");
}
if (!hasCase((graph) => Array.isArray(graph.sink))) {
  throw new Error("corpus must contain a sink list");
}
if (!hasCase((graph) => Array.isArray(graph.exits) && graph.exits.length > 0)) {
  throw new Error("corpus must contain phase exits");
}
if (
  !hasCase((graph) =>
    (graph.phaseEdges as Array<Record<string, unknown>> | undefined)?.some(
      (edge) => edge.back === true,
    ),
  )
) {
  throw new Error("corpus must contain a back phase edge");
}
if (
  !hasCase((graph) =>
    (graph.phases as Array<Record<string, unknown>> | undefined)?.some(
      (phase) => Array.isArray(phase.alongside) && phase.alongside.length > 0,
    ),
  )
) {
  throw new Error("corpus must contain an alongside list");
}
if (
  !hasCase((graph) =>
    (graph.participants as Array<Record<string, unknown>> | undefined)?.some(
      (participant) => participant.member !== undefined,
    ),
  )
) {
  throw new Error("corpus must contain member cards");
}
if (
  !hasCase((graph) =>
    (graph.participants as Array<Record<string, unknown>> | undefined)?.some(
      (participant) => participant.many === true,
    ),
  )
) {
  throw new Error("corpus must contain a many card");
}
if (
  !hasCase((graph) =>
    (graph.handoffs as Array<Record<string, unknown>> | undefined)?.some(
      (handoff) => handoff.back === true,
    ),
  )
) {
  throw new Error("corpus must contain a back hand-off");
}
if (
  !hasCase((graph) =>
    (graph.handoffs as Array<Record<string, unknown>> | undefined)?.some(
      (handoff) => Array.isArray(handoff.types),
    ),
  )
) {
  throw new Error("corpus must contain a typed hand-off");
}
if (
  !hasCase((graph) =>
    (graph.lanes as Array<Record<string, unknown>> | undefined)?.some(
      (lane) => lane.namePattern !== undefined,
    ),
  )
) {
  throw new Error("corpus must contain a lane namePattern");
}
if (
  !hasCase((graph) =>
    (graph.steps as Array<Record<string, unknown>> | undefined)?.some(
      (step) => step.labelPattern !== undefined,
    ),
  )
) {
  throw new Error("corpus must contain a step labelPattern");
}
if (
  !hasCase((graph) =>
    (graph.steps as Array<Record<string, unknown>> | undefined)?.some(
      (step) => Array.isArray(step.lanes) && step.source !== undefined,
    ),
  )
) {
  throw new Error("corpus must contain a may-set step with source");
}

mkdirSync(dirname(OUTPUT), { recursive: true });
writeFileSync(OUTPUT, `${JSON.stringify(cases, null, 2)}\n`);

const realCases = cases.filter((item) => !item.name.startsWith("synthetic/")).length;
const nullCases = cases.filter((item) => item.expected === null).length;
const truncatedCases = cases.filter(
  (item) =>
    item.expected !== null &&
    (item.expected as { truncated?: boolean }).truncated === true,
).length;
console.log(
  `wrote ${cases.length} cases (${realCases} scripts, ${nullCases} null, ` +
    `${truncatedCases} truncated) to ${OUTPUT}`,
);
