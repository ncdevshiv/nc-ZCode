// @ts-nocheck
/** world.run literal command collection (TS-compiler-API-free). */
import type { CompileDiagnostic, WorkflowProgram } from "../compiler/compile.js";
import type { SiteTable } from "./sites.js";

export const WORLD_RUN_LITERAL_CODE = 9003;

export interface WorldRunCommands {
  commands: string[];
  diagnostics: CompileDiagnostic[];
}

export function collectWorldRunCommands(workflow: WorkflowProgram, table: SiteTable): WorldRunCommands {
  const commands = new Set<string>();
  const diagnostics: CompileDiagnostic[] = [];
  for (const site of table.worldReads) {
    if (site.op !== "run") continue;
    const args = (site.args ?? []) as any[];
    const cmdArg = args[0];
    const cmd = literalStringValue(cmdArg);
    if (cmd == null) {
      const loc = cmdArg && typeof cmdArg.start === "number" ? workflow.toScriptLoc(cmdArg.start) : site.loc;
      diagnostics.push({
        code: WORLD_RUN_LITERAL_CODE, column: loc.column, line: loc.line,
        message: "world.run(cmd, ...): cmd must be a compile-time string literal.",
      });
    } else {
      commands.add(cmd);
    }
  }
  return { commands: [...commands].sort(), diagnostics };
}

function literalStringValue(node: any): string | null {
  if (!node) return null;
  if (node.type === "Literal" && typeof node.value === "string") return node.value;
  if (node.type === "StringLiteral") return node.value ?? null;
  if (node.type === "TemplateLiteral" && Array.isArray(node.quasis) && node.quasis.length === 1 && (node.expressions?.length ?? 0) === 0) {
    return node.quasis[0]?.value?.cooked ?? node.quasis[0]?.value?.raw ?? null;
  }
  return null;
}
