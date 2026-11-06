// ============================================================
// Folding and reduction of stage edges - display the only edge deletion point of the stage layer of the graph
// ============================================================
// Taken out from create-workflow-graph-bounds.ts: folding itself is a piece of self-consistent pure graph theory (ordered pair folding +
// Strongly connected components + run the parser's reduction on shrinkage points), while the rest of the clipping layer is about referential integrity and upper bounds. After disassembling
// This logic can be read and pinned separately, and max-lines margin is left for the bounds file.

import {
  reduceOrdering,
  type ReducibleEdge,
// The same reason as bounds: use the /projections subpath instead of the root bucket, which will use the typescript compiler
// Drag them into the browser package together (the folio playback reuses this link in the browser).
} from "@zcode/dynamic-workflow/projections";

/** An edge before folding: the type has been collapsed into "is it a back edge or not". */
export interface RawEdge {
  from: string;
  to: string;
  back: boolean;
}

/**
* Folding of stage edges (the handover edges are reduced and sent by the analyzer according to the same algorithm, and are no longer touched here):
 *
* 1. The same ordered pair is folded into one line (first-seeing order), `back` exists if and only if **all** edges before folding are back edges -
* Any forward fact is processed as forward. It is better to have one more constraint in the ranking than one less; self-loops are discarded.
 *
* 2. The reduction runs on the **shrinking point** instead of the original image. Take the strongly connected components of forward edges (backward edges are not counted): edges within the same component
* Unconditionally leave the edges across components according to (component pair, kind) greedy irreducible reduction fed to the analyzer after deduplication (the forward edges are the same
* kind, go back to carry(`carryOf: "seq"`), it will degenerate into typeless reduction), and then expand according to the surviving components
* Return to the original side. The output remains in input order.
 *
* Feeding the original image directly to the reduction is wrong: a witness path can go back around the circle
* Come. When two sets of unrelated if/else reuse the same pair of stage names, A→B and B→A in the business graph are both ordinary branch edges.
* Based on this, the reduction determines that choice → A is implied by choice → B → A and deletes it. The picture becomes "The conditional branch always goes to B, and A is B
*'s fork in the road" - a lie. A path through a node in the same ring as the endpoint makes no assertion about whether control can get there, so
* The testimony is only valid when shrinking this DAG.
 */
export function foldPhaseEdges(raw: readonly RawEdge[]): RawEdge[] {
  const folded = foldPairs(raw);
  const componentOf = componentsOf(folded);
  const component = (id: string): string => componentOf.get(id) ?? id;
  const keyOf = (from: string, to: string, back: boolean): string => `${from} ${to} ${back}`;

// The edges across components are deduplicated according to (component pair, kind) and are in first-look order; edges with the same component are not reduced at all - they are self-loops in the contraction point,
// Says nothing about reachability relationships on the DAG.
  const order: string[] = [];
  const byKey = new Map<string, ReducibleEdge>();
  for (const edge of folded) {
    const from = component(edge.from);
    const to = component(edge.to);
    if (from === to) continue;
    const key = keyOf(from, to, edge.back);
    if (byKey.has(key)) continue;
    order.push(key);
    byKey.set(
      key,
      edge.back ? { carryOf: "seq", from, kind: "carry", to } : { from, kind: "seq", to },
    );
  }
  const condensed = order.map((key) => byKey.get(key) as ReducibleEdge);
  const kept = new Set(
    reduceOrdering(condensed).map((edge) => keyOf(edge.from, edge.to, edge.kind === "carry")),
  );
  return folded.filter((edge) => {
    const from = component(edge.from);
    const to = component(edge.to);
    return from === to || kept.has(keyOf(from, to, edge.back));
  });
}

/** The same ordered pair is folded into one line, in first-seeing order; `back` is the conjunction of folded members; self-loops are discarded. */
function foldPairs(raw: readonly RawEdge[]): RawEdge[] {
  const order: string[] = [];
  const byKey = new Map<string, RawEdge>();
  for (const edge of raw) {
    if (edge.from === edge.to) continue;
    const key = `${edge.from} ${edge.to}`;
    const seen = byKey.get(key);
    if (seen === undefined) {
      order.push(key);
      byKey.set(key, { ...edge });
    } else {
      seen.back = seen.back && edge.back;
    }
  }
  return order.map((key) => byKey.get(key) as RawEdge);
}

/**
* Each node → the representative of the strongly connected component where it is located (the node that appears for the first time in the component). Only look at the forward side: the backward side says
* "Next round", counting it into the component will shrink the entire loop body to a point, and the truly redundant edges inside the loop body can no longer be deleted.
 *
* Use mutual reachability instead of Tarjan: stage graph upper bound 32 nodes (`CREATE_WORKFLOW_GRAPH_MAX_PHASES`),
* The cost is irrelevant, and "a to b and b to a" is the component definition itself, which does not need to be proved again when read.
 */
function componentsOf(folded: readonly RawEdge[]): Map<string, string> {
  const nodes: string[] = [];
  const seen = new Set<string>();
  const next = new Map<string, string[]>();
  for (const edge of folded) {
    for (const id of [edge.from, edge.to]) {
      if (seen.has(id)) continue;
      seen.add(id);
      nodes.push(id);
    }
    if (edge.back) continue;
    const list = next.get(edge.from);
    if (list === undefined) next.set(edge.from, [edge.to]);
    else list.push(edge.to);
  }

  const reach = new Map<string, Set<string>>();
  for (const start of nodes) {
    const reached = new Set<string>();
    const stack = [...(next.get(start) ?? [])];
    while (stack.length > 0) {
      const node = stack.pop() as string;
      if (reached.has(node)) continue;
      reached.add(node);
      stack.push(...(next.get(node) ?? []));
    }
    reach.set(start, reached);
  }

  const componentOf = new Map<string, string>();
  for (const node of nodes) {
    if (componentOf.has(node)) continue;
    componentOf.set(node, node);
    for (const other of reach.get(node) ?? []) {
      if (componentOf.has(other)) continue;
      if (reach.get(other)?.has(node) === true) componentOf.set(other, node);
    }
  }
  return componentOf;
}
