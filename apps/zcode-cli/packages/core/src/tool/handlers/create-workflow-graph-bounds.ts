// ============================================================
// CreateWorkflow display graph bounding - Bounded projection of tool output boundaries
// ============================================================
// Detach from create-workflow.ts: stage vocabulary list
// The handler file exceeds the max-lines upper limit, and the clipping itself is a piece of self-consistent pure logic - the analyzer enters the entire graph, and the contract
// Shape out, no I/O, no memo, no ports. with zcode-protocol-v4/create-workflow-display.ts from
// rows.ts breaks out the same precedent.
//
// What is installed here is a **display diagram** and is no longer a mirror image of the cause and effect diagram. The second layer is subagent-oriented: only the site table is left in the step layer.
// (Running state and viewer keys, no longer drawn, no longer with edges), the participant layer (subagent card + handover edge at each stage) is composed of
// The handover graph projection of the analyzer is provided. Here we only do upper limit and forwarding. The stage layer is still the stage quotient of the control flow graph and is still reduced by this layer.
// (Folding and point reduction are in create-workflow-graph-fold.ts). The edge has only one shape: `{from, to, back?}`;
// region / certainty / edge types stay in the analyzer, the GUI never reads them. Function name and payload field name inheritance history.

import {
  CREATE_WORKFLOW_GRAPH_MAX_HANDOFF_TYPES,
  CREATE_WORKFLOW_GRAPH_MAX_HANDOFFS,
  CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS,
  CREATE_WORKFLOW_GRAPH_MAX_LANES,
  CREATE_WORKFLOW_GRAPH_MAX_NAME_CHARS,
  CREATE_WORKFLOW_GRAPH_MAX_PARTICIPANTS,
  CREATE_WORKFLOW_GRAPH_MAX_PHASE_EDGES,
  CREATE_WORKFLOW_GRAPH_MAX_PHASES,
  CREATE_WORKFLOW_GRAPH_MAX_STEPS,
  type CreateWorkflowCausalityGraph,
  type CreateWorkflowEdge,
  type CreateWorkflowHandoff,
  type CreateWorkflowLane,
  type CreateWorkflowNamePattern,
  type CreateWorkflowParticipant,
  type CreateWorkflowPhase,
  type CreateWorkflowStep,
} from "@zcode/contracts";
import {
  FLOW_ABORT,
  FLOW_ENTRY,
  FLOW_SINK,
  type CausalityGraph,
  type ControlFlowGraph,
  type HandoffGraph,
  UNPHASED,
// The browser-side playback view directly reuses this function: go to /projections
// Subpath instead of the root bucket. The root bucket will drag the typescript compiler into the browser package. The semantics are exactly the same as root bucket export.
} from "@zcode/dynamic-workflow/projections";
// The folding and reduction of stage edges (including the contraction point rules on the ring input) are separated into modules, see the file header of this file.
import { foldPhaseEdges, type RawEdge } from "./create-workflow-graph-fold.js";

