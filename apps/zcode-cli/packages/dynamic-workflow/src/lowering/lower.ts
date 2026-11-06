// @ts-nocheck
import {
  collectDiagnostics,
  createWorkflowProgram,
  type CompileDiagnostic,
  type WorkflowProgram,
} from "../compiler/compile.js";
import { collectSites, type SiteTable } from "../analysis/sites.js";
import { collectFacadeMisuse } from "../analysis/facade-misuse.js";

/** TS-compiler-API-free lowering. Authoritative lowering in Rust nc-code-workflow/compile/ts_workflow. */

export const HOST_BINDING = "__host";

export interface LoweredWorkflow {
  code: string;
  siteIds: string[];
}

export interface LowerResult {
  diagnostics: CompileDiagnostic[];
  ok: boolean;
  lowered?: LoweredWorkflow;
}

export function lowerWorkflowScript(scriptText: string): LowerResult {
  const workflow = createWorkflowProgram(scriptText);
  const diagnostics = collectDiagnostics(workflow.program ?? workflow);
  if (diagnostics.length > 0) return { diagnostics, ok: false };
  const table = collectSites(workflow);
  const misuse = collectFacadeMisuse(workflow, table);
  if (misuse.length > 0) return { diagnostics: misuse, ok: false };
  return { diagnostics, lowered: lowerWorkflow(workflow, table), ok: true };
}

export function lowerWorkflow(workflow: WorkflowProgram, table: SiteTable): LoweredWorkflow {
  const source = workflow.scriptText ?? "";
  // Build replacements from site table (sites collected via oxc-based/compat path have spans)
  type Rep = { start: number; end: number; text: string };
  const reps: Rep[] = [];
  const allSites: Array<{ site: any; kind: string }> = [
    ...(table.phases ?? []).map((s: any) => ({ site: s, kind: "phase" })),
    ...(table.actors ?? []).map((s: any) => ({ site: s, kind: "actor" })),
    ...(table.worldReads ?? []).map((s: any) => ({ site: s, kind: "world-read" })),
    ...(table.asks ?? []).map((s: any) => ({ site: s, kind: "ask" })),
    ...(table.reports ?? []).map((s: any) => ({ site: s, kind: "report" })),
    ...((table as any).artifacts ?? []).map((s: any) => ({ site: s, kind: "artifact" })),
  ];
  // sort by call start desc for stable replacement (use call.start/end if present in script coords)
  // Spans from oxc are in wrapped source; we need to map to original script. Sites collected
  // by compat paths may not have precise spans - fall back to returning source with a note.
  const preludeLen = (`async function __workflowScript__() {\n`).length;
  for (const { site, kind } of allSites) {
    const call = site.call as any;
    const ws = workflow.wrappedSource ?? "";
    const startW = call?.start ?? call?.span?.start;
    const endW = call?.end ?? call?.span?.end;
    if (typeof startW !== "number" || typeof endW !== "number") continue;
    const start = Math.max(0, startW - preludeLen);
    const end = Math.max(start, Math.min(source.length, endW - preludeLen));
    let text = source.slice(start, end); // fallback
    if (kind === "phase") {
      text = site.name != null ? `${HOST_BINDING}.enterPhase(${JSON.stringify(site.name)})` : "void 0";
    } else if (kind === "actor") {
      const args = [JSON.stringify(site.id)];
      const name = site.name; if (name != null) args.push(JSON.stringify(name));
      text = `${HOST_BINDING}.createActor(${args.join(", ")})`;
    } else if (kind === "world-read") {
      text = `${HOST_BINDING}.worldRead(${JSON.stringify(site.id)}, ${JSON.stringify(site.op)}, [])`;
    } else if (kind === "ask") {
      const recv = site.receiverText ?? "undefined";
      const instr = site.instructionText ?? "undefined";
      text = `${HOST_BINDING}.ask(${JSON.stringify(site.id)}, ${recv}, ${instr})`;
    } else if (kind === "report") {
      const parts = [JSON.stringify(site.id)];
      if (site.itemText) parts.push(site.itemText);
      if (site.artifactId) parts.push(JSON.stringify(site.artifactId));
      text = `${HOST_BINDING}.report(${parts.join(", ")})`;
    } else if (kind === "artifact") {
      const member = site.op === "file" || site.op === "markdown" ? "publishArtifact" : "declareArtifact";
      text = `${HOST_BINDING}.${member}(${JSON.stringify(site.id)}, ${JSON.stringify(site.op)}, [])`;
    }
    reps.push({ start, end, text });
  }
  reps.sort((a, b) => b.start - a.start);
  let code = source;
  for (const r of reps) code = code.slice(0, r.start) + r.text + code.slice(r.end);
  const siteIds = [
    ...(table.asks ?? []).map((s: any) => s.id),
    ...(table.actors ?? []).map((s: any) => s.id),
    ...(table.worldReads ?? []).map((s: any) => s.id),
    ...(table.reports ?? []).map((s: any) => s.id),
    ...((table as any).artifacts ?? []).map((s: any) => s.id),
  ];
  return { code, siteIds };
}
