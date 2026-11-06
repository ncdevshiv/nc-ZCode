// ============================================================
// Anthropic strict tool schema: qualification determination + subset folding
// ============================================================
// Anthropic `strict: true` uses constrained decoding to ensure that tool_use.input exactly satisfies input_schema,
// But only accepts a subset of JSON Schema (API document "JSON Schema Limitations"):
// Support: object/array/string/integer/number/boolean/null, enum/const/anyOf/allOf,
// A set of string format, `additionalProperties: false` (required for all objects).
// Not supported: numerical constraints (minimum/maximum/multipleOf), string length constraints, complex array constraints, recursion $ref,
// `additionalProperties` takes a value other than false.
// This module turns a common schema into its equivalent in a strict subset: foldable constraints **folded into description** (the model can still be read,
// The engine side validator is still mandatory), and the shape that cannot be folded returns undefined (the caller sends it as it is, without strict).
// The first user is typed `submit_result` of the dwf mono subagent.

import type { JsonSchema } from "@zcode/contracts";

/** String format recognized by strict mode; other formats are folded into description. */
const STRICT_STRING_FORMATS = new Set([
  "date-time",
  "time",
  "date",
  "duration",
  "email",
  "hostname",
  "uri",
  "ipv4",
  "ipv6",
  "uuid",
]);

/** Folds in the description keyword and its wording. */
const FOLDED_KEYWORDS: Record<string, (value: unknown) => string> = {
  minimum: (v) => `minimum ${String(v)}`,
  maximum: (v) => `maximum ${String(v)}`,
  exclusiveMinimum: (v) => `greater than ${String(v)}`,
  exclusiveMaximum: (v) => `less than ${String(v)}`,
  multipleOf: (v) => `multiple of ${String(v)}`,
  minLength: (v) => `at least ${String(v)} characters`,
  maxLength: (v) => `at most ${String(v)} characters`,
  pattern: (v) => `must match /${String(v)}/`,
  minItems: (v) => `at least ${String(v)} items`,
  maxItems: (v) => `at most ${String(v)} items`,
  default: (v) => `default ${JSON.stringify(v)}`,
};

/**
* Determine whether the modelId is an Anthropic first-party direct connection model (bare `claude-` prefix; `anthropic/claude-...`, etc.
*Gateway routing ID does not count). Only the first-party model adopts strict. Compatible gateways may reject tool fields they don't recognize, so
* Qualification determination and provider capability boundaries must be kept separate; normal schema behavior is used when qualification cannot be confirmed.
 */
export function isAnthropicFirstPartyModelId(modelId: string | undefined): boolean {
  return typeof modelId === "string" && modelId.startsWith("claude-");
}

/**
* Convert schema to its equivalent in the strict subset; returns undefined if the shape is inexpressible.
 *
* Not expressible = `$ref`/`$defs` (recursive), `additionalProperties` is schema(Record<string, T>),
* Empty schema `{}` (`unknown`, no type to constrain), tuple (`prefixItems`). FOLDED = FOLDED_KEYWORDS
* `format` other than strict. object defaults to `additionalProperties: false` (compositing side emits it).
 */
export function toStrictToolSchema(schema: JsonSchema): JsonSchema | undefined {
  const out = strictNode(schema);
  return out === INELIGIBLE ? undefined : out;
}

const INELIGIBLE = Symbol("ineligible");
type StrictNode = JsonSchema | typeof INELIGIBLE;

function strictNode(node: JsonSchema): StrictNode {
  if ("$ref" in node || "$defs" in node || "prefixItems" in node) return INELIGIBLE;

  const notes: string[] = [];
  const out: JsonSchema = {};
  for (const [key, value] of Object.entries(node)) {
    const fold = FOLDED_KEYWORDS[key];
    if (fold !== undefined) {
      notes.push(fold(value));
      continue;
    }
    if (key === "format" && typeof value === "string" && !STRICT_STRING_FORMATS.has(value)) {
      notes.push(`format ${value}`);
      continue;
    }
    out[key] = value;
  }

// Recursive: properties/items/anyOf/allOf/additionalProperties.
  if (isRecord(out.properties)) {
    const properties: Record<string, JsonSchema> = {};
    for (const [name, child] of Object.entries(out.properties)) {
      if (!isRecord(child)) return INELIGIBLE;
      const strictChild = strictNode(child);
      if (strictChild === INELIGIBLE) return INELIGIBLE;
      properties[name] = strictChild;
    }
    out.properties = properties;
  }
  if (isRecord(out.items)) {
    const items = strictNode(out.items);
    if (items === INELIGIBLE) return INELIGIBLE;
    out.items = items;
  }
  for (const combinator of ["anyOf", "allOf"] as const) {
    const branches = out[combinator];
    if (!Array.isArray(branches)) continue;
    const strictBranches: JsonSchema[] = [];
    for (const branch of branches) {
      if (!isRecord(branch)) return INELIGIBLE;
      const strictBranch = strictNode(branch);
      if (strictBranch === INELIGIBLE) return INELIGIBLE;
      strictBranches.push(strictBranch);
    }
    out[combinator] = strictBranches;
  }

  const types = Array.isArray(out.type) ? out.type : out.type === undefined ? [] : [out.type];
  if (types.includes("object")) {
// Record<string, T>: additionalProperties is schema → strict subset cannot be expressed.
    if (isRecord(out.additionalProperties)) return INELIGIBLE;
    if (out.additionalProperties !== false) out.additionalProperties = false;
    if (!isRecord(out.properties)) out.properties = {};
  }
// Empty schema (unknown): There is nothing to constrain, strict does not accept it.
  if (Object.keys(out).length === 0 && notes.length === 0) return INELIGIBLE;

  if (notes.length > 0) {
    const existing = typeof out.description === "string" ? out.description.trim() : "";
    const folded = notes.join("; ");
    out.description = existing.length > 0 ? `${existing} (${folded})` : folded;
  }
  return out;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
