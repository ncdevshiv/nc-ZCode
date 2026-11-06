// ============================================================
// Parsing of model reference (string of `subagent_model` → one-time selection)
// ============================================================
//
// Pure function, zero I/O: host facts passed in via `ModelCatalogPort` (contracts'
// `interfaces/model-catalog.port.ts`), the parsing itself remains in core. The reason for separation is **measurable**——
// Three-level matching, case, gear verification and "What to say when you can't figure it out" are the only areas in this set that will be repeatedly changed.
// Once they are mixed with the port implementation, they can only be fixed by integration testing.
//
// There is only one call point: `CreateWorkflow` / `resolveInput` of `AmendWorkflow`. Parsing must occur in
// Confirmation window **before** - the window displays the model that will take effect, and the window should not be opened at all for calls that cannot be solved.

import type { ModelCatalogEntry, ModelSelection } from "@zcode/contracts";
import {
  ZCODE_MODEL_REASONING_SEPARATOR,
  formatModelPickerValue,
  parseModelPickerValue,
} from "@zcode/shared/model-selection";

/**
* Analysis results. Failure will always bring `candidates`: after the model guesses a name incorrectly, the most useful next step information is "Then here
*", otherwise it will just change the spelling and guess again.
 */
type ModelReferenceResolution =
  | { ok: true; selection: ModelSelection; entry: ModelCatalogEntry; canonical: string }
  | {
      ok: false;
      reason: "not_found" | "ambiguous" | "disabled" | "reasoning_level_unknown";
      message: string;
      candidates: ModelCatalogEntry[];
    };

/**
* `not_found` The upper bound on the number of rows in the list. A directory with dozens of providers can list hundreds of lines, and this copy is
* Directly enter the model context - for the excess part, replace it with "Add the rest to ListModels". That is the way to get the full amount.
* Truncation is not.
 */
const MODEL_REFERENCE_CANDIDATE_LINES = 40;

/**
* Parse the model name mentioned by the user into a selection. Three levels, from high to low specificity (the order is part of the specification):
 *
* 1. `providerId/modelId` full name accurate hit;
* 2. The bare `modelId` happens to be hung under only one provider;
* 3. Naked `modelId` is hung under multiple providers → among them, take the one with **current session**, otherwise `ambiguous`.
 *
* All comparisons are case-insensitive: the registry does not give `modelId` different display names, so there is no "press label to find another round"
* This file - the only difference between the user's name and the id is the case and provider prefix.
 *
* Disabled entries **never hit silently**: only return `disabled` with respective reasons when matching disabled entries. Skip quietly
* If it selects another model, the user will get a model he does not want; if it is selected quietly, it will not explode until the sub-agent speaks for the first time.
 *
* The presence of `$level` must be a legal level of the model (otherwise `reasoning_level_unknown`); if absent, the registry default file will be used
* (Models without gears do not have options). `canonical` is the picker form, using the registry's own spelling - all the way to
* It is the `run-launched` event of journal, the two reading pages and the confirmation window.
 */
export function resolveModelReference(
  text: string,
  entries: ModelCatalogEntry[],
): ModelReferenceResolution {
  const { reference, level } = splitReasoningLevel(text.trim());
  const matches = matchEntries(reference, entries);
  if (matches.length === 0) return notFoundResolution(text, entries);

  const enabled = matches.filter((entry) => entry.disabledReason === undefined);
  if (enabled.length === 0) return disabledResolution(text, matches);

// 3rd gear. The full name also goes here: if the same provider/model appears twice in the directory, it is a problem with the host. The first one is picked silently.
// It will leave no answer to "Which one should I choose?"
  const entry = enabled.length === 1 ? enabled[0]! : enabled.find((candidate) => candidate.current);
  if (entry === undefined) return ambiguousResolution(text, enabled);

  const options = resolveReasoningOptions(entry, level);
  if (options === undefined) {
// `resolveReasoningOptions` only returns undefined when the gear is given and does not match, so the level here must be present.
    return reasoningLevelUnknownResolution(text, entry, level ?? "");
  }

  const selection: ModelSelection = {
// The registry's spelling, not the user's: `canonical` needs to be backfilled verbatim into the next call.
    providerId: entry.providerId,
    modelId: entry.modelId,
    ...options,
  };
  return { ok: true, selection, entry, canonical: formatModelPickerValue(selection) };
}

