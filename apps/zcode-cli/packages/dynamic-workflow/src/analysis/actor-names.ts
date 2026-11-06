// @ts-nocheck
/**
* Compilation time courtesy diagnostics for named actor duplicates.
 *
* The rule itself is **runtime**: the engine checks for duplicates of non-empty valid names in createActor, and if it encounters a run-level failure
* (`DuplicateActorName`). The reason is that the named actor is the identity key imported by the amend-resume cache.
* And any run is a potential precursor for future revisions - duplicate names in the precursor will cause import matching ambiguity.
 *
* Here we do **the half that the literal can see through**, two clauses:
 *
* 1. **Two places with the same name**: When the effective names of the two `agent(...)` sites are the same string literal, one can be reported in the latter place.
* Positioning diagnostics, allowing authors to change names on the cheap side instead of failing halfway through. any dynamic component
* (`` `worker-${i}` ``, identifier, expansion, shorthand attributes) are all skipped - those can only be checked at runtime.
* 2. **Static name in fan-out**: When an `agent("x")` site runs in the body of fan-out, a new one is created for each element.
* actor and they share the same name - a site with the same name N times will inevitably fail loudly during runtime.
 *
* Clause 1 **Only false negatives, no false positives**: There is no cost in skipping a site that may have the same name (the engine takes the guesswork), but accidentally hurting one
* Legal scripts will block submissions.
 *
* The collection of `.map` may only have ≤1
* element, such a script is actually legal during runtime, so this clause will theoretically give a false positive. The reason for accepting it is that the costs are asymmetric -
* The repair is free (change to element-by-element name `` `x-${item}` `` or anonymously), but the cost of not reporting it is that the model burns an entire run
* Just learned this rule. `paths.map((p) => agent("reviewer").ask(…))` is exactly the textbook way of writing fan-out,
* Silently left to runtime is the worst level.
 *
* Determination of fan-out ** Classification of reused site tables ** (`IterationCandidate` of `analysis/sites.ts`: with inline
* Callback array method + `for...of`), do not create a new set of loop recognition by yourself. The cost is ordinary `for` / `while` /
* `do` loops are not in that category, and thus are not covered by this clause - they fall back to the old behavior of clause 1 (invisible, returned to runtime).
* That is the direction of false negatives, consistent with the conservative orientation of this module.
 *
* `CreateWorkflow` with
* Both handlers of `SaveWorkflow` go to `analyzeScript` first (this diagnosis is in `analyzeWorkflowScript`
* In the authoring batch), `!ok` means early return with diagnosis, **not submit / not placed**, so a 9005/9006 is indeed
* Block submission and saving. But run service's `compileOnce` (recompilation of submit and resume) only re-checks
* typecheck / schema / world.run literal three items, **not including** this diagnosis - so it is not on the submit path
* Forced gates, don't think of them as load-bearing structures: the authority for named uniqueness is always the runtime duplication check of the engine createActor.
 */

import ts from "../compiler/ts-shim.js";
import type { CompileDiagnostic, WorkflowProgram } from "../compiler/compile.js";
import type { SiteTable } from "./sites.js";

/** Two literals have the same name (clause 1). 9001 = facade-sitting, 9002 = schema, 9003 = world-run, 9004 = phase. */
const DUPLICATE_ACTOR_NAME_CODE = 9005;

/**
* Static name in fan-out body (clause 2), separate code instead of multiplexing 9005.
 *
* The two clauses are the same rule, but the **certainty is different**: 9005 only appears when the duplicate name is statically determined, 9006 is the deliberate one above
* Clause that accepts false positives. The reader therefore needs to be able to distinguish between them - specifically the analysis-side fixture corpus:
* Those scripts are never executed and are only used to pin shapes, where the combination of static name + fan-out is exactly what the parser must handle correctly **
* shape (`ActorNode.family`, actor tag), so the corpus should be exempted from 9006 and remain strict with 9005.
* Sharing the same code can only be distinguished by matching the message text, and shunting based on error text is prohibited everywhere in this warehouse.
 */
export const FANOUT_ACTOR_NAME_CODE = 9006;

/**
* Collect literal name duplicate diagnosis. If not empty, the script cannot be submitted (`analyzeWorkflowScript` and world.run/phase
*'s compile-time rules).
 */
