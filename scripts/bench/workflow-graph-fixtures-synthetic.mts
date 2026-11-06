// Synthetic half of the bound_causality_graph fixture corpus: each case starts from a
// real analysis produced by the REAL analyzer and stretches one structure past a bound
// (or pollutes references). Expectations still come from the real TS function — this
// module never hand-writes JSON. Split out of gen-workflow-graph-fixtures.mts to keep
// every linted file under the max-lines bound.

type Json = Record<string, any>;

export interface SyntheticCasesApi {
  MAX_PARTICIPANTS: number;
  MAX_HANDOFFS: number;
  MAX_PHASES: number;
  json: <T>(value: T) => T;
  analysisOfScript: (script: string) => Json;
  requireCausality: (analysis: Json, name: string) => Json;
  requireFlow: (analysis: Json, name: string) => Json;
  requireHandoff: (analysis: Json, name: string) => Json;
  addAnalysisCase: (name: string, analysis: Json) => void;
  mulberry32: (seed: number) => () => number;
  readFixture: (relative: string) => string;
}

export function addSyntheticCases(api: SyntheticCasesApi): void {
  // ─────────────────────────────────────────────────────────────────────────────
  // E. Synthetic stretches of real analysis JSON (oracle expectations are still the
  //    real TS function's output).
  // ─────────────────────────────────────────────────────────────────────────────

  // E1. More participant cards than the contract keeps. Every card shares one surviving
  // step, so only the participant bound fires.
  {
    const analysis = api.json(
      api.analysisOfScript(`
    const a = agent("solo", "S");
    const r = await a.ask("q");
    return r;
  `),
    );
    analysis.handoff = {
      participants: Array.from({ length: api.MAX_PARTICIPANTS + 6 }, (_, index) => ({
        id: `unphased:actor#1[${index}]`,
        phase: "unphased",
        lane: "actor#1",
        steps: ["ask#1"],
      })),
      handoffs: [],
    };
    api.addAnalysisCase("synthetic/participants-over-limit", analysis);
  }

  // E2. More hand-offs than the contract keeps; the first edge also carries more types
  // than `api.MAX_HANDOFF_TYPES`, one of them empty after bounding.
  {
    const analysis = api.json(
      api.analysisOfScript(`
    const a = agent("a", "A");
    const b = agent("b", "B");
    await a.ask("first");
    await b.ask("second");
    return 1;
  `),
    );
    const handoff = api.requireHandoff(analysis, "synthetic/handoffs-over-limit");
    if (handoff.participants.length !== 2) {
      throw new Error("synthetic/handoffs-over-limit: expected two participants");
    }
    handoff.handoffs = Array.from({ length: api.MAX_HANDOFFS + 44 }, (_, index) => ({
      from: "unphased:actor#1",
      to: "unphased:actor#2",
      ...(index === 0 ? { types: ["", "t1", "t2", "t3", "t4", "t5", "t6", "t7", "t8", "t9"] } : {}),
    }));
    api.addAnalysisCase("synthetic/handoffs-over-limit", analysis);
  }

  // E3. Phase edges over the bound only AFTER folding: 200 raw a→b edges (next + loop)
  // collapse to one, so the vocabulary survives.
  {
    const analysis = api.json(
      api.analysisOfScript(`
    phase("one");
    const a = agent("a", "A");
    await a.ask("q");
    return 1;
  `),
    );
    const flow = api.requireFlow(analysis, "synthetic/phase-edge-collapse");
    flow.phases = [
      { id: "phase#1", name: "one" },
      { id: "phase#2", name: "two" },
    ];
    flow.phaseEdges = [
      { from: "entry", to: "phase#1", kind: "next" },
      ...Array.from({ length: 100 }, () => ({
        from: "phase#1" as const,
        to: "phase#2" as const,
        kind: "next" as const,
      })),
      ...Array.from({ length: 100 }, () => ({
        from: "phase#1" as const,
        to: "phase#2" as const,
        kind: "loop" as const,
      })),
    ];
    api.addAnalysisCase("synthetic/phase-edge-collapse", analysis);
  }

  // E4. 32 phases and a complete 16×16 bipartite forward edge set: the transitive
  // reduction keeps all 256 (no two-hop witness exists), exceeding MAX_PHASE_EDGES
  // after folding and dropping the whole vocabulary.
  {
    const analysis = api.json(
      api.analysisOfScript(
        api.readFixture("native/crates/nc-code-workflow/tests/fixtures/workflow_linear.ts"),
      ),
    );
    const phaseIds = Array.from({ length: api.MAX_PHASES }, (_, index) => `phase#${index + 1}`);
    const flow = api.requireFlow(analysis, "synthetic/phase-edges-over-limit");
    flow.phases = phaseIds.map((id, index) => ({
      id,
      name: `phase ${index + 1}`,
      loc: { line: index + 1, column: 1 },
    }));
    flow.phaseEdges = [
      { from: "entry", to: "phase#1", kind: "next" },
      { from: "phase#32", to: "sink", kind: "jump" },
      { from: "phase#1", to: "abort", kind: "throw" },
    ];
    for (let left = 1; left <= 16; left += 1) {
      for (let right = 17; right <= 32; right += 1) {
        flow.phaseEdges.push({
          from: `phase#${left}`,
          to: `phase#${right}`,
          kind: "next",
        });
      }
    }
    const causality = api.requireCausality(analysis, "synthetic/phase-edges-over-limit");
    for (const step of causality.steps) step.phase = "phase#1";
    const handoff = api.requireHandoff(analysis, "synthetic/phase-edges-over-limit");
    for (const participant of handoff.participants) participant.phase = "phase#1";
    api.addAnalysisCase("synthetic/phase-edges-over-limit", analysis);
  }

  // E5. A visible fold: duplicate ordered pairs merge (a→b back disappears), entry /
  // abort / unknown / self edges drop, sink becomes an exit, and a forward SCC survives.
  {
    const analysis = api.json(
      api.analysisOfScript(
        api.readFixture("native/crates/nc-code-workflow/tests/fixtures/workflow_linear.ts"),
      ),
    );
    const flow = api.requireFlow(analysis, "synthetic/phase-edge-fold");
    flow.phases = [
      { id: "a", name: "A" },
      { id: "b", name: "B" },
      { id: "c", name: "C" },
    ];
    flow.phaseEdges = [
      { from: "entry", to: "a", kind: "next" },
      { from: "a", to: "b", kind: "next" },
      { from: "a", to: "b", kind: "loop" },
      { from: "b", to: "a", kind: "next" },
      { from: "b", to: "c", kind: "next" },
      { from: "a", to: "c", kind: "next" },
      { from: "a", to: "sink", kind: "jump" },
      { from: "b", to: "abort", kind: "throw" },
      { from: "a", to: "a", kind: "next" },
      { from: "a", to: "missing", kind: "next" },
    ];
    const causality = api.requireCausality(analysis, "synthetic/phase-edge-fold");
    for (const step of causality.steps) step.phase = "a";
    for (const participant of api.requireHandoff(analysis, "synthetic/phase-edge-fold")
      .participants) {
      participant.phase = "a";
    }
    api.addAnalysisCase("synthetic/phase-edge-fold", analysis);
  }

  // E6. `alongside` sanitising: self, unknown ids and duplicates drop; an empty phase
  // name is omitted.
  {
    const analysis = api.json(
      api.analysisOfScript(
        api.readFixture("native/crates/nc-code-workflow/tests/fixtures/workflow_linear.ts"),
      ),
    );
    const flow = api.requireFlow(analysis, "synthetic/alongside-sanitized");
    if (flow.phases === undefined || flow.phases.length !== 2) {
      throw new Error("synthetic/alongside-sanitized: expected two phases");
    }
    flow.phases[0].name = "";
    flow.phases[0].alongside = ["phase#2", "phase#2", "phase#1", "missing"];
    flow.phases[1].alongside = ["phase#1", "phase#1", "phase#2", "missing"];
    api.addAnalysisCase("synthetic/alongside-sanitized", analysis);
  }

  // E7. A step and a card name a phase the vocabulary does not list: the step's `phase`
  // is stripped and the card falls back to `unphased`; the other card keeps its phase.
  {
    const analysis = api.json(
      api.analysisOfScript(
        api.readFixture("native/crates/nc-code-workflow/tests/fixtures/workflow_linear.ts"),
      ),
    );
    const causality = api.requireCausality(analysis, "synthetic/unknown-phases");
    causality.steps[0].phase = "phase#404";
    const handoff = api.requireHandoff(analysis, "synthetic/unknown-phases");
    handoff.participants[0].phase = "phase#404";
    api.addAnalysisCase("synthetic/unknown-phases", analysis);
  }

  // E8. Phase ids longer than MAX_ID_CHARS: ids are bounded on the way out, but card
  // phases are compared against the RAW vocabulary ids before bounding, so the card
  // falls back to `unphased` while the step keeps its (bounded) phase.
  {
    const analysis = api.json(
      api.analysisOfScript(
        api.readFixture("native/crates/nc-code-workflow/tests/fixtures/workflow_linear.ts"),
      ),
    );
    const longOne = `phase#${"x".repeat(70)}1`;
    const longTwo = `phase#${"x".repeat(70)}2`;
    const flow = api.requireFlow(analysis, "synthetic/truncated-phase-ids");
    if (flow.phases === undefined || flow.phaseEdges === undefined) {
      throw new Error("synthetic/truncated-phase-ids: expected a phase vocabulary");
    }
    flow.phases[0].id = longOne;
    flow.phases[1].id = longTwo;
    flow.phaseEdges = [
      { from: "entry", to: longOne, kind: "next" },
      { from: longOne, to: longTwo, kind: "next" },
      { from: longTwo, to: "sink", kind: "jump" },
    ];
    const causality = api.requireCausality(analysis, "synthetic/truncated-phase-ids");
    causality.steps[0].phase = longOne;
    causality.steps[1].phase = longTwo;
    const handoff = api.requireHandoff(analysis, "synthetic/truncated-phase-ids");
    handoff.participants[0].phase = longOne;
    handoff.participants[1].phase = longTwo;
    api.addAnalysisCase("synthetic/truncated-phase-ids", analysis);
  }

  // E9. Causality without the control-flow / hand-off projections: no phases, no cards.
  {
    const analysis = api.json(
      api.analysisOfScript(
        api.readFixture("native/crates/nc-code-workflow/tests/fixtures/workflow_linear.ts"),
      ),
    );
    delete analysis.flow;
    delete analysis.handoff;
    api.addAnalysisCase("synthetic/without-flow", analysis);
  }

  // E10. Typed transitive reduction on the condensed phase DAG: a→c is implied by
  // a→b→c and is deleted before the output bound is judged.
  {
    const analysis = api.json(
      api.analysisOfScript(
        api.readFixture("native/crates/nc-code-workflow/tests/fixtures/workflow_linear.ts"),
      ),
    );
    const flow = api.requireFlow(analysis, "synthetic/phase-edge-transitive");
    flow.phases = [
      { id: "a", name: "A" },
      { id: "b", name: "B" },
      { id: "c", name: "C" },
    ];
    flow.phaseEdges = [
      { from: "a", to: "b", kind: "next" },
      { from: "b", to: "c", kind: "next" },
      { from: "a", to: "c", kind: "next" },
    ];
    api.addAnalysisCase("synthetic/phase-edge-transitive", analysis);
  }

  // E11. Carry minimisation: a→c (loop) is implied by a→b (loop) + b→c (next) and is
  // deleted; a→b survives because no forward* → carry → forward* path replaces it.
  {
    const analysis = api.json(
      api.analysisOfScript(
        api.readFixture("native/crates/nc-code-workflow/tests/fixtures/workflow_linear.ts"),
      ),
    );
    const flow = api.requireFlow(analysis, "synthetic/phase-edge-carry-witness");
    flow.phases = [
      { id: "a", name: "A" },
      { id: "b", name: "B" },
      { id: "c", name: "C" },
    ];
    flow.phaseEdges = [
      { from: "a", to: "b", kind: "loop" },
      { from: "b", to: "c", kind: "next" },
      { from: "a", to: "c", kind: "loop" },
    ];
    api.addAnalysisCase("synthetic/phase-edge-carry-witness", analysis);
  }

  // E12. Deterministic random phase quotients over one real analysis shape: exercise the
  // fold (self loops, duplicate pairs, mixed loop/forward kinds), SCC components, the typed
  // transitive/carry reduction, alongside sanitation and phase refs with one oracle batch.
  {
    const random = api.mulberry32(20261031);
    const ids = ["a", "b", "c", "d", "e", "f"];
    const edgeEnds = [...ids, "entry", "sink", "abort", "missing"];
    const kinds = ["next", "loop", "exit", "jump", "branch"] as const;
    const pick = <T extends unknown>(items: readonly T[]): T =>
      items[Math.floor(random() * items.length)] as T;
    for (let index = 0; index < 40; index += 1) {
      const analysis = api.json(
        api.analysisOfScript(
          api.readFixture("native/crates/nc-code-workflow/tests/fixtures/workflow_linear.ts"),
        ),
      );
      const flow = api.requireFlow(analysis, `synthetic/random-phase-graph-${index}`);
      flow.phases = ids.map((id) => ({
        id,
        ...(random() < 0.75 ? { name: `phase ${id}` } : {}),
        ...(random() < 0.5 ? { loc: { line: 1, column: 1 } } : {}),
        ...(random() < 0.5
          ? {
              alongside: Array.from({ length: 1 + Math.floor(random() * 8) }, () =>
                pick([...ids, "missing"]),
              ),
            }
          : {}),
      }));
      flow.phaseEdges = Array.from({ length: 8 + Math.floor(random() * 24) }, () => ({
        from: pick(edgeEnds),
        to: pick(edgeEnds),
        kind: pick(kinds),
      }));
      const causality = api.requireCausality(analysis, `synthetic/random-phase-graph-${index}`);
      for (const step of causality.steps) step.phase = random() < 0.8 ? pick(ids) : "missing";
      for (const participant of api.requireHandoff(
        analysis,
        `synthetic/random-phase-graph-${index}`,
      ).participants) {
        participant.phase = random() < 0.8 ? pick(ids) : "missing";
      }
      api.addAnalysisCase(`synthetic/random-phase-graph-${index}`, analysis);
    }
  }
}
