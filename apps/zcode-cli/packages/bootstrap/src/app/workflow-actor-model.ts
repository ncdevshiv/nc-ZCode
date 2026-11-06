// ============================================================
// Model surface of workflow actor (run selection / journal pin → AgentRuntime model configuration)
// ============================================================
//
// persona's model level (`model?: "main" | "lite"`) has been retired. The host does not have a lite model source after the provider is refactored.
// "lite" and "main" already share the same path - inherit the current model of the parent session.
//
// So this module only answers one question: **Which model should this actor's session run on**. Three sources in order of priority:
// `subagentModel` of this run (`subagent_model` of `CreateWorkflow` / `AmendWorkflow`) > resume pin (the last model actually run in the journal) > current model of the parent session.
// It is a sister module on the same seam as workflow-actor-tools.ts: one gives the tool surface, the other gives the model surface,
// All are expanded by the runtime factory on the driver side when creating AgentRuntime.

import type { ModelSelection } from "@zcode/shared/model-selection";
import { parseProviderQualifiedModelSelection } from "./provider-registry-selection.js";

/** Host-side facts required to parse the model face. */
interface WorkflowActorModelHost {
  /**
* The parent session's **current** model selection (`runtime.getSessionModelSelection()`). Only if there is no run level selection
* Useful: Compare with pin (to determine whether the pinned model is the current main model), thereby determining whether the pin branch should be real
* coverage). Absent when the parent session has no selections.
   */
  parentSelection?: ModelSelection | undefined;
  /**
* This run's own subagent model (`subagent_model` of `CreateWorkflow` / `AmendWorkflow`, from journal
* read back in the `run-launched` event). **Entire selection**, including reasoning
* Gear - when the user says "subagent runs GLM-5.3-Flash$high", that gear is part of the selection and cannot be dropped here.
   *
* Position: **Top**. It is the user's explicit statement about this run; when present, both the pin and the parent model are just what it was intended to replace.
* Default (see function comments below). The master agent is not affected by it - it only describes subagents.
   */
  runSelection?: ModelSelection | undefined;
}

/** Model face slice of AgentRuntimeConfig. */
interface WorkflowActorModelPolicy {
  /**
* Expand into the override for AgentRuntimeConfig. **Empty object means "no coverage"**: The baseline of the child runtime is
* Model selection for parent session (script-workflow-child-runtime.ts), so nothing when there is no run selection and no pin
If * is not written, it means inheriting the parent model.
   */
  configOverrides: {
    modelSelection?: ModelSelection;
  };
}

/**
* Thrown if the pinned model cannot be constructed. Bring the pin itself: the troubleshooter needs to know which model is pinned in the journal.
* Instead of guessing from a generic "model reference is illegal" message.
 */
export class WorkflowActorPinnedModelError extends Error {
  readonly pinnedModel: string;

  constructor(pinnedModel: string, cause?: unknown) {
    super(`Cannot construct the model pinned for this subagent: ${pinnedModel}`);
    this.name = "WorkflowActorPinnedModelError";
    this.pinnedModel = pinnedModel;
    if (cause !== undefined) (this as { cause?: unknown }).cause = cause;
  }
}

