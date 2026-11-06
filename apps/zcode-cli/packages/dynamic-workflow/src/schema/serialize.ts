// @ts-nocheck
import type { JsonSchema } from "./types.js";

/**
* Deterministic serialization: recursively sort the object keys of the schema and then JSON.stringify to ensure stable snapshots and launch
* The order of insertion is irrelevant. Arrays (enum/required/prefixItems/anyOf, etc.) retain their original order ——
* Their order is meaningful (and determined by the checker's property/member order).
 */

export function serializeSchema(schema: JsonSchema): string {
  return `${JSON.stringify(canonicalize(schema), null, 2)}\n`;
}

function canonicalize(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonicalize);
  if (value !== null && typeof value === "object") {
    const sorted: Record<string, unknown> = {};
    for (const key of Object.keys(value as Record<string, unknown>).sort()) {
      sorted[key] = canonicalize((value as Record<string, unknown>)[key]);
    }
    return sorted;
  }
  return value;
}
