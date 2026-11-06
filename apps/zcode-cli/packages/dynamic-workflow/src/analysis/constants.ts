// @ts-nocheck
/**
* Tracking vocabulary: area type, jump type and cover stage id. They originally live in causality-order.ts, but that file
* import `typescript` at runtime (walk the AST itself), while reading only core's pure projection (causality-graph/
* flow-phase / phase-graph) only require these values. Put them into a small file with zero dependencies, and the projected module will no longer
* Drag `typescript` into the runtime closure - this is the portfolio browser package (
* The premise that `@zcode/dynamic-workflow/projections`) can be established. Export causality-order.ts as it is,
* All existing reference locations and behaviors remain unchanged.
 */

/**
 * Region kinds. The analyzer emits `seq` (the root), `loop`,
 * `fanout` and `branch`. `parallel` stays derived — incomparability in the ordering
 * already defines concurrency, and storing it would be a second source of truth.
 * `shared` is reserved for the future specialization cap.
 */
export type RegionKind = "seq" | "parallel" | "loop" | "fanout" | "branch" | "shared";

/**
 * STRUCTURAL region kinds: the tree nodes
 * the walk always saw but did not record before the control-flow projection needed them.
 * The causality projection looks THROUGH every one of them (its "transparency rule"), which
 * is what keeps the three older views byte-identical while the trace grows.
 *
 *  - `choice`: one if / ternary / switch / short-circuit; its arms are `branch` children
 *  - `call`: one inlined helper body (a `return` target)
 *  - `try` / `attempt` / `catch` / `finally`: one try statement and its three parts
 */
export type StructuralRegionKind = "choice" | "call" | "try" | "attempt" | "catch" | "finally";

/** Every kind the trace can hold. The public causality `RegionKind` stays the narrow union. */
export type TraceRegionKind = RegionKind | StructuralRegionKind;

const STRUCTURAL_REGION_KINDS: ReadonlySet<string> = new Set<StructuralRegionKind>([
  "choice",
  "call",
  "try",
  "attempt",
  "catch",
  "finally",
]);

export function isStructuralRegionKind(kind: TraceRegionKind): kind is StructuralRegionKind {
  return STRUCTURAL_REGION_KINDS.has(kind);
}

/** The reserved fallback phase: the root block's initial current phase, and where every
 * step issued before the script's first marker lands. It has no `name` — the UI shows a
 * localized word, exactly as it does for the `workspace` / `unknown` lanes. */
export const UNPHASED_ID = "unphased";

export type JumpKind = "continue" | "break" | "return" | "throw" | "recur";
