// @ts-nocheck
import type { SiteGraph } from "../analysis/types.js";
import { interpret } from "../analysis/interpret.js";
import { projectSiteGraph } from "../analysis/graph.js";
import type { SiteTable } from "../analysis/sites.js";
import type { WorkflowProgram } from "../compiler/compile.js";
import type { AskSpec } from "../engine/types.js";
import { canonicalJson } from "../engine/hash.js";
import type { JsonSchema } from "./types.js";

/**
* **submit profile** for each actor site: determines which `submit_result` tool the actor's subagent session gets.
 *
* - `untyped`: all asks that can fall to this actor are untyped (or it has no ask at all) → do not register submit_result.
* - `mono`: schema **all the same** for typed ask that can fall to this actor (canonical JSON is equal; untyped ask can
* Mixed in) → The tool declaration is `{ result: schema }`, which is frozen for the actor and unchanged across ask, so it is cache neutral.
* - `generic`: more than one schema for typed ask → General tool today + schema endnotes for every ask.
 *
* Why is it during compilation and not during runtime: the tool block is rendered at the front of the prompt, and once you press ask to change the schema, the actor will be destroyed.
* for the entire cache prefix. There is only one thing: "the schema remains unchanged throughout the actor life cycle"
* The typed tool is free only when it can be determined at compile time; if it cannot be determined, it returns to generic, and the behavior remains equivalent byte by byte.
 */
export type ActorSubmitProfile =
  | { kind: "untyped" }
  | { kind: "mono"; schema: JsonSchema }
  | { kind: "generic" };

/** Absent/Unable to determine profile: Today's behavior. */
export const GENERIC_SUBMIT_PROFILE: ActorSubmitProfile = { kind: "generic" };

/**
* Derive the submit profile of each actor site from the site map and ask specifications. Pure function.
 *
* **Reliability Rules**: The binding of ask→actor takes the may-set (`SiteNode.actors`) on the site map. As long as there is **anyone**
* The actor set of the ask site is empty (the receiver is not parsed), and the analysis cannot tell who the ask will fall on - so
* **All** actors are marked `generic`. This is intentionally conservative: if a typed ask falls to an `untyped` subagent,
* It has no tools to submit, so it can only exhaust nudge and fail; it would be better to save less cache than to remove the tool with an incomplete picture.
*Similarly, when an ask actor set has multiple members (receivers on conditional branches), its schema is included in each member.
 *
* askSpecs will also be returned to generic as a whole when a certain ask site is missing: the site table and specifications are from the same compilation, and the absence can only be
* Wiring error, no guessing here (engine side hard-fails this with MissingAskSpec, just don't amplify it here).
 *
* Returns a table covering every actor site in `graph.actors`.
 */
export function deriveActorSubmitProfiles(
  graph: SiteGraph,
  askSpecs: ReadonlyMap<string, AskSpec>,
): Map<string, ActorSubmitProfile> {
  const profiles = new Map<string, ActorSubmitProfile>();
  const actorIds = graph.actors.map((actor) => actor.id);

// The typed schema collected by each actor is deduplicated according to the standard JSON (the same schema object is synthesized on different ask sites)
// Once, the references are different but the content is the same and must be compared by content).
  const schemasByActor = new Map<string, Map<string, JsonSchema>>();
  for (const id of actorIds) schemasByActor.set(id, new Map());

  for (const node of graph.nodes) {
    if (node.kind !== "ask") continue;
    const actors = node.actors ?? [];
    const spec = askSpecs.get(node.id);
    if (actors.length === 0 || spec === undefined) {
      for (const id of actorIds) profiles.set(id, GENERIC_SUBMIT_PROFILE);
      return profiles;
    }
    if (!spec.typed) continue;
    const schema = spec.schema as JsonSchema;
    const key = canonicalJson(schema);
    for (const actorId of actors) {
//The actor id in the site graph must be in graph.actors; fill a bucket defensively instead of silently skipping it.
      let bucket = schemasByActor.get(actorId);
      if (bucket === undefined) {
        bucket = new Map();
        schemasByActor.set(actorId, bucket);
      }
      bucket.set(key, schema);
    }
  }

  for (const [actorId, bucket] of schemasByActor) {
    if (bucket.size === 0) profiles.set(actorId, { kind: "untyped" });
    else if (bucket.size === 1)
      profiles.set(actorId, { kind: "mono", schema: [...bucket.values()][0]! });
    else profiles.set(actorId, GENERIC_SUBMIT_PROFILE);
  }
  return profiles;
}

/**
* Convenient entry point for "compile once": explain on the same {@link WorkflowProgram} that builds the site table and synthesizes the schema
* Project the site map (analyzeWorkflowScript runs these two steps), and then derive the profile. For run submission path call,
* No need to assemble interpret + projectSiteGraph yourself (both are not on the public side of the package).
 */
export function deriveActorSubmitProfilesFor(
  workflow: WorkflowProgram,
  table: SiteTable,
  askSpecs: ReadonlyMap<string, AskSpec>,
): Map<string, ActorSubmitProfile> {
  return deriveActorSubmitProfiles(projectSiteGraph(interpret(workflow, table)), askSpecs);
}
