/**
* Browser-safe pure projection bucket:
 * `@zcode/dynamic-workflow/projections`。
 *
* The downstream browser-side consumer freezes `AnalysisCore` into core.json, decodes it in the browser and recalculates the site map / cause and effect diagram / on the spot.
* CFG / handover diagram / actor diagram, and then use `*ToMermaid` or display contract to draw it. There can't be anything on that chain.
* `typescript` - It is a compiler required when the analyzer casts core, it is several MB in size and should not be included in the front-end package.
 *
* Therefore, only modules that do not import `typescript` at runtime are exported here: core's type and specification text, JSON
* Codecs, five projections, reducers, mermaid and text serializers, graph types. `analyzeWorkflowScript`,
* The compiler, lowering, and engine are not included - they are exported from the root.
 */
export {
  isActorSite,
  serializeCore,
  type AnalysisCore,
  type CoreActorSite,
  type CoreAskSite,
  type CoreFacts,
  type CoreFanoutSite,
  type CoreSimpleSite,
  type CoreSites,
  type CoreTypes,
} from "./analysis/core.js";
export {
  decodeAnalysisCore,
  encodeAnalysisCore,
  type AnalysisCoreJson,
  type MapEntries,
} from "./analysis/core-json.js";
export type {
  ActorEvent,
  ControlFact,
  IssueEvent,
  JumpEvent,
  MarkEvent,
  OrderEvent,
  OrderRegion,
  OrderTrace,
  PhaseInfo,
  SettleEvent,
} from "./analysis/causality-order.js";
export {
  isStructuralRegionKind,
  UNPHASED_ID,
  type JumpKind,
  type RegionKind,
  type StructuralRegionKind,
  type TraceRegionKind,
} from "./analysis/constants.js";
export { projectSiteGraph } from "./analysis/graph.js";
export { toActorGraph, type ActorEdge, type ActorGraph, type ActorNode } from "./analysis/actor-graph.js";
export {
  projectCausalityGraph,
  SINK_ID,
  UNKNOWN_LANE,
  WORKSPACE_LANE,
  type CausalityGraph,
  type Certainty,
  type Lane,
  type NamePattern,
  type OrderEdge,
  type OrderKind,
  type Phase,
  type Region,
  type Step,
  type StepKind,
} from "./analysis/causality-graph.js";
export { reduceOrdering, type ReducibleEdge } from "./analysis/causality-reduce.js";
export {
  FLOW_ABORT,
  FLOW_ENTRY,
  FLOW_SINK,
  projectControlFlow,
  type ControlFlowGraph,
  type FlowEdge,
  type FlowEdgeKind,
  type FlowNode,
  type FlowNodeKind,
  type FlowPhase,
  type FlowVia,
} from "./analysis/flow-graph.js";
export {
  FANOUT_EXPAND_CAP,
  projectHandoffGraph,
  UNPHASED,
  type Handoff,
  type HandoffGraph,
  type HandoffParticipant,
} from "./analysis/handoff-graph.js";
export {
  actorGraphToMermaid,
  causalityGraphToMermaid,
  controlFlowToMermaid,
  handoffGraphToMermaid,
  phaseFlowToMermaid,
  phaseGraphToMermaid,
  siteGraphToMermaid,
} from "./analysis/mermaid.js";
export {
  serializeActorGraph,
  serializeCausalityGraph,
  serializeControlFlow,
  serializeGraph,
  serializeHandoffGraph,
} from "./analysis/serialize.js";
export type { ActorSite, SiteEdge, SiteGraph, SiteKind, SiteLoc, SiteNode } from "./analysis/types.js";