/**
* Normalized `subagent_model` → structured selection. **Only used in handler**: The string that goes there has passed
* The analysis of `resolveInput`, so if it cannot be solved, it may be that someone has bypassed the normalization - that is a wiring fault, press the wiring fault to shout out,
* Instead of silently discarding the model that the user wants (the subagent will run on the session model quietly, no one can see it).
 */
export function parseWorkflowSubagentModel(canonical: string | undefined): ModelSelection | undefined {
  if (canonical === undefined) return undefined;
  try {
    return parseModelPickerValue(canonical);
  } catch (cause) {
    throw new Error(
      `workflow subagent_model reached the handler un-canonicalised: ${canonical}`,
      { cause },
    );
  }
}

/**
* A sentence in the result copy of the effective subagent model (shared by `CreateWorkflow` and `AmendWorkflow`, the shape is the same
* `describeWorkflowConcurrencyLimit`). **Only appears when the model is set**: run on the session model does not
* It can be said that one more sentence will only make the model think that it has chosen something.
 *
* The half sentence in brackets is for the model itself: it is easiest to read "the sub-agent changed the model" as "I also changed it", and then in the next round
* Rehearse a fake current model to the user.
 */
export function describeWorkflowSubagentModel(canonical: string | undefined): string {
  if (canonical === undefined) return "";
  return ` Subagents run on ${canonical} (the main agent stays on the session model).`;
}

/** The canonical id of the directory entry (without stall): the `id` of `ListModels` is the same shape as the row in the failed list. */
export function formatModelCatalogId(entry: ModelCatalogEntry): string {
  return `${entry.providerId}/${entry.modelId}`;
}

/**
* Cut out `$level`. The search starting point follows the provider separator (same as `parseModelPickerValue`): provider id
* There will be no `$` in it, but using the entire string as a model name will cause deformed strings such as `a$b/c` to be cut at the wrong position.
 */
function splitReasoningLevel(text: string): { reference: string; level?: string } {
  const providerSeparatorIndex = text.indexOf("/");
  const searchFrom = providerSeparatorIndex + 1;
  const index = text.indexOf(ZCODE_MODEL_REASONING_SEPARATOR, searchFrom);
// The empty side (`$high`, `glm$`) doesn't count as a gear: that's a misspelling, making it fall to not_found to list.
  if (index <= searchFrom || index >= text.length - 1) return { reference: text };
  return { reference: text.slice(0, index), level: text.slice(index + 1) };
}

/** Candidate sets for the 1st and 2nd levels: with `/` by full name, without by bare `modelId`. */
function matchEntries(reference: string, entries: ModelCatalogEntry[]): ModelCatalogEntry[] {
  const providerSeparatorIndex = reference.indexOf("/");
  if (providerSeparatorIndex > 0) {
    const providerId = reference.slice(0, providerSeparatorIndex);
    const modelId = reference.slice(providerSeparatorIndex + 1);
    return entries.filter(
      (entry) => sameToken(entry.providerId, providerId) && sameToken(entry.modelId, modelId),
    );
  }
  return entries.filter((entry) => sameToken(entry.modelId, reference));
}

/**
* Gear selection. Returning `undefined` means that the given level is illegal (the call point returns `reasoning_level_unknown` accordingly);
* Returning `{}` means that this model does not have a gear - note that this is not the same as "the gear is an empty string", so it cannot be merged with a null value.
 */
