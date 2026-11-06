// @ts-nocheck
import type { RegionKind } from "./constants.js";
import type { OrderKind } from "./causality-reduce.js";
import type { SiteLoc, NamePattern } from "./types.js";

// causality-graph.ts reaches the upper limit of oxlint max-lines (400 lines) and changes the public type of causality graph
// (Step / Region / OrderEdge / Lane / Phase / CausalityGraph and three lane constants) and internal
// Fact is split into this file; the public side is still exported from causality-graph.ts (export it as it is there). This file does not import
// `typescript`, the browser-side `./projections` bucket is safe to reach.

export type StepKind = "ask" | "world-read";
export type Certainty = "always" | "maybe";

/** The single lane every `files.*` read runs in. */
export const WORKSPACE_LANE = "workspace";
/** Lane of an ask whose receiver the analysis could not resolve to any actor site. */
export const UNKNOWN_LANE = "unknown";
/** The terminal marker: the artifact the script returns. */
export const SINK_ID = "sink";

export interface Step {
  /** A site id (`ask#3`, `world-read#1`); it is also the runtime instance's key. */
  id: string;
  kind: StepKind;
  label: string;
/** When `label` only gets the secret string, inline the template shape of the `agent()` receiver. */
  labelPattern?: NamePattern;
  loc: SiteLoc;
  /** Actor site id, or `workspace` / `unknown`. For an unexpanded may-set, `lanes[0]`. */
  lane: string;
  /** Present only when the ask receiver is dynamically selected: the candidate lanes. */
  lanes?: string[];
  /**
   * The step this one was expanded from, emitted only on a may-set copy (see
   * `expandMaySetLanes` in causality-graph-lanes.ts). It is the site id a runtime instance
   * actually reports, which is what makes it the live overlay's join key (`source ?? id`
   * against `node.siteId`, narrowed by `node.actorSiteId === lane`).
   *
   * This composes with future per-callsite specialization rather than competing with it. Once
   * specialization lands, the expanded-from id IS the specialized site id —
   * `ask#3/2~actor#1` carries source `ask#3/2` — so the join stays stable under both
   * features. On a copy the field means "the site the runtime reports", NOT "the site
   * before specialization".
   */
  source?: string;
  /** Innermost enclosing region id. */
  region: string;
  certainty: Certainty;
  /**
   * The author-declared phase this step belongs to (`phase#2`, or the reserved
   * `unphased`). Present only when the script declares at least one `phase()` marker, in
   * which case EVERY step carries one — a step claimed by k>1 phases is expanded into k
   * copies to keep the partition total (see phase-graph.ts).
   */
  phase?: string;
  /**
   * How this step's instances relate when an enclosing region repeats:
   *   `stack`  — they coexist, which is the one multiplicity cue (stacked cards);
   *   `serial` — they follow one another, already visible as the cycle-closing arrow.
   * Absent when the step runs at most once. Finding: the cue splits on
   * CONCURRENCY, not cardinality — a step is `serial` exactly when a barrier inside
   * the repeating region settles it, or when a fixed actor's mailbox serializes it.
   */
  repeat?: "stack" | "serial";
}

export interface Region {
  id: string;
  kind: RegionKind;
  parent?: string;
  loc?: SiteLoc;
  /** Derivable literal round count of a `for`; model-only, never displayed. */
  bound?: number;
  label?: string;
}

export interface OrderEdge {
  from: string;
  to: string;
  kind: OrderKind;
  certainty: Certainty;
  /** `data` edges only: the taint witness path's exactness bit. Model-only. */
  exact?: boolean;
}

export interface Lane {
  id: string;
  /** The author's verbatim word, when `agent()` was given a literal. Never a reconstruction. */
  name?: string;
  /**
* When `name` is absent and the first argument to `agent()` is a template string with holes, the static shape of that name. The ellipsis is added when rendering.
   */
  namePattern?: NamePattern;
  loc?: SiteLoc;
  /**
   * Enclosing iteration regions of the `agent()` call, outermost first. A non-empty
   * list makes the lane a family — one fresh actor per element — and nested iterations
   * MULTIPLY, so multiplicity is the product and a single id cannot express it.
   */
  families?: string[];
}

/**
 * An author-declared phase: a name for a group of steps, and the position of the marker
 * that opened it. The synthetic fallback `unphased` carries neither — the UI shows a
 * localized word for it, exactly as it does for the `workspace` / `unknown` lanes.
 */
export interface Phase {
  id: string;
  name?: string;
  loc?: SiteLoc;
}

export interface CausalityGraph {
  steps: Step[];
  regions: Region[];
  lanes: Lane[];
  edges: OrderEdge[];
  /** Step ids whose artifacts reach the script's return; absent when it returns nothing. */
  sink?: { fedBy: string[] };
  /**
   * The phase vocabulary, present TOGETHER WITH `phaseEdges` and `Step.phase` or not at
   * all: a script with no `phase()` markers gets none of the three and its graph is
   * byte-identical to what it was before the feature. `unphased` first when it has
   * members, then the author's phases in first-reach order.
   */
  phases?: Phase[];
  /** The quotient of `edges` by the phase partition. Never carries `exact`. */
  phaseEdges?: OrderEdge[];
}

/** Ordered facts inside the projection (inputs for deduplication, edge reshaping, reduction); not on the public side of the package. */
export interface Fact {
  from: string;
  to: string;
  kind: OrderKind;
  certainty: Certainty;
  exact?: boolean;
/** The underlying kind of the carry side (the forward kind before modification); when the carry is minimized, press it to determine the strength of the witness. */
  carryOf?: Exclude<OrderKind, "carry">;
  /**
* The stage set where the issue event that witnesses this fact is located (**only the seq fact generated by the await barrier** takes it).
* The stage copy is based on the head end of its narrowed edge: the barrier fact is witnessed on a specific issue, and the current "one site, one step"
* The design combines multiple issues from the same site at different call points into one step. If you lose this source, it will
* It is also true to treat "gate's bench before cargo test has been settled" as preflight's bench——
* A time assertion in the wrong direction (multiple ordering is allowed, misordering is not).
   *
* Absent = no source information = headend full expansion (data/control/fifo with zone duplication fact by contract full expansion).
   *
* There is no symmetrical source information at the tail end (the settle event does not have a stage), but **the tail side is no longer a gap**: the time of stage copy
* The feasibility judgment (`admits` of phase-graph.ts) intercepts both ends equally according to the position, so "copy arrangement"
* Before the step that produces its own actual parameters" this kind of pseudo-edge is eliminated by that rule, not relying on the source information here.
   */
  toPhases?: Set<string>;
  /**
* This `seq` fact can only be true by **the next round**: `from` is emitted in a branch arm ending with `continue`,
* `to` is located further back in the same loop body. Linear time tracking will continue to complete the loop body according to may semantics, so
* Remember "fixer@k before juries@k+1" as the forward order of the same round; the control flow projection for the same place is
* `loop via=continue`. Facts with this mark are treated as carry when finalizing the edge (all the facts in the pairing are considered to be carried,
* See `dedupeFacts`, causality-graph-lanes.ts). Arms ending in `break` are stronger: step in them is not
* It is impossible to precede the following step in the same loop, and such fact will not be sent directly.
   */
  viaJump?: true;
}
