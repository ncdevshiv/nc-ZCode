// @ts-nocheck
/** Artifact declaration collection (TS-compiler-API-free). */
import type { CompileDiagnostic, WorkflowProgram } from "../compiler/compile.js";
import type { SiteTable } from "./sites.js";
import { ARTIFACT_ID_PATTERN } from "../facade/artifact-caps.js";

export const ARTIFACT_DECLARATION_CODE = 9005;
export const ARTIFACT_HOISTING_CODE = 9007;
export const ARTIFACT_PRIMARY_CONFLICT_CODE = 9008;

export interface DeclaredArtifact { id: string; kind: string }
export interface ArtifactDeclarations {
  declaredArtifacts: DeclaredArtifact[];
  diagnostics: CompileDiagnostic[];
}

export function collectArtifactDeclarations(workflow: WorkflowProgram, table: SiteTable): ArtifactDeclarations {
  const seen = new Map<string, { kind: string }>();
  const diagnostics: CompileDiagnostic[] = [];
  const sites = ((table as any).artifacts ?? []) as any[];
  for (const site of sites) {
    const id = site.artifactId;
    if (!id) {
      diagnostics.push({
        code: ARTIFACT_DECLARATION_CODE, column: site.loc?.column ?? 1, line: site.loc?.line ?? 1,
        message: "artifact id must be a compile-time string literal.",
      });
      continue;
    }
    if (!ARTIFACT_ID_PATTERN.test(id)) {
      diagnostics.push({
        code: ARTIFACT_DECLARATION_CODE, column: site.loc?.column ?? 1, line: site.loc?.line ?? 1,
        message: `artifact id "${id}" does not match required pattern.`,
      });
    }
    const prev = seen.get(id);
    if (prev && prev.kind !== site.op) {
      diagnostics.push({
        code: ARTIFACT_PRIMARY_CONFLICT_CODE, column: site.loc?.column ?? 1, line: site.loc?.line ?? 1,
        message: `artifact "${id}" declared as both "${prev.kind}" and "${site.op}".`,
      });
    } else if (!prev) {
      seen.set(id, { kind: site.op });
    }
  }
  const declaredArtifacts = [...seen.entries()].map(([id, v]) => ({ id, kind: v.kind })).sort((a, b) => a.id.localeCompare(b.id));
  return { declaredArtifacts, diagnostics };
}
