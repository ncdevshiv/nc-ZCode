export { FACADE_DTS, FACADE_FILE_NAME, SNIPPET_FACADE_DTS } from "./facade/dts.js";
export { WORLD_READ_CAPS } from "./facade/world-read-caps.js";
export { REPORT_CAPS } from "./facade/report-caps.js";
// User interface products: upper limit constants, registry vocabulary, compile-time lists and diagnostics.
export { ARTIFACT_CAPS, ARTIFACT_ID_PATTERN } from "./facade/artifact-caps.js";
export {
  ARTIFACT_REGISTRY,
  artifactFamilyOf,
  isArtifactPresetOp,
  type ArtifactContentOp,
  type ArtifactOp,
  type ArtifactPresetOp,
  type ArtifactRow,
} from "./facade/registry.js";
export {
  ARTIFACT_DECLARATION_CODE,
  ARTIFACT_HOISTING_CODE,
  ARTIFACT_PRIMARY_CONFLICT_CODE,
  collectArtifactDeclarations,
  type ArtifactDeclarations,
  type DeclaredArtifact,
} from "./analysis/artifacts.js";
export {
  collectDiagnostics,
  compileWorkflowScript,
  createWorkflowProgram,
  SCRIPT_FILE_NAME,
  type CompileDiagnostic,
  type CompileResult,
  type CreateWorkflowProgramOptions,
  type ScriptLoc,
  type WorkflowProgram,
} from "./compiler/compile.js";
export { analyzeWorkflowScript, type AnalyzeResult } from "./analysis/analyze.js";
export { collectSites, type SiteTable } from "./analysis/sites.js";
export {
  collectWorldRunCommands,
  WORLD_RUN_LITERAL_CODE,
  type WorldRunCommands,
} from "./analysis/world-run.js";
export { collectPhaseMarkerDiagnostics, PHASE_MARKER_CODE } from "./analysis/phases.js";
export {
  toActorGraph,
  type ActorEdge,
  type ActorGraph,
  type ActorNode,
} from "./analysis/actor-graph.js";
export {
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
  type RegionKind,
  type Step,
  type StepKind,
} from "./analysis/causality-graph.js";
// Greedy irreducible transitive reduction. display
// The clipping layer reuses it to perform untyped reduction on a single type of edge.
// ——All the forward edges are of the same kind and the backward edges are carried. It degenerates into an ordinary irreducible reduction and is reliable on loop input.
export { reduceOrdering, type ReducibleEdge } from "./analysis/causality-reduce.js";
export {
  UNPHASED_ID,
  type JumpKind,
  type StructuralRegionKind,
  type TraceRegionKind,
} from "./analysis/causality-order.js";
// Control flow projection: occurrence-level CFG and stage quotient.
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
  actorGraphToMermaid,
  causalityGraphToMermaid,
  controlFlowToMermaid,
  handoffGraphToMermaid,
  phaseFlowToMermaid,
  phaseGraphToMermaid,
  siteGraphToMermaid,
} from "./analysis/mermaid.js";
// Canonical text form of cause-and-effect diagram (snapshot surface).
export {
  serializeCausalityGraph,
  serializeControlFlow,
  serializeHandoffGraph,
} from "./analysis/serialize.js";
// Handover diagram projection: participant cards and handover edges on the second layer of the board.
export {
  FANOUT_EXPAND_CAP,
  projectHandoffGraph,
  UNPHASED,
  type Handoff,
  type HandoffGraph,
  type HandoffParticipant,
} from "./analysis/handoff-graph.js";
// Unify the core product of analysis and its normative text form:
// Site diagram / cause and effect diagram / actor diagram are all pure projections on it.
export {
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
// core's JSON encoding and decoding: the sample package freezes `AnalysisCore` into
// `core.json`, the downstream browser recalculates the projection on the frozen product on-site; the Map is placed in order `[key, value][]`.
export {
  decodeAnalysisCore,
  encodeAnalysisCore,
  type AnalysisCoreJson,
} from "./analysis/core-json.js";
export type {
  ActorSite,
  SiteEdge,
  SiteGraph,
  SiteKind,
  SiteLoc,
  SiteNode,
} from "./analysis/types.js";
export {
  buildAskSpecs,
  synthesizeAskSchemas,
  synthesizeAskSchemas as synthesizeWorkflowSchemas,
  type SchemaSynthesisResult,
} from "./schema/synthesize.js";
export { validate, formatViolation, formatViolations } from "./schema/validate.js";
// Submit profile for each actor site: Compilation time determines the subagent to take the typed / generic / none submit_result tool.
export {
  GENERIC_SUBMIT_PROFILE,
  deriveActorSubmitProfiles,
  deriveActorSubmitProfilesFor,
  type ActorSubmitProfile,
} from "./schema/actor-submit-profiles.js";
export { serializeSchema } from "./schema/serialize.js";
export {
  MAX_UNION_MEMBERS,
  SCHEMA_DIAGNOSTIC_CODE,
  type JsonSchema,
  type JsonSchemaType,
  type JsonValue,
  type Violation,
} from "./schema/types.js";
export {
  HOST_BINDING,
  lowerWorkflow,
  lowerWorkflowScript,
  type LoweredWorkflow,
  type LowerResult,
} from "./lowering/index.js";
export {
  WorkflowEngine,
  InMemoryJournalStore,
  WorkflowError,
  INSTRUCTIONS_HEAD_MAX_CHARS,
  LAST_TOOL_NAME_MAX_CHARS,
  LAST_TOOL_TARGET_MAX_CHARS,
  NUDGE_ATTEMPTS,
  REPAIR_ATTEMPTS,
  canonicalJson,
  fnv1a,
  inputHash,
  refToString,
  type ActorId,
  type ActorRecord,
  type ActorRef,
  type ActorSessionSeed,
  type ArtifactPublishRequest,
  type ArtifactRef,
  type ArtifactVersionRecord,
  type AskMessage,
  type AskLastTool,
  type AskProgress,
  type AskSpec,
  type AskStats,
  type Caps,
  type EngineConfig,
  type ImportedActorCandidate,
  type ImportedAskEntry,
  type ImportedRunCache,
  type ImportedWorldEntry,
  type InstanceRef,
  type JournalStorePort,
  type ListEventsOptions,
  type NodeKind,
  type NodeOutcome,
  type NodeRecord,
  type NodeRecordStatus,
  type WorldReadInput,
  WORLD_READ_INPUT_MAX_BYTES,
  type PersonaSpec,
  type RunEvent,
  type RunRecord,
  type RunSettlement,
  type RunSettlementRecord,
  type RunStallInfo,
  type RunStatus,
  type RunStopReason,
  type SessionRef,
  type StoredEvent,
  type SubmitVerdict,
  type ValidateFn,
  type WorkflowDriver,
  type WorkflowErrorCode,
  type WorkflowErrorJson,
  type ProviderStopDetails,
  type WorkflowErrorMismatch,
  type WorkflowHostApi,
  type WorkflowReportSink,
  type WorldReadOp,
  type AskWaitInfo,
  type ConcurrencyChange,
  type ConcurrencyChangeReason,
} from "./engine/index.js";
export {
  ConcurrencyController,
  CONCURRENCY_DECREASE_FACTOR,
  CONCURRENCY_FLOOR,
  CONCURRENCY_IDLE_RESET_MS,
  CONCURRENCY_INCREASE_AFTER_SUCCESSES,
  CONCURRENCY_INCREASE_STEP,
  type ConcurrencyControllerSnapshot,
  type ConcurrencyThrottleReason,
} from "./engine/index.js";
