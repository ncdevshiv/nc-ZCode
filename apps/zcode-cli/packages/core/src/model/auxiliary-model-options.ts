import type { Model, ModelOptions } from "@zcode/contracts";

const AUXILIARY_MAX_OUTPUT_TOKENS = 5_000;

/**
* Auxiliary calls uniformly use the lowest item of the public gear and limit the output budget; the gear order comes from Model Config,
* Protocol behavior can no longer be inferred by names such as disabled/off.
 */
export function auxiliaryModelOptions(model: Model): Required<ModelOptions> {
  return {
    reasoningLevel: model.optionSpecs.reasoningLevel.values[0]!,
    maxOutputTokens: Math.min(AUXILIARY_MAX_OUTPUT_TOKENS, model.optionSpecs.maxOutputTokens.max),
  };
}
