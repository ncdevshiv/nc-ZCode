// @ts-nocheck
import { collectDiagnostics, createWorkflowProgram } from "../compiler/compile.js";
import type { CompileDiagnostic } from "../compiler/compile.js";
import { collectSites } from "./sites.js";
import { collectFacadeMisuse } from "./facade-misuse.js";
import { collectWorldRunCommands } from "./world-run.js";
import { collectArtifactDeclarations, type DeclaredArtifact } from "./artifacts.js";
import { collectPhaseMarkerDiagnostics } from "./phases.js";
import { collectDuplicateActorNames, FANOUT_ACTOR_NAME_CODE } from "./actor-names.js";
import { interpret } from "./interpret.js";
import { projectSiteGraph } from "./graph.js";
import { projectCausalityGraph, type CausalityGraph } from "./causality-graph.js";
import { projectControlFlow, type ControlFlowGraph } from "./flow-graph.js";
import { projectHandoffGraph, type HandoffGraph } from "./handoff-graph.js";
import type { AnalysisCore } from "./core.js";
import type { SiteGraph } from "./types.js";

/**
 * Result of analyzing a workflow script: the same diagnostics `compileWorkflowScript`
 * produces, plus the analysis core and its two projected views.
 *
 * `ok` means "submittable": any diagnostic clears it. The core and the graphs are present
 * whenever the script was analyzable at all, which is *almost* the same thing — the one
 * diagnostic that leaves them in place is {@link FANOUT_ACTOR_NAME_CODE} (a static actor
 * name inside a fan-out). That clause describes a **run-time** failure, not something that
 * stops the analysis: the shape is perfectly analyzable, and withholding the graph would
 * hide the picture exactly when the author needs it to see which fan-out to fix. So:
 * graphs present ⟸ ok, but not the converse.
 */
export interface AnalyzeResult {
  diagnostics: CompileDiagnostic[];
  ok: boolean;
  /**
   * The all-in-one analysis artifact: taint
   * facts + temporal trace, position-free. `graph` and `causality` are pure projections
   * of it — deriving them again later needs the core only, never the script.
   */
  core?: AnalysisCore;
  graph?: SiteGraph;
  /** The presentation-level happens-before view. */
  causality?: CausalityGraph;
  /** Where execution can go next, per occurrence and per phase. */
  flow?: ControlFlowGraph;
  /** Who takes part in each phase and who hands off to whom. */
  handoff?: HandoffGraph;
  /**
* **User interface product** declared by the script: `[{id, kind}]`, remove duplicates and press id
* Sort. Compiled products of the same family as world.run's command set - you can tell what this workflow will produce before running it.
* Unlike the graph, it is given as usual when the diagnostic is non-null (a fact that it is read directly from the site table and does not rely on interpretation).
   */
  declaredArtifacts: DeclaredArtifact[];
}

/**
 * Typecheck a workflow script and, when clean, interpret it. The pipeline is
 * createWorkflowProgram -> collectSites (the site-table substrate) -> interpret (the
 * fused taint-fixpoint + temporal walk, minting the {@link AnalysisCore}) ->
 * projectSiteGraph / projectCausalityGraph (pure projections of the core).
 *
 * Between the site table and the interpretation runs the facade-siting check
 * ({@link collectFacadeMisuse}): a facade callable that escapes into value space has
 * no site, so the graph cannot represent it. Its diagnostics are surfaced exactly like
 * typechecker diagnostics — `ok: false`, graphs withheld.
 */
export function analyzeWorkflowScript(scriptText: string): AnalyzeResult {
  const workflow = createWorkflowProgram(scriptText);
  const diagnostics = collectDiagnostics(workflow.program);
// Cannot compile / facade Even the site table cannot be trusted when escaping, and the product list can only be empty (default rather than absent: reader
// What you get is always an array, there is no need to distinguish between "no product" and "unable to analyze" at each consumption point).
  if (diagnostics.length > 0) return { declaredArtifacts: [], diagnostics, ok: false };

  const table = collectSites(workflow);
  const misuse = collectFacadeMisuse(workflow, table);
  if (misuse.length > 0) return { declaredArtifacts: [], diagnostics: misuse, ok: false };

// world.run literal cmd check and misuse: A command that is formed at runtime has no demonstrable authorization
// Object (the command set displayed in the confirmation window is closed at compile time), so it is the same as "the facade call must have a site"
// The kind of errors caused by compilation-time teaching and rewriting. Same for phase tag: non-literal
// Neither names nor non-statement position markers have anything to refer to.
//Literal actor has the same name: the main entrance of the rule is at runtime (DuplicateActorName of engine createActor),
// This trip just moves the part of the literal that can be seen through to the cheaper side.
// Apply three times together - the author can see everything to be changed in one go.
  const worldRun = collectWorldRunCommands(workflow, table);
// The compile-time rules of the product are the same: id is a compile-time literal, and the label points to a
// Declared presets and members with the same ID do not span two types of members - all three are of the "it is better to teach and rewrite now than explode during runtime" category.
  const artifacts = collectArtifactDeclarations(workflow, table);
  const authoring = [
    ...worldRun.diagnostics,
    ...artifacts.diagnostics,
    ...collectPhaseMarkerDiagnostics(workflow, table),
    ...collectDuplicateActorNames(workflow, table),
  ];
// The static actor name (9006) in fan-out is the only one in this batch that does not include the picture below: it says that this script is running
// It will hit DuplicateActorName instead of "this code cannot be analyzed" - the shape itself is completely analyzable, and deducting the image will only
// Remove the picture when the author most needs to see which fan-out the picture is targeting. It clears `ok` as usual (the commit is still blocked).
// Incidental benefit: The analysis corpus can also pin the graphic product in the shape of "static name in fan-out", otherwise a block of data will block the way.
// Compiler-time diagnostics will render its own shape inexpressible in the corpus.
  const withholding = authoring.filter((d) => d.code !== FANOUT_ACTOR_NAME_CODE);
  if (withholding.length > 0) {
    return { declaredArtifacts: artifacts.declaredArtifacts, diagnostics: authoring, ok: false };
  }

  // One interpretation feeds everything: the core carries the taint facts AND the
  // temporal trace, and both graphs project off it without touching the AST again.
  const core = interpret(workflow, table);
  const graph = projectSiteGraph(core);
  const causality = projectCausalityGraph(core, graph);
  return {
    causality,
    core,
    declaredArtifacts: artifacts.declaredArtifacts,
    diagnostics: authoring,
    flow: projectControlFlow(core),
    graph,
    handoff: projectHandoffGraph(core, causality, graph),
    ok: authoring.length === 0,
  };
}
