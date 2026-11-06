// @ts-nocheck
/**
 * Type-aware transitive reduction over the happens-before relation. With one arrow
 * style on screen, reduction carries the whole burden of keeping the picture readable —
 * and it must stay TYPED even though rendering is not. A uniform reduction over the
 * untyped relation deletes the wrong arrows: in `planner-reviewer`, `scan → judge` is
 * the genuine data dependency and the incidental ordering path
 * `scan → plan → review → judge` transitively implies it, so a uniform pass would keep
 * the incidental chain and drop the meaningful edge.
 *
 * Precedence: `data` = `control` > `fifo` > `seq`.
 *
* Repair record
 *
* 1. The forward edge deletion decision is changed to edge-by-edge SURVIVING set determination. Old implementation batches once for ORIGINAL relationships
* Delete the edge, and then use the monotonic recovery process to restore the edge that "witnesses the path itself to be deleted" - restoring one edge will re-establish other deleted edges.
* Provide witnesses but delete them without any step, thus converging to severely over-recovered edge sets on inputs with loops (real symptoms:
* In the graph of jsonl-db optimization loop, 55 of the 66 forward edges are contained by other edges). Phase 1’s “one site, one step” rule
* Let the shared helper (called once before and after the loop) produce bidirectional forward edges. Rings are normal rather than abnormal. The edge deletion algorithm must be on the ring.
* Keep it redundant. Naturally, no recovery step is required after edge-by-edge determination of the survival set: each deleted edge has a survival witness at the moment of deletion.
* The later deleted edge will not invalidate the previous deletion - the later deleted edge's own witness can be substituted into the previous witness (kind allows set edges
* seq ⊇ fifo ⊇ data=control shrinks monotonically, and the intensity only increases but does not decrease after substitution). DAG with unique typed pass
* The reduction is consistent edge by edge, only the behavior of graphs with residual cycles changes.
 *
* 2. Added carry minimization (the old rule "carry is never deleted" is invalidated). The carry edge asserts A@k → B@k+1, while
* The combination of "k forward path in rounds → exactly one hop carry → k+1 forward paths in rounds" asserts exactly the same fact, so there is
* The carry of this testimony is purely redundant ink (the 8-step loop once drew 19 back-edges, the forward chain + one back edge will say
* Exhausted). Determine the strength by type hop by hop: each hop witnessed (carry hop) is determined by its underlying kind, that is, the original edge before it is modified.
* kind) must not be weaker than the deleted carry's own underlying kind; exactly one jump to carry, two jumps to assert k → k+2,
* Strict is weaker. In the same way as the forward phase, the survival set is determined edge by edge. The two back edges that witness each other will not disappear at the same time - closed loop
* The loop is still closed.
 */

export type OrderKind = "data" | "control" | "fifo" | "seq" | "carry";

/**
 * Dedup precedence when several facts hold for one ordered pair — the same kind lattice
 * {@link JUSTIFIED_BY} reads, in the shape a dedup needs. Lives here rather than beside
 * either consumer because the step-level dedup and the phase quotient's must agree by
 * construction.
 */
export const KIND_RANK: Record<OrderKind, number> = {
  carry: 0,
  control: 4,
  data: 3,
  fifo: 2,
  seq: 1,
};


export interface ReducibleEdge {
  from: string;
  to: string;
  kind: OrderKind;
  /**
* The underlying kind of the `carry` edge: the original forward kind of the return edge before it was transformed into carry. carry minimize press it
* Determine how strong the witness needs to be; if it is absent, it will be processed as hard (data) - it is better to leave an extra edge instead of accidentally deleting data facts.
   */
  carryOf?: Exclude<OrderKind, "carry">;
}

/** Kinds a justifying path may consist of, per the kind of the edge under test. */
const JUSTIFIED_BY: Partial<Record<OrderKind, ReadonlySet<OrderKind>>> = {
  // A hard dependency yields only to a path of hard dependencies. `data` and `control`
  // are both non-removable — no refactoring can make a consumer precede its producer,
  // or a guarded step precede its guard — so either one justifies either one. What this
  // does NOT yield to is `seq`/`fifo`: those are incidental serialization the reader is
  // meant to be able to delete mentally, and the constraint must survive that deletion.
  // This is what kills the phantom producer→sink edges the actor projection emitted
  // through relays, and what keeps `scan → judge` alive against the incidental
  // `scan → plan → review → judge`.
  //
  // Bug: `data` originally yielded to `data` alone, which kept every data fact that a
  // control edge already implied. In a refine-until-approved loop that is most of the
  // picture — `initial plan → revision` reads as a second arrow on top of
  // `initial plan → initial review → (guards) → revision`, saying nothing the chain
  // did not. Ordering-redundant arrows are pure ink here, because the renderer draws
  // every kind identically; a data edge earns its place only by asserting an order no
  // hard path already asserts.
  control: new Set<OrderKind>(["data", "control"]),
  data: new Set<OrderKind>(["data", "control"]),
  // FIFO yields to a real dependency or to another FIFO hop (a same-actor chain
  // already implies its own transitive closure), but never to bare serialization.
  // For example, `assess → refine → wrap up` already implies `assess → wrap up`.
  fifo: new Set<OrderKind>(["data", "control", "fifo"]),
  // Pure serialization yields to any ordering at all.
  seq: new Set<OrderKind>(["data", "control", "fifo", "seq"]),
};

