// @ts-nocheck
import type { CompileDiagnostic, WorkflowProgram } from "../compiler/compile.js";
import type { SiteTable } from "../analysis/sites.js";
import type { AskSpec } from "../engine/types.js";
import { SchemaRejection } from "./emit.js";
import type { JsonSchema } from "./types.js";
import { SCHEMA_DIAGNOSTIC_CODE } from "./types.js";

/** TS-compiler-API-free schema synthesis.
 * Primary path: the Rust native compiler produces schemas (in CompiledScript.askSpecs).
 * This module provides a compatible TS API that works without the TypeScript compiler:
 * it attempts lightweight extraction from oxc AST/script text when a real TS TypeChecker
 * is unavailable, and falls back gracefully.
 */

export interface SchemaSynthesisResult {
  schemas: Record<string, JsonSchema>;
  diagnostics: CompileDiagnostic[];
}

function isRealChecker(checker: any): boolean {
  // Our compat stub's getTypeAtLocation returns {} and has no real type resolution
  if (!checker) return false;
  try {
    // A real TS checker has getSymbolAtLocation that behaves meaningfully; our stub returns undefined always
    // but we can't distinguish. Check for a method that stubs don't have custom behavior for.
    return typeof checker.getApparentType === "function" && checker._isReal === true;
  } catch { return false; }
}

export function synthesizeAskSchemas(workflow: WorkflowProgram, table: SiteTable): SchemaSynthesisResult {
  const checker = workflow.program?.getTypeChecker?.();
  const schemas: Record<string, JsonSchema> = {};
  const diagnostics: CompileDiagnostic[] = [];

  // With TS-compiler-free compile, we don't have a real type checker.
  // Try to synthesize from source text by parsing interface/type declarations near ask sites.
  const src = workflow.scriptText ?? "";
  for (const site of table.asks) {
    const call: any = site.call;
    const typeArgName = extractTypeArgName(call, src, workflow);
    if (!typeArgName || typeArgName === "string") continue;
    try {
      const schema = schemaFromTypeName(typeArgName, src);
      if (schema) schemas[site.id] = schema;
    } catch (error) {
      if (!(error instanceof SchemaRejection)) throw error;
      diagnostics.push({
        code: SCHEMA_DIAGNOSTIC_CODE, column: site.loc.column, line: site.loc.line,
        message: `${error.reason} (at ${error.path})`,
      });
    }
  }
  // report item diagnostics - best effort, no type checker
  return { diagnostics, schemas };
}

function extractTypeArgName(call: any, src: string, workflow: WorkflowProgram): string | null {
  // ESTree/oxc: call.typeArguments / call.typeParameters
  const targs = call?.typeArguments?.params ?? call?.typeParameters?.params ?? call?.typeArguments;
  if (Array.isArray(targs) && targs[0]) {
    const ta = targs[0];
    if (ta.type === "TSTypeReference" && ta.typeName) {
      return ta.typeName.name ?? ta.typeName.right?.name ?? null;
    }
    if (ta.type === "TSStringKeyword") return "string";
    // fallback: slice source
    if (typeof ta.start === "number" && typeof ta.end === "number" && workflow.wrappedSource) {
      const raw = workflow.wrappedSource.slice(ta.start, ta.end).trim();
      // strip prelude offset issues by using script text if possible
      return raw || null;
    }
  }
  // TS AST fallback: typeArguments[0] node text
  const tsArgs = call?.typeArguments;
  if (Array.isArray(tsArgs) && tsArgs[0]) {
    try { if (typeof tsArgs[0].getText === "function") return tsArgs[0].getText(); } catch {}
  }
  return null;
}

/** Best-effort schema extraction from interface/type declarations in source text. */
function schemaFromTypeName(name: string, src: string): JsonSchema | null {
  // Find `interface Name { ... }` or `type Name = { ... }`
  const ifaceRe = new RegExp(`\\binterface\\s+${escapeRe(name)}\\s*(?:extends[^{]+)?\\{([\\s\\S]*?)\\n\\}`, "m");
  let m = ifaceRe.exec(src);
  if (m) return parseInterfaceBody(m[1]);
  const typeRe = new RegExp(`\\btype\\s+${escapeRe(name)}\\s*=\\s*\\{([\\s\\S]*?)\\n\\}`, "m");
  m = typeRe.exec(src);
  if (m) return parseInterfaceBody(m[1]);
  return null;
}

function parseInterfaceBody(body: string): JsonSchema {
  const properties: Record<string, JsonSchema> = {};
  const required: string[] = [];
  // simple line-based parser for `name?: type;` / `name: type;`
  const propRe = /^\s*(?:\/\*\*[\s\S]*?\*\/\s*)*(?:\/\/[^\n]*\n\s*)*(\w+)\s*(\?)?\s*:\s*([^;\/]+?)(?:;|\s*$)/gm;
  let pm: RegExpExecArray | null;
  while ((pm = propRe.exec(body)) !== null) {
    const [, pname, opt, ptype] = pm;
    properties[pname] = typeExprToSchema(ptype.trim());
    if (!opt) required.push(pname);
  }
  const schema: JsonSchema = { type: "object", properties, additionalProperties: false };
  if (required.length) schema.required = required;
  return schema;
}

function typeExprToSchema(t: string): JsonSchema {
  t = t.replace(/\s+/g, " ").trim();
  if (t === "string") return { type: "string" };
  if (t === "number") return { type: "number" };
  if (t === "boolean") return { type: "boolean" };
  if (t === "null") return { type: "null" };
  if (t === "unknown" || t === "any") return {};
  if (t.endsWith("[]")) return { type: "array", items: typeExprToSchema(t.slice(0, -2)) };
  const arrM = /^Array<(.+)>$/.exec(t);
  if (arrM) return { type: "array", items: typeExprToSchema(arrM[1]) };
  if (t.includes("|")) {
    const parts = t.split("|").map(s => s.trim()).filter(s => s !== "undefined");
    if (parts.length === 1) return typeExprToSchema(parts[0]);
    return { anyOf: parts.map(typeExprToSchema) };
  }
  // string literals
  if (/^"[^"]*"$/.test(t) || /^'[^']*'$/.test(t)) return { const: t.slice(1, -1) };
  return {};
}

function escapeRe(s: string): string { return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"); }

export function buildAskSpecs(_workflow: WorkflowProgram, table: SiteTable, schemas: Record<string, JsonSchema>): Record<string, AskSpec> {
  const out: Record<string, AskSpec> = {};
  for (const site of table.asks) {
    const schema = schemas[site.id];
    out[site.id] = schema ? { typed: true, schema: schema as any } : { typed: false };
  }
  return out;
}

export { SchemaRejection } from "./emit.js";