export function collectDuplicateActorNames(
  workflow: WorkflowProgram,
  table: SiteTable,
): CompileDiagnostic[] {
  const diagnostics: CompileDiagnostic[] = [];
  const claimed = new Map<string, ts.CallExpression>();
// The collection of fan-out bodies comes from the site table's IterationCandidate (array method's inline callback body + for...of's
// Loop body), which uses the same concept as `ActorNode.fanOutId` to determine "whether the site falls within the fan-out body".
  const fanOutBodies = new Set<ts.Node>(table.iterations.map((iteration) => iteration.body));
  for (const site of table.actors) {
// The site table's `site.name` is not used here: it falls back to the binding name (`const planner = agent()`
// Marked as "planner"), that is the display label used for the diagram, not a valid name at runtime. What's important here is that the engine will see it.
// That value, so read it from the actual parameter.
    const name = staticEffectiveName(site.call);
    if (name === undefined || name === "") continue;
    const first = claimed.get(name);
    if (first === undefined) claimed.set(name, site.call);
    const loc = workflow.toScriptLoc(site.call.getStart(workflow.scriptFile));
// A site can report at most one: the fan-out clause is more specific (it even has a different revision - by element name instead of changing the name),
// So it takes precedence. The name is still registered as usual, and subsequent sites with the same name should be reported again.
    if (isInsideFanOut(site.call, fanOutBodies)) {
      diagnostics.push({
        code: FANOUT_ACTOR_NAME_CODE,
        column: loc.column,
        line: loc.line,
        message:
          `this agent(...) runs once per element of a fan-out but its name "${name}" is a fixed ` +
          `string, so every element creates a different actor under the same name — the run ` +
          `fails with DuplicateActorName as soon as the collection holds more than one item. ` +
          `An actor name must be unique within a run: it is the identity key an amended re-run ` +
          `matches its imported cache against. Build a per-element name (\`${name}-\${item}\`), ` +
          `or drop the name — anonymous actors are legal (they just never reuse cached work).`,
      });
      continue;
    }
    if (first === undefined) continue;
// The diagnosis falls in the last place: the name that appears first is the existing fact, and it is the later one that needs to be changed.
    const firstLoc = workflow.toScriptLoc(first.getStart(workflow.scriptFile));
    diagnostics.push({
      code: DUPLICATE_ACTOR_NAME_CODE,
      column: loc.column,
      line: loc.line,
      message:
        `two actors are named "${name}" (the first is on line ${firstLoc.line}): an actor name ` +
        `must be unique within a run. The name is the identity key an amended re-run matches ` +
        `its imported cache against, and any run can become the predecessor of one, so a ` +
        `repeated name makes that match ambiguous. Give this one its own name, or drop the ` +
        `name entirely — anonymous actors are legal (they just never reuse cached work).`,
    });
  }
  return diagnostics;
}

/**
* Whether the call lexically falls within a fan-out body. Follow the parent chain starting from the call itself (the call can be the body itself:
* The arrow abbreviation of `paths.map((p) => agent("x"))`), whichever candidate is hit.
 */
function isInsideFanOut(call: ts.CallExpression, fanOutBodies: ReadonlySet<ts.Node>): boolean {
  for (let node: ts.Node | undefined = call; node !== undefined; node = node.parent) {
    if (fanOutBodies.has(node)) return true;
  }
  return false;
}

/**
* An `agent(...)` call with a statically known **valid name**, otherwise undefined (= skipped).
 *
* The same rule as the engine's `normalizePersona`: persona.name trumps the name argument; the string persona is
* system prompt, without name. If you can't see through any part of it, give up on it as a whole - rather miss it.
 *
* facade's `AgentPersona` **name is not declared today**, so the persona written in the script never has a name and is a valid name
* always falls into the name actual parameter. The persona section therefore never fires now; it is retained to synchronize with normalizePersona,
* Instead of taking effect today - once the facade has a name and it is still judged based on the name parameter, one has been renamed by persona
*'s actors will be **false positive**, and false positives will block legitimate scripts (there is no price for false positives: the engine is running at a loss).
 */
function staticEffectiveName(call: ts.CallExpression): string | undefined {
  const [nameArg, personaArg] = call.arguments;
  if (personaArg !== undefined && !ts.isStringLiteralLike(personaArg)) {
    if (!ts.isObjectLiteralExpression(personaArg)) return undefined; // 动态 persona：可能带 name
    for (const property of personaArg.properties) {
// Expansion may bring in a name, but static is not sure.
      if (ts.isSpreadAssignment(property)) return undefined;
// The calculated key (`{ ["na" + "me"]: … }`) is also uncertain: it could be name. Here we must give up the whole
// Not `continue` - continue will fall back to the name actual parameter, which is equivalent to asserting that persona has not been renamed, and that is once
// Guess, and guessing in the wrong direction happens to be a false positive (blocking legitimate scripts).
      if (property.name !== undefined && ts.isComputedPropertyName(property.name)) return undefined;
      if (property.name === undefined || !isNameKey(property.name)) continue;
      if (!ts.isPropertyAssignment(property)) return undefined; // shorthand / 方法：动态
      return ts.isStringLiteralLike(property.initializer) ? property.initializer.text : undefined;
    }
// There is no name in the object literal, fall back to the name actual parameter.
  }
  return nameArg !== undefined && ts.isStringLiteralLike(nameArg) ? nameArg.text : undefined;
}

/** Whether the attribute key is **statically determined** is `name` (the calculated key has been completely abandoned at the call point, so don’t go here). */
function isNameKey(key: ts.PropertyName): boolean {
  return (ts.isIdentifier(key) || ts.isStringLiteralLike(key)) && key.text === "name";
}
