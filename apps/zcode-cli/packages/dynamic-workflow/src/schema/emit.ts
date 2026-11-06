// @ts-nocheck
import type { JsonSchema } from "./types.js";
import { MAX_UNION_MEMBERS } from "./types.js";

/** TS-compiler-API-free schema emission. */

export class SchemaRejection extends Error {
  constructor(readonly reason: string, readonly path: string) {
    super(reason);
    this.name = "SchemaRejection";
  }
}

/** Minimal SchemaEmitter compatible with legacy API but operating on parsed type descriptors. */
export class SchemaEmitter {
  private defs: Record<string, JsonSchema> = {};
  constructor(_checker?: any, _location?: any) {}

  emitTop(type: any): JsonSchema {
    const schema = this.emit(type, "$");
    if (Object.keys(this.defs).length) return { ...schema, $defs: this.defs };
    return schema;
  }

  private emit(type: any, path: string): JsonSchema {
    if (!type) throw new SchemaRejection("unknown type", path);
    const flags = type.flags ?? 0;
    // TS TypeFlags numeric values used when given legacy TS types (duck-typed)
    if (flags & 1) throw new SchemaRejection("type 'any' is not allowed; use 'unknown' or a concrete type", path); // Any
    if (flags & 2) return {}; // Unknown
    if (flags & 131072) throw new SchemaRejection("type 'never' cannot be represented", path); // Never
    if (flags & (32768 | 16384)) throw new SchemaRejection("'undefined' is only allowed on optional properties", path); // Undefined|Void
    if (flags & 65536) return { type: "null" }; // Null
    if (flags & 16) return { const: (type as any).intrinsicName === "true" }; // BooleanLiteral (approx)
    if (flags & 8) return { type: "boolean" }; // Boolean
    if (flags & 128) return { const: (type as any).value }; // StringLiteral
    if (flags & 256) return { const: (type as any).value }; // NumberLiteral
    if (flags & 4) return { type: "string" }; // String
    if (flags & 32) return { type: "number" }; // Number
    if (flags & 64) return { type: "integer" }; // BigInt - approximate
    // If it's a plain descriptor object (our own format)
    if (typeof type.kind === "string") return this.emitDescriptor(type, path);
    // union
    if (Array.isArray((type as any).types)) {
      const members = (type as any).types as any[];
      if (members.length > MAX_UNION_MEMBERS) throw new SchemaRejection(`union has ${members.length} members (max ${MAX_UNION_MEMBERS})`, path);
      // filter undefined from optionals
      const filtered = members.filter(m => !(m.flags & (32768 | 16384)));
      if (filtered.length === 1) return this.emit(filtered[0], path);
      const literals = filtered.every(m => (m.flags & (128 | 256 | 512 | 1024)));
      if (literals) return { enum: filtered.map(m => m.value ?? (m.intrinsicName === "true" ? true : m.intrinsicName === "false" ? false : null)) };
      return { anyOf: filtered.map((m, i) => this.emit(m, `${path}|${i}`)) };
    }
    // array
    if ((type as any).symbol?.name === "Array" || (type as any).target?.symbol?.name === "Array") {
      const targs = (type as any).resolvedTypeArguments ?? (type as any).typeArguments ?? [];
      return { type: "array", items: targs[0] ? this.emit(targs[0], `${path}[]`) : {} };
    }
    // object - try to extract properties via checker-like interface
    if ((type as any).getProperties || flags & 524288) { // Object
      return { type: "object", additionalProperties: true };
    }
    return {};
  }

  private emitDescriptor(d: any, path: string): JsonSchema {
    switch (d.kind) {
      case "string": return { type: "string" };
      case "number": return { type: "number" };
      case "boolean": return { type: "boolean" };
      case "null": return { type: "null" };
      case "unknown": case "any": return {};
      case "array": return { type: "array", items: d.items ? this.emitDescriptor(d.items, `${path}[]`) : {} };
      case "object": {
        const props: Record<string, JsonSchema> = {};
        const req: string[] = [];
        for (const [k, v] of Object.entries(d.properties ?? {})) {
          const pd = v as any;
          props[k] = this.emitDescriptor(pd.optional ? unwrapOptional(pd.type) : pd.type ?? pd, `${path}.${k}`);
          if (!pd.optional) req.push(k);
        }
        const s: JsonSchema = { type: "object", properties: props, additionalProperties: false };
        if (req.length) s.required = req;
        return s;
      }
      case "union": return { anyOf: (d.members ?? []).map((m: any, i: number) => this.emitDescriptor(m, `${path}|${i}`)) };
      case "literal": return { const: d.value };
      default: return {};
    }
  }
}

function unwrapOptional(t: any): any {
  if (!t) return { kind: "unknown" };
  if (t.kind === "union") {
    const ms = (t.members ?? []).filter((m: any) => m.kind !== "undefined");
    return ms.length === 1 ? ms[0] : { ...t, members: ms };
  }
  return t;
}