/** The underlying kind of the carry edge; absence is treated as hard (see {@link ReducibleEdge.carryOf}). */
const underlyingOf = (edge: ReducibleEdge): Exclude<OrderKind, "carry"> =>
  edge.carryOf ?? "data";

/**
 * Drop edges a strong-enough path of surviving edges already implies — forward edges
 * first (each decided against the surviving set, in input order), then `carry` edges
 * against the surviving result (one forward leg, exactly one carry hop, one forward
 * leg). Deterministic given input order; on a DAG the forward phase is the unique
 * typed transitive reduction, and on residual cycles (one step issued from several
 * call sites) both phases stay sound — every drop has a surviving witness — and
 * irredundant.
 */
export function reduceOrdering<E extends ReducibleEdge>(edges: readonly E[]): E[] {
  const dropped = new Set<E>();
  const forward = edges.filter((edge) => edge.kind !== "carry");
  const carries = edges.filter((edge) => edge.kind === "carry");

  const outgoing = new Map<string, E[]>();
  for (const edge of forward) {
    const list = outgoing.get(edge.from);
    if (list === undefined) outgoing.set(edge.from, [edge]);
    else list.push(edge);
  }

  /**
   * Is `to` reachable from `from` over surviving `allowed`-kind forward edges without
   * using any direct `from → to` hop? Any such path has length ≥ 2, which is exactly
   * the reduction condition. Cycle-safe: the visited set bounds the walk.
   *
   * Deliberately certainty-BLIND. A certainty-aware variant (an unconditional ordering
   * may only yield to an unconditional path) is strictly sounder per-execution, and was
   * tried: it restores an edge from every ancestor of the returned artifact in 30 corpus
   * fixtures, because a path through any conditional step stops justifying anything.
   * That is precisely the phantom producer→sink noise the design exists to remove, and
   * the reader is not doing per-execution case analysis — they read a chain as a chain.
   * Certainty stays a model-only property of the surviving edges.
   */
  const reaches = (from: string, to: string, allowed: ReadonlySet<OrderKind>): boolean => {
    const seen = new Set<string>([from]);
    const stack: string[] = [];
    for (const edge of outgoing.get(from) ?? []) {
      if (edge.to === to || !allowed.has(edge.kind) || dropped.has(edge)) continue;
      if (!seen.has(edge.to)) {
        seen.add(edge.to);
        stack.push(edge.to);
      }
    }
    while (stack.length > 0) {
      const node = stack.pop() as string;
      for (const edge of outgoing.get(node) ?? []) {
        if (!allowed.has(edge.kind) || dropped.has(edge)) continue;
        if (edge.to === to) return true;
        if (!seen.has(edge.to)) {
          seen.add(edge.to);
          stack.push(edge.to);
        }
      }
    }
    return false;
  };

  for (const edge of forward) {
    const allowed = JUSTIFIED_BY[edge.kind];
    if (allowed === undefined) continue; // defensive: `carry` is already filtered out
    if (edge.from === edge.to) continue;
    if (reaches(edge.from, edge.to, allowed)) dropped.add(edge);
  }

  const carryOutgoing = new Map<string, E[]>();
  for (const edge of carries) {
    const list = carryOutgoing.get(edge.from);
    if (list === undefined) carryOutgoing.set(edge.from, [edge]);
    else list.push(edge);
  }

  /**
   * Does a surviving composition `forward* → one carry hop → forward*` (every hop of
   * an `allowed` kind, the carry hop judged by its underlying kind) connect `from` to
   * `to` without using `candidate` itself? Two-state walk: state 1 is "the carry hop
   * is spent". Parallel edges cannot occur (facts are deduped per ordered pair before
   * back-edge typing), so any witness found here has length ≥ 2 by construction.
   */
  const carryWitness = (candidate: E, allowed: ReadonlySet<OrderKind>): boolean => {
    const seen = new Set<string>([`${candidate.from} 0`]);
    const stack: [string, 0 | 1][] = [[candidate.from, 0]];
    while (stack.length > 0) {
      const [node, spent] = stack.pop() as [string, 0 | 1];
      const push = (next: string, state: 0 | 1): boolean => {
        if (state === 1 && next === candidate.to) return true;
        const key = `${next} ${state}`;
        if (!seen.has(key)) {
          seen.add(key);
          stack.push([next, state]);
        }
        return false;
      };
      for (const edge of outgoing.get(node) ?? []) {
        if (!allowed.has(edge.kind) || dropped.has(edge)) continue;
        if (push(edge.to, spent)) return true;
      }
      if (spent === 1) continue;
      for (const edge of carryOutgoing.get(node) ?? []) {
        if (edge === candidate || dropped.has(edge) || !allowed.has(underlyingOf(edge))) continue;
        if (push(edge.to, 1)) return true;
      }
    }
    return false;
  };

  for (const edge of carries) {
    const allowed = JUSTIFIED_BY[underlyingOf(edge)];
    if (allowed === undefined) continue;
    if (carryWitness(edge, allowed)) dropped.add(edge);
  }

  return edges.filter((edge) => !dropped.has(edge));
}
