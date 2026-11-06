import type { ModelSelection } from "@zcode/shared";
import { parseProviderQualifiedModelSelection } from "../app/provider-registry-selection.js";

export function formatProtocolModelSelection(ref: ModelSelection): string {
  return `${ref.providerId}/${ref.modelId}`;
}

function modelSelectionFromString(input: string): ModelSelection {
  const selection = parseProviderQualifiedModelSelection(input);
  if (!selection) throw new Error(`Invalid provider-qualified model selection: ${input}`);
  return selection;
}

/** The getModel() of unbound App returns a null value; the read protocol cannot turn the legal null state into a recovery exception. */
export function optionalModelSelectionFromString(input: string): ModelSelection | undefined {
  return input.trim() ? modelSelectionFromString(input) : undefined;
}
