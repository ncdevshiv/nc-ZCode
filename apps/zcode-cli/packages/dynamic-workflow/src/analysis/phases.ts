// @ts-nocheck
/**
 * `phase("...")` compile-time verification (TS-compiler-API-free).
 */
import type { CompileDiagnostic, ScriptLoc, WorkflowProgram } from "../compiler/compile.js";
import type { SiteTable } from "./sites.js";

export const PHASE_MARKER_CODE = 9004;
const LITERAL_MESSAGE =
  'phase()\'s argument must be a compile-time string literal ("gate" or a no-substitution ' +
  "template): the script's phase names are fixed when it is submitted, because they label " +
  "the graph the user confirms before anything runs. Write the name inline — a name that " +
  "only exists at run time cannot be drawn.";
const EMPTY_MESSAGE =
  'phase("") has no name to show: the phase label is what the confirmation graph draws. ' +
  'Give the group a word ("preflight", "gate", "wrap-up"), or drop the marker — a script ' +
  "with no markers is perfectly legal and is drawn step by step.";

export function collectPhaseMarkerDiagnostics(
  workflow: WorkflowProgram,
  table: SiteTable,
): CompileDiagnostic[] {
  const diagnostics: CompileDiagnostic[] = [];
  const push = (loc: ScriptLoc, message: string): void => {
    diagnostics.push({ code: PHASE_MARKER_CODE, column: loc.column, line: loc.line, message });
  };
  for (const marker of table.phases) {
    const nameLoc = marker.nameExpr && typeof (marker.nameExpr as any).start === "number"
      ? workflow.toScriptLoc((marker.nameExpr as any).start)
      : marker.loc;
    if (marker.name === undefined) push(nameLoc, LITERAL_MESSAGE);
    else if (marker.name.trim() === "") push(nameLoc, EMPTY_MESSAGE);
  }
  return diagnostics;
}
