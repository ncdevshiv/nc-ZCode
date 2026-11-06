// @ts-nocheck
import type { ScriptLoc } from "../compiler/compile.js";

/**
 * The site-graph types: the may-flow digraph over facade call sites emitted by the
 * analyzer. This module is the
 * shared vocabulary; the taint pass (later step) fills in the edges.
 */

/**
 * A node kind. `source`/`sink` are the two virtual endpoints; the rest are facade
 * call sites. `fan-out` nodes are only emitted by the taint pass once an iteration
 * candidate is shown to reach a facade site — this substrate never produces them.
 */
export type SiteKind = "ask" | "world-read" | "join" | "fan-out" | "source" | "sink";

/** A 1-based, prelude-stripped location in the author's script. */
export type SiteLoc = ScriptLoc;

/**
* When the name is only formed at runtime (`` agent(`researcher${i + 1}`) ``), the part of the shape that can be obtained statically:
* The literal before the first hole (`head`) and the literal after the last hole (`tail`).
 *
* Why can I only get the shape: Folding the template into 8 specific names requires expanding `map`, and `×N` is the same as
* The meaning of `maybe stack`'s existence is to refuse to expand. So what is given here is the shape, not the name.
 *
* Invariant: at least one of `head`/`tail` is present (undefined will be returned if it cannot be obtained, and empty pattern will not be returned),
* Both have been trimmed, and each contains at least one letter or number - `` `${x}-` `` is rendered as `...-` than "unnamed agent"
* Worse, so that kind of affix is ​​discarded directly. The ellipses are added only when rendering, and only data is moved here.
 */
export interface NamePattern {
  head?: string;
  tail?: string;
}

/**
 * A graph node. The virtual `source`/`sink` carry no location and a fixed label;
 * every other node is positioned at its facade call site. `actors` is populated for
 * `ask` nodes only: the may-set of actor site ids the ask's receiver resolves to
 * (the same set that drives the context relation), so the renderer can place the
 * node in its actor lane(s). Ordered by actor source order, deduped. `within` is the
 * id of the nearest enclosing promoted fan-out node when the site's call lies
 * lexically inside a promoted iteration candidate's body — the site then executes
 * once per element;
 * omitted otherwise. `artifactType` is the producer's statically known artifact type
 * (a human-readable type name, provenance semantics) for `ask`/`world-read`/`join`/
 * `fan-out` nodes; absent on the virtual endpoints and when the type is
 * unknowable/uninformative (`any`/`unknown`/`never`/`void`).
 */
export interface SiteNode {
  id: string;
  kind: SiteKind;
  loc?: SiteLoc;
  label: string;
  /**
* `label` only gets the secret string (inline `` agent(`researcher${i}`).ask(…) ``——receiver is a call type,
* is neither a literal nor an identifier, so it falls into `"ask"`), the static shape of the template. Only `ask` nodes may have this.
   */
  labelPattern?: NamePattern;
  actors?: string[];
  within?: string;
  artifactType?: string;
}

/**
 * An actor (`agent()` site). Rendered as a lane, not a graph node. `within` is the
 * id of the nearest enclosing promoted fan-out when the `agent()` call lies inside
 * that fan-out's body: a lane FAMILY (one fresh actor per element) rather than a
 * single shared lane. Omitted for actors created outside any fan-out.
 */
export interface ActorSite {
  id: string;
  loc: SiteLoc;
  name?: string;
  /**
* Static shape when `name` is absent (the first parameter of `agent()` is not a literal) and the first parameter is a template string. with `name`
* Actual mutual exclusion: literals for names, templates for shapes. **Deliberately not included in `name`** - the contract of `name` is "as is by the author"
*The word written down", mix the reconstruction into it, and the downstream will no longer be able to tell whether what is seen is a literal or an inference.
   */
  namePattern?: NamePattern;
  within?: string;
}

/**
 * A may-flow edge: "the output of `from` may feed `to`". `data` edges carry
 * artifacts; `context` edges relate asks sharing an actor. `exact` is false once
 * any widening rule fired along the witness path. `port` records a join input /
 * element position when the join argument is a static array literal. `type` is the
 * producer's artifact type name (provenance: the value feeding `to` was computed from
 * `from`'s output), port-refined for edges OUT OF a join; absent on context edges,
 * source-completion edges, and when the type is unknowable/uninformative.
 */
export interface SiteEdge {
  from: string;
  to: string;
  kind: "data" | "context";
  exact: boolean;
  port?: number;
  type?: string;
}

export interface SiteGraph {
  nodes: SiteNode[];
  actors: ActorSite[];
  edges: SiteEdge[];
}
