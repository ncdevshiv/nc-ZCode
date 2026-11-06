// ============================================================
// Workflow analysis → display - the only projection of static analysis results to the display graph/score card
// ============================================================
//
// The display diagram is composed of **three** analysis projections: the cause and effect diagram for sites and lanes, the control flow diagram for stage vocabulary and stage edges, and the handover diagram for
// Sub-agent cards and handover edges at each stage. If any copy is uploaded less, the picture will be silently missing a layer - the center will be activated directly.
// The run details side panel has no timeline and no subagents. The reason is that the startup path only passes the cause and effect diagram. So "Analysis results →
//Bounded display image" is only spelled out once here; it is called in three places: CreateWorkflow's handler, confirmation window gate and direct startup.
// No one is calling the actual parameters of `boundCausalityGraph(...)` anymore.

import {
  CREATE_WORKFLOW_TOOL_NAME,
  type CreateWorkflowCausalityGraph,
  type CreateWorkflowOutput,
  type ToolResultDisplayPayload,
} from "@zcode/contracts";
import type { AnalyzeResult } from "@zcode/dynamic-workflow";
import { createCreateWorkflowDisplay } from "../executor/result-display.js";
import { boundCausalityGraph } from "./create-workflow-graph-bounds.js";

/** Bounded display of analysis results; absent when the script cannot analyze even a single site (compilation failure occurs earlier than the cause and effect diagram). */
export function boundGraphOfAnalysis(
  analysis: AnalyzeResult,
): CreateWorkflowCausalityGraph | undefined {
  return analysis.causality === undefined
    ? undefined
    : boundCausalityGraph(analysis.causality, analysis.flow, analysis.handoff);
}

/**
* `create_workflow` display of analysis results (confirmation window, startup wheel metadata). Model-oriented `response` in these
* There is no content in front of the reader - the display projection itself does not read it. AmendWorkflow passes its own tool name: two startup tools
* Share the same display kind.
 */
export function displayOfAnalysis(
  analysis: AnalyzeResult,
  toolName: string = CREATE_WORKFLOW_TOOL_NAME,
): ToolResultDisplayPayload | undefined {
  const causalityGraph = boundGraphOfAnalysis(analysis);
  return createCreateWorkflowDisplay(toolName, {
    diagnostics: analysis.diagnostics,
    ok: analysis.ok,
    response: "",
    ...(causalityGraph === undefined ? {} : { causalityGraph }),
  } satisfies CreateWorkflowOutput);
}
