// @ts-nocheck
import type { JsonSchema, JsonValue } from "./types.js";

/** TS-compiler-API-free JSDoc constraint harvesting (text/duck-typed). */

const NUMERIC_TAGS: Record<string, keyof JsonSchema> = {
  exclusiveMaximum: "exclusiveMaximum", exclusiveMinimum: "exclusiveMinimum",
  maximum: "maximum", maxItems: "maxItems", maxLength: "maxLength",
  minimum: "minimum", minItems: "minItems", minLength: "minLength",
};
const STRING_TAGS: Record<string, keyof JsonSchema> = { format: "format", pattern: "pattern" };

function parseDefault(raw: string): JsonValue {
  if (raw.length === 0) return "";
  try { return JSON.parse(raw) as JsonValue; } catch { return raw; }
}

/** Harvest from a duck-typed symbol (legacy) or return empty. */
export function harvestConstraints(symbol: any, checker?: any): Partial<JsonSchema> {
  const out: Partial<JsonSchema> = {};
  if (!symbol) return out;
  try {
    if (checker && typeof symbol.getDocumentationComment === "function") {
      // legacy TS symbol path - but we avoid importing ts; use duck typing
      const parts = symbol.getDocumentationComment(checker);
      const doc = Array.isArray(parts) ? parts.map((p: any) => p?.text ?? "").join("").trim() : "";
      if (doc) out.description = doc;
      if (typeof symbol.getJsDocTags === "function") {
        for (const tag of symbol.getJsDocTags(checker)) applyTag(out, tag?.name, Array.isArray(tag?.text) ? tag.text.map((p:any)=>p?.text??"").join("").trim() : String(tag?.text??""));
      }
    }
  } catch { /* no TS compiler available */ }
  return out;
}

/** Parse JSDoc constraints from raw comment text (TS-compiler-free). */
export function harvestConstraintsFromText(text: string): Partial<JsonSchema> {
  const out: Partial<JsonSchema> = {};
  if (!text) return out;
  // description = text before first @tag
  const desc = text.replace(/^\s*\*?\s?/gm, "").split(/@\w+/)[0]?.trim() ?? "";
  if (desc) out.description = desc;
  const tagRe = /@(\w+)\s+([^\n@*]+)/g;
  let m: RegExpExecArray | null;
  while ((m = tagRe.exec(text)) !== null) applyTag(out, m[1], m[2].trim());
  return out;
}

function applyTag(out: Partial<JsonSchema>, name: string | undefined, raw: string): void {
  if (!name) return;
  const nk = NUMERIC_TAGS[name];
  if (nk !== undefined) { const v = Number(raw); if (Number.isFinite(v)) (out as any)[nk] = v; return; }
  const sk = STRING_TAGS[name];
  if (sk !== undefined) { if (raw.length > 0) (out as any)[sk] = raw; return; }
  if (name === "default") out.default = parseDefault(raw);
}

export function mergeConstraints(schema: JsonSchema, extra: Partial<JsonSchema>): JsonSchema {
  const merged: JsonSchema = { ...schema };
  for (const [key, value] of Object.entries(extra)) {
    if (value === undefined) continue;
    if (key === "description" && merged.description !== undefined) continue;
    (merged as any)[key] = value;
  }
  return merged;
}