/**
* Map the run selection and pin in the journal to the model configuration of AgentRuntime. Pure function.
 *
* `pinnedModel` is the `resolvedModel` (`providerId/modelId`) recorded by this actor in the journal,
* Only resume (including amend-resume seeds inherited from the predecessor) will bring it.
 *
* Priority: **This run's `subagentModel` > resume pin > parent session's current model**.
 *
* | run selection | pin | parse results |
 * |---|---|---|
* | Yes | Any (including malformation, not parsed) | Covered to run selection (whole line, including reasoning gear) |
* | None | None | Do not overwrite (parent session current model) |
* | None | = Current model of parent session | Not overwritten (pinned is the current main model) |
* | None | ≠ parent session's current model | overrides the pin parsed selection |
* | None | Malformed (missing provider segment) | {@link WorkflowActorPinnedModelError} |
 *
* **Omission means inheritance, explicit value means replacement. ** resume / amend of `resolveInput` versus `subagentModel` with
* `max_concurrency` is already this rule; pin is the same rule used on the **implicit default**: one without `subagentModel`
*'s run, the default of its sub-agent is not "the model of the parent session at this moment", but "the model that this sub-agent actually ran last time". run has
* `subagentModel`, there is no default to inherit, and pin has nothing to say. So pin is ranked below run selection - it guards against
* Silent drift, while `AmendWorkflow` with `subagent_model` is exactly that explicit, user-visible decision (confirmation window and
* The tool output all says "Subagents run on..."). Previously the pin was ranked above the run selection, resulting in each
* The continuation sub-agents with live work all run on the precursor model, and `run-launched` and the confirmation window are different.
* The history will not be lost when changing the model: the new run's own `dwf_actor` line will record the new selection, and the predecessor line will still be the old model, lineage
* Therefore, "at which run was changed" is retained.
 *
* Why pin - it's the persistence half of persona's freezing invariant: persona freezes on `agent()`,
* And resume will rebuild the actor from the journal. Without a pin, if the parent session changes the main model between runs, there will be a
* actor transcript **quietly changed a frozen identity** in the middle: the first half of the ask is produced by model X, and the one after the resume
* Produced by model Y without any mention of identity change anywhere.
 *
* **v1 pin-miss strategy (no path selected by run): would rather fail than change the model silently. ** pin points to the host
* When the model cannot be constructed (the provider is gone, the model is offline), this function **does not fall back** to the parent session model - which happens to be pin
* That silent identity change you need to guard against. Malformed pins fail here with {@link WorkflowActorPinnedModelError};
* A model with "format is legal but the host no longer has it" cannot be found when the session is created (if you want to check the host's Registry, that is
* This pure function deliberately does not introduce machines), it will surface as a node-level error on the first ask** model call - this is intentional
* Accepted: It is better to fail loudly later than to quietly change a model and continue running. To change models during resume, there is only one way:
* `AmendWorkflow` takes `subagent_model`, which is what run selects.
 *
* This function **does not produce** the fact "which model it will eventually run on": it will fall into the journal, and the authority is created by the child
* runtime itself (`runtime.getSessionModelSelection()`). Let the runtime speak for itself, and there will be no "strategy thinking"
* Selected A, and the runtime actually ran B. This is a deviation that can only occur if the two places are calculated twice. See you in the library
* `journalActorResolvedModel` of dynamic-workflow-run-launch.ts.
 */
export function workflowActorModelPolicy(
  host: WorkflowActorModelHost,
  pinnedModel?: string,
): WorkflowActorModelPolicy {
// The run selection is present: the entire line is covered, and the pin is not even parsed - it is just the default that this run will replace.
  if (host.runSelection !== undefined) {
    return { configOverrides: { modelSelection: host.runSelection } };
  }
  if (pinnedModel === undefined) return { configOverrides: {} };
  const pinned = parsePinnedModel(pinnedModel);
// What is nailed is the current model of the parent session: let the baseline of the child runtime express itself. "No coverage" is **stronger**
// Expression - Baseline inherits along with the reasoning option, while overriding by identity will replace the option with an equivalent without the options.
  if (host.parentSelection !== undefined && sameModelIdentity(pinned, host.parentSelection)) {
    return { configOverrides: {} };
  }
// The parent session changed the main model between runs. Still pinning the pin - silently changing models is exactly what pins are designed to prevent;
// Take AmendWorkflow's subagent_model (the one above).
// The reasoning option is not recalculated on this path: the pin holds the **model identity** (providerId/modelId), and only these two paragraphs are recorded in the journal.
  return { configOverrides: { modelSelection: pinned } };
}

/** Pin comparison only looks at two parts of the identity: journal only remembers `providerId/modelId`, options are not part of the identity. */
function sameModelIdentity(a: ModelSelection, b: ModelSelection): boolean {
  return a.providerId === b.providerId && a.modelId === b.modelId;
}

/**
* Parse pins in journal. **Without default provider**: pin is `providerId/modelId` written natively,
* If the provider segment is missing, it means that the record is not written in this format (or has been changed). At this time, use the provider of the parent session to fill it in.
* Equivalent to guessing a new identity - exactly what pins are designed to prevent. Rather fail loudly.
 */
function parsePinnedModel(pinnedModel: string): ModelSelection {
  const parsed = parseProviderQualifiedModelSelection(pinnedModel);
  if (parsed === undefined) throw new WorkflowActorPinnedModelError(pinnedModel);
  return parsed;
}
