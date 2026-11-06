import { createCoreError, CoreErrorType, type ModelSelection } from "../deps.js";
import { cloneModelSelection } from "../model-selection.js";
import type { EffectiveModelSelectionResult } from "@zcode/shared/model-selection";

const SUBAGENT_SELECTION_MESSAGES = {
  "selection-missing": "No model selected / No model selected",
  "account-connection-unavailable": "Account connection unavailable / The current account connection is unavailable",
  "provider-not-found": "Provider unavailable / The provider does not exist or is unavailable",
  "model-not-found": "Model unavailable / The model does not exist or is not available",
  "reasoning-level-missing": "No reasoning level selected / No thinking level selected",
  "reasoning-level-not-supported": "Reasoning level unsupported / The selected thinking level is not supported",
} satisfies Record<NonNullable<EffectiveModelSelectionResult["selectionIssue"]>, string>;

/** Explicit profile is the intent to be parsed; inheritance and internal override have been executed and will not be re-corresponded to the account. */
export function resolveSubagentSelection(input: {
  profileSelection?: ModelSelection | null;
  parentSelection?: ModelSelection | null;
  overrideSelection?: ModelSelection;
  resolveSelection?: (selection: ModelSelection) => EffectiveModelSelectionResult;
}): { hasConcreteModel: boolean; selection: ModelSelection } {
  const explicit = input.profileSelection;
  const result: EffectiveModelSelectionResult = input.overrideSelection
    ? { effectiveSelection: input.overrideSelection }
    : explicit
      ? input.resolveSelection
        ? input.resolveSelection(cloneModelSelection(explicit))
        : { effectiveSelection: explicit }
      : { effectiveSelection: input.parentSelection ?? null };
  if (!result.effectiveSelection || result.selectionIssue) {
    const reason = result.selectionIssue ?? "selection-missing";
    const requested = input.overrideSelection ?? explicit ?? input.parentSelection;
    const identity = requested ? `; selection=${requested.providerId}/${requested.modelId}` : "";
// Parsing failure cannot fall back to the parent model, otherwise the subtask model explicitly specified by the user will be quietly changed.
// Public error projection does not read structured fields (the consumer cannot see it when there is only selectionIssue); only message is retained in the background.
// Therefore, the reason is added to the existing reason and message at the same time. Both consumption paths can be located without adding a dedicated error protocol.
    throw createCoreError(
      CoreErrorType.ConfigurationError,
      `Cannot start subagent: ${SUBAGENT_SELECTION_MESSAGES[reason]} [reason=${reason}${identity}]`,
      {
        recoverable: true,
        context: {
          selectionIssue: reason,
          reason,
          ...(result.effectiveSelection
            ? {
                providerId: result.effectiveSelection.providerId,
                modelId: result.effectiveSelection.modelId,
              }
            : {}),
        },
      },
    );
  }
  return {
    hasConcreteModel: explicit != null,
    selection: cloneModelSelection(result.effectiveSelection),
  };
}
