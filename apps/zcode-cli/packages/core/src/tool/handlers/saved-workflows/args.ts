// ============================================================
// Saved workflows - parameter verification
// ============================================================
//
// Verification occurs before the **confirmation window**: if the parameters are passed incorrectly, there is nothing worthy of user judgment. Popping up a window destined to fail is just
// Use an invalid decision to interrupt the model's own error correction loop. This is the same as CreateWorkflow's handling of scripts that cannot be compiled.
// Principle (`prepareApproval` comment in create-workflow.ts).

import type { SavedWorkflowArgDeclaration, SavedWorkflowArgsDeclaration } from "@zcode/contracts";

export type WorkflowArgsValidation =
  | { ok: true; args: Record<string, unknown> }
  | { ok: false; errors: string[] };

/**
* Verify the parameters passed by the caller according to the declaration and complete the default values.
 *
* Collect **all** violations and return them at once, instead of exiting after hitting the first one: the model gets "less PR, and one more passed
* prNumber" can be changed once. If you get "missing pr", you will change it once and hit it again.
 *
* It is an error for a workflow that does not declare args to receive any parameters - it cannot read them, and silently discarding them will make the caller think that the parameters are
* is effective.
 */
export function validateWorkflowArgs(
  declaration: SavedWorkflowArgsDeclaration | undefined,
  provided: Record<string, unknown> | undefined,
): WorkflowArgsValidation {
  const declared = declaration ?? {};
  const given = provided ?? {};
  const errors: string[] = [];
  const args: Record<string, unknown> = {};

  const declaredNames = Object.keys(declared);
  for (const key of Object.keys(given)) {
    if (declared[key] !== undefined) continue;
    errors.push(
      declaredNames.length === 0
        ? `unknown argument '${key}': this workflow declares no arguments`
        : `unknown argument '${key}' (declared: ${declaredNames.join(", ")})`,
    );
  }

  for (const [key, spec] of Object.entries(declared)) {
    const supplied = given[key];
// Default and explicit undefined are treated the same: undefined cannot be passed in JSON, so the two can only be the same thing.
    if (supplied === undefined) {
      if (spec.default !== undefined) {
// The default value and the incoming value go through the same type check: a parameter is declared as number but is written as "3" by default.
// The mistake is at the moment of saving, we should not wait until the script reads it before exploding.
        const failure = typeMismatch(key, spec, spec.default, "default value");
        if (failure === undefined) args[key] = spec.default;
        else errors.push(failure);
        continue;
      }
      if (spec.required === true) errors.push(`missing required argument '${key}'`);
      continue;
    }

    const failure = typeMismatch(key, spec, supplied, "value");
    if (failure === undefined) args[key] = supplied;
    else errors.push(failure);
  }

  return errors.length > 0 ? { ok: false, errors } : { ok: true, args };
}

/**
* Description when the types do not match, `undefined` when they match.
 *
* `json` accepts everything - it means exactly "no checking here", so even null is a legal json value. three primitive buttons
* typeof judgment; `number` additionally rejects NaN and Infinity, because they cannot pass through JSON to the sandbox, and releasing them will only
* A readable error is moved into the script and becomes `null`.
 */
function typeMismatch(
  key: string,
  spec: SavedWorkflowArgDeclaration,
  value: unknown,
  what: string,
): string | undefined {
  const describe = (expected: string): string =>
    `argument '${key}': expected ${expected}, got ${describeValue(value)} (${what})`;

  switch (spec.type) {
    case "string":
      return typeof value === "string" ? undefined : describe("a string");
    case "number":
      return typeof value === "number" && Number.isFinite(value)
        ? undefined
        : describe("a finite number");
    case "boolean":
      return typeof value === "boolean" ? undefined : describe("a boolean");
    case "json":
      return undefined;
  }
}

function describeValue(value: unknown): string {
  if (value === null) return "null";
  if (Array.isArray(value)) return "an array";
  return `a ${typeof value}`;
}
