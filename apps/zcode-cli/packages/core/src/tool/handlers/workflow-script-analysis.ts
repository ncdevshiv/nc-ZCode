// ============================================================
// Workflow script analysis - shared entry for single-slot memory
// ============================================================
//
// The approval gate will analyze the script before execution, and analyze it again after the handler is executed. An approved call therefore needs to be run twice.
// TypeScript program. Make a single slot memory according to the original script and fold this pair into one compilation; deliberately not a universal cache——
// The only repetition worthy of convergence is this set of immediately preceding and following calls.
//
// This module is extracted because **two** tools now share it: CreateWorkflow needs to be compiled before running the script, and SaveWorkflow
// The script must also be compiled before saving, and it must use the same checker - "it can be saved but cannot be run" is the most difficult thing to explain about this feature
// A way to be broken.

import { analyzeWorkflowScript, type AnalyzeResult } from "@zcode/dynamic-workflow";

let lastAnalysis: { script: string; result: AnalyzeResult } | null = null;

/** Compile and analyze a workflow script, and the adjacent repeated calls hit the memory slot. */
export function analyzeScript(script: string): AnalyzeResult {
  if (lastAnalysis?.script === script) return lastAnalysis.result;
  const result = analyzeWorkflowScript(script);
  lastAnalysis = { script, result };
  return result;
}