// display does not go through the tool result budget: the graph must be entered into the tool output (and thus into real-time events and persistence
// metadata) independent length limit. Collections refer to each other, so the pruning order is fixed - step first (source order, truncation retained
//The beginning of the script), then converge to the lanes they reference, and finally filter the edges and sinks according to the surviving steps. Referential integrity takes precedence
// Regarding the number of reservations: it is better to draw less than to let the UI get the id pointing to a non-existent node.
export function boundCausalityGraph(
  graph: CausalityGraph,
  flow?: ControlFlowGraph,
  handoff?: HandoffGraph,
): CreateWorkflowCausalityGraph {
  let truncated = graph.steps.length > CREATE_WORKFLOW_GRAPH_MAX_STEPS;
  const headSteps = graph.steps.slice(0, CREATE_WORKFLOW_GRAPH_MAX_STEPS);

  // 1. Lanes: only the ones the surviving steps stand in, in the graph's lane order
  //    (workspace first, actors in creation order).
  const wantedLanes = new Set<string>();
  for (const step of headSteps) {
    wantedLanes.add(step.lane);
    for (const lane of step.lanes ?? []) wantedLanes.add(lane);
  }
  const laneList = graph.lanes.filter((lane) => wantedLanes.has(lane.id));
  truncated = truncated || laneList.length > CREATE_WORKFLOW_GRAPH_MAX_LANES;
  const keptLanes = laneList.slice(0, CREATE_WORKFLOW_GRAPH_MAX_LANES);
  const laneIds = new Set(keptLanes.map((lane) => lane.id));

  // 2. A step whose own lane got dropped has nowhere to sit; a may-set narrows instead.
  const steps = headSteps.filter((step) => laneIds.has(step.lane));
  truncated = truncated || steps.length < headSteps.length;
  const stepIds = new Set(steps.map((step) => step.id));

  // 3. Participants and hand-offs (the analyzer's projection, already reduced and in
  //    stack order). A card's steps narrow to the surviving ones and a card left with none
  //    goes; the list then truncates in order (the opener survives, the tail does not),
  //    and hand-offs touching a dropped card go with it. A dropped card's steps stay in
  //    `steps` — run status still joins on them, they just have no card.
  const laneOk = (lane: string): boolean => laneIds.has(lane);
  const participantList: CreateWorkflowParticipant[] = [];
  for (const participant of handoff?.participants ?? []) {
    if (!laneOk(participant.lane)) continue;
    const memberSteps = participant.steps.filter((id) => stepIds.has(id));
    if (memberSteps.length === 0) continue;
    participantList.push({
      id: boundGraphText(participant.id, CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS),
      phase: boundGraphText(participant.phase, CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS),
      lane: boundGraphText(participant.lane, CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS),
      steps: memberSteps
        .slice(0, CREATE_WORKFLOW_GRAPH_MAX_STEPS)
        .map((id) => boundGraphText(id, CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS)),
      ...(participant.member === undefined ? {} : { member: { ...participant.member } }),
      ...(participant.many === true ? { many: true as const } : {}),
    });
  }
  truncated = truncated || participantList.length < (handoff?.participants.length ?? 0);
  truncated = truncated || participantList.length > CREATE_WORKFLOW_GRAPH_MAX_PARTICIPANTS;
  const participants = participantList.slice(0, CREATE_WORKFLOW_GRAPH_MAX_PARTICIPANTS);
  const participantIds = new Set(participants.map((participant) => participant.id));
  const handoffList: CreateWorkflowHandoff[] = (handoff?.handoffs ?? [])
    .filter((edge) => participantIds.has(edge.from) && participantIds.has(edge.to))
    .map((edge) => {
      const types = (edge.types ?? [])
        .slice(0, CREATE_WORKFLOW_GRAPH_MAX_HANDOFF_TYPES)
        .map((type) => boundGraphText(type, CREATE_WORKFLOW_GRAPH_MAX_NAME_CHARS))
        .filter((type) => type.length > 0);
      return {
        from: boundGraphText(edge.from, CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS),
        to: boundGraphText(edge.to, CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS),
        ...(edge.back === true ? { back: true as const } : {}),
        ...(types.length > 0 ? { types } : {}),
      };
    });
  truncated = truncated || handoffList.length < (handoff?.handoffs.length ?? 0);
  truncated = truncated || handoffList.length > CREATE_WORKFLOW_GRAPH_MAX_HANDOFFS;
  const handoffs = handoffList.slice(0, CREATE_WORKFLOW_GRAPH_MAX_HANDOFFS);

  const sink = (graph.sink?.fedBy ?? [])
    .filter((id) => stepIds.has(id))
    .slice(0, CREATE_WORKFLOW_GRAPH_MAX_STEPS)
    .map((id) => boundGraphText(id, CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS));

  // 4. Phases: the vocabulary is all-or-nothing and comes from the
  //    CONTROL-FLOW projection — every phase some occurrence carries, member steps or not
  //    (a marker-only phase is a position control passes through, so it must show). Edges
  //    touching `entry` / `abort` have no node to land on; edges into `sink` become
  //    `exits`; self-loops say nothing at quotient granularity. Over either bound and the
  //    whole vocabulary goes, `phase` stripped from every step with it: a step naming an
  //    unlisted phase is a dangling reference, and referential integrity beats retention.
  const declaredPhases = flow?.phases;
  const phaseIds = new Set((declaredPhases ?? []).map((phase) => phase.id));
  const rawPhaseEdges: RawEdge[] = [];
  const exitSet = new Set<string>();
  for (const edge of flow?.phaseEdges ?? []) {
    if (edge.from === FLOW_ENTRY || edge.from === FLOW_ABORT || edge.to === FLOW_ABORT) continue;
    if (!phaseIds.has(edge.from)) continue;
    if (edge.to === FLOW_SINK) {
      exitSet.add(edge.from);
      continue;
    }
    if (!phaseIds.has(edge.to)) continue;
    rawPhaseEdges.push({ back: edge.kind === "loop", from: edge.from, to: edge.to });
  }
  const phaseEdges: CreateWorkflowEdge[] = foldPhaseEdges(rawPhaseEdges).map((edge) => ({
    from: boundGraphText(edge.from, CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS),
    to: boundGraphText(edge.to, CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS),
    ...(edge.back ? { back: true as const } : {}),
  }));
// The upper bound of the edge controls what is sent out, so it is evaluated after the reduction**.
  const phaseVocabularyDropped =
    declaredPhases !== undefined &&
    (declaredPhases.length > CREATE_WORKFLOW_GRAPH_MAX_PHASES ||
      phaseEdges.length > CREATE_WORKFLOW_GRAPH_MAX_PHASE_EDGES);
  truncated = truncated || phaseVocabularyDropped;
  const emitPhases = declaredPhases !== undefined && !phaseVocabularyDropped;

  const boundPhases: CreateWorkflowPhase[] = (declaredPhases ?? []).map((phase) => {
// Synthesis phase `unphased` has no name (UI localization); empty names are also processed as "unnamed" instead of outputting the entire
// Parsing failed, same attitude as lane name.
    const name = phase.name ? boundGraphText(phase.name, CREATE_WORKFLOW_GRAPH_MAX_NAME_CHARS) : undefined;
// `alongside`: other stages that are still running when entering this stage (strand is not joined). Same referential integrity as edge
// Rules - references pointing to unlisted stages are discarded, self-references are discarded (it is not parallel to itself), deduplication is preserved, and the upper bound is the same
// Stage table. It **not** goes through foldEdges/reduceOrdering: it's the fact that it's nodes not edges, control doesn't go from there
// Transfer it over, and the reduction will treat it as a runs after to cut off the real edge. Moving forward and backward with the vocabulary is automatic
// ——boundPhases The entire table is loaded only when emitPhases is true.
    const alongside: string[] = [];
    const alongsideSeen = new Set<string>();
    for (const id of phase.alongside ?? []) {
      if (id === phase.id || !phaseIds.has(id) || alongsideSeen.has(id)) continue;
      if (alongside.length >= CREATE_WORKFLOW_GRAPH_MAX_PHASES) break;
      alongsideSeen.add(id);
      alongside.push(boundGraphText(id, CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS));
    }
    return {
      id: boundGraphText(phase.id, CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS),
      ...(name === undefined ? {} : { name }),
      ...(phase.loc === undefined ? {} : { line: phase.loc.line, column: phase.loc.column }),
      ...(alongside.length === 0 ? {} : { alongside }),
    };
  });
  const exits = (declaredPhases ?? [])
    .filter((phase) => exitSet.has(phase.id))
    .map((phase) => boundGraphText(phase.id, CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS));

  const boundLanes: CreateWorkflowLane[] = keptLanes.map((lane) => {
// An empty name (such as agent("")) will violate the contract's min(1) and be treated as "unnamed" instead of failing the entire output parsing.
    const name = lane.name ? boundGraphText(lane.name, CREATE_WORKFLOW_GRAPH_MAX_NAME_CHARS) : undefined;
    const namePattern = boundNamePattern(lane.namePattern);
    return {
      id: boundGraphText(lane.id, CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS),
      ...(name === undefined ? {} : { name }),
      ...(namePattern === undefined ? {} : { namePattern }),
      ...(lane.loc === undefined ? {} : { line: lane.loc.line, column: lane.loc.column }),
    };
  });

  const boundSteps: CreateWorkflowStep[] = steps.map((step) => {
    const lanes = (step.lanes ?? []).filter((lane) => laneIds.has(lane));
// The label of ask comes from the script literal and may be empty; the contract requires min(1) and returns step id.
    const label = step.label ? boundGraphText(step.label, CREATE_WORKFLOW_GRAPH_MAX_NAME_CHARS) : step.id;
    const labelPattern = boundNamePattern(step.labelPattern);
    return {
      id: boundGraphText(step.id, CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS),
      kind: step.kind,
      label,
      ...(labelPattern === undefined ? {} : { labelPattern }),
      line: step.loc.line,
      column: step.loc.column,
      lane: boundGraphText(step.lane, CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS),
      ...(lanes.length > 1 ? { lanes } : {}),
// may-set copy's associated key: the site pointed to here has been replaced by the copy, so it does not participate in the referential integrity above
// Convergence (that rule governs the edges and nodes pointed to by the sink). The field is optional. If it is omitted, it will not be caught by the schema.
// Let the real-time overlay quietly not be associated with the instance.
      ...(step.source === undefined
        ? {}
        : { source: boundGraphText(step.source, CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS) }),
// When the entire vocabulary is downgraded, `phase` must disappear together. `phase` pointing to an unlisted phase also disappears: with a
// Cards with phase ids that are not in `phases` are dangling references, and the UI will check for a non-existent phase. Analyzer guarantees
// "The stage of each issue is the stage of a certain node", so this tightening has zero behavioral changes under normal input; clipping layer
//'s stance is still one of self-defense rather than trust in the producer.
      ...(emitPhases && step.phase !== undefined && phaseIds.has(step.phase)
        ? { phase: boundGraphText(step.phase, CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS) }
        : {}),
      ...(step.repeat === undefined ? {} : { repeat: step.repeat }),
    };
  });

// The phase of a card has the same rules as the `phase` of step: the vocabulary is downgraded, or points to an unlisted phase → falls under `unphased`
// (Contract: `participant.phase` ∈ `phases[].id`, or all `unphased` in `phases` absence). card id
// Do not change - it is an opaque key, and the transition edge and running state are all associated with it; the implicit module of the UI only looks at the `phase` field.
  const boundParticipants: CreateWorkflowParticipant[] = participants.map((participant) =>
    emitPhases && phaseIds.has(participant.phase) ? participant : { ...participant, phase: UNPHASED },
  );

  return {
    steps: boundSteps,
    lanes: boundLanes,
    participants: boundParticipants,
    handoffs,
    ...(emitPhases ? { phases: boundPhases, phaseEdges, exits } : {}),
    ...(sink.length > 0 ? { sink } : {}),
    ...(truncated ? { truncated: true } : {}),
  };
}