function resolveReasoningOptions(
  entry: ModelCatalogEntry,
  level: string | undefined,
): { options?: { reasoningLevel: string } } | undefined {
  if (level !== undefined) {
// The registry's own spelling wins: the `HIGH` typed by the user must be changed to `high` in the directory, otherwise the canonical backfill will be deformed once.
    const matched = entry.reasoningLevels.find((candidate) => sameToken(candidate, level));
    return matched === undefined ? undefined : { options: { reasoningLevel: matched } };
  }
  if (entry.reasoningLevels.length === 0 || entry.defaultReasoningLevel === undefined) return {};
  return { options: { reasoningLevel: entry.defaultReasoningLevel } };
}

/**
* Not found: List **optional** ids. Disabled items are not included in the list - let the model choose from a list of items that it cannot use,
* Will only lead to a second failure. Mark the entry for the current session with `[current]`: it is the equivalent of "set nothing", mark it
* The model only knows that choosing it means not choosing it.
 */
function notFoundResolution(text: string, entries: ModelCatalogEntry[]): ModelReferenceResolution {
  const candidates = entries.filter((entry) => entry.disabledReason === undefined);
  const shown = candidates.slice(0, MODEL_REFERENCE_CANDIDATE_LINES);
  const overflow = candidates.length - shown.length;
  const lines = shown.map(
    (entry) => `${formatModelCatalogId(entry)}${entry.current ? " [current]" : ""}`,
  );
  if (overflow > 0) lines.push(`… and ${overflow} more.`);
  const body =
    candidates.length === 0
      ? "No models are configured on this host."
      : ["Available models:", ...lines].join("\n");
  return {
    ok: false,
    reason: "not_found",
    message: `No configured model matches \`${text}\`. ${body}\n\nPass one of these ids, or call ListModels.`,
    candidates,
  };
}

/** 同名挂在多个 provider 下，且没有一条是当前会话的：只能让调用方写全称。 */
function ambiguousResolution(
  text: string,
  candidates: ModelCatalogEntry[],
): ModelReferenceResolution {
  const lines = candidates.map((entry) => formatModelCatalogId(entry));
  return {
    ok: false,
    reason: "ambiguous",
    message: `\`${text}\` is configured under more than one provider:\n${lines.join("\n")}\n\nPass the full \`providerId/modelId\` of the one you want.`,
    candidates,
  };
}

/** 只匹配到禁用条目：连理由一起说出来，否则用户看到的是「这个模型不存在」而它明明在列表里。 */
function disabledResolution(
  text: string,
  candidates: ModelCatalogEntry[],
): ModelReferenceResolution {
  const lines = candidates.map(
    (entry) => `${formatModelCatalogId(entry)} — ${entry.disabledReason}`,
  );
  return {
    ok: false,
    reason: "disabled",
    message: `\`${text}\` matches a model that cannot be used on this host:\n${lines.join("\n")}\n\nResolve that with the user, or call ListModels and pick another id.`,
    candidates,
  };
}

/** 档位不合法：模型认对了，只是档位打错——所以清单给的是**这个模型的**档位，不是整张目录。 */
function reasoningLevelUnknownResolution(
  text: string,
  entry: ModelCatalogEntry,
  level: string,
): ModelReferenceResolution {
  const id = formatModelCatalogId(entry);
  const levels =
    entry.reasoningLevels.length === 0
      ? `${id} has no reasoning levels — drop the \`$\` suffix.`
      : `Its levels are: ${entry.reasoningLevels.join(", ")}.${
          entry.defaultReasoningLevel === undefined
            ? ""
            : ` Omit the suffix to use ${entry.defaultReasoningLevel}.`
        }`;
  return {
    ok: false,
    reason: "reasoning_level_unknown",
    message: `\`${level}\` is not a reasoning level of ${id}. ${levels}`,
    candidates: [entry],
  };
}

/** id 的比较口径：两端去空白后大小写不敏感。注册表里 id 是标识符，不是自由文本。 */
function sameToken(left: string, right: string): boolean {
  return left.trim().toLowerCase() === right.trim().toLowerCase();
}