// Bug prevention: actor name / ask label comes from script string literal (external input), direct slice may be truncated
// UTF-16 surrogate pair; treated the same as result-display's MCP text length limit - the boundary falls at a high position
// After surrogate, half characters are discarded to ensure that the payload can be serialized safely. id is ASCII generated by analysis, and slice is passed identically.
function boundGraphText(value: string, maxChars: number): string {
  const bounded = value.slice(0, maxChars);
  const lastCodeUnit = bounded.charCodeAt(bounded.length - 1);
  return lastCodeUnit >= 0xd800 && lastCodeUnit <= 0xdbff ? bounded.slice(0, -1) : bounded;
}

/**
* The length limit of the name pattern: the two affix are both from script literals and are truncated by the same surrogate-safe method.
 *
* After truncation, there may be no affix left (theoretically the analyzer is guaranteed to be non-empty, but the min(1) of the contract should not depend on the upstream one)
* Guaranteed) - then the entire field is absent, rather than sending a `{}` and letting `.strict()` pass and rendering a lone
* Ellipsis.
 */
function boundNamePattern(
  pattern: { head?: string; tail?: string } | undefined,
): CreateWorkflowNamePattern | undefined {
  if (pattern === undefined) return undefined;
  const head = pattern.head ? boundGraphText(pattern.head, CREATE_WORKFLOW_GRAPH_MAX_NAME_CHARS) : "";
  const tail = pattern.tail ? boundGraphText(pattern.tail, CREATE_WORKFLOW_GRAPH_MAX_NAME_CHARS) : "";
  if (head === "" && tail === "") return undefined;
  return {
    ...(head === "" ? {} : { head }),
    ...(tail === "" ? {} : { tail }),
  };
}
