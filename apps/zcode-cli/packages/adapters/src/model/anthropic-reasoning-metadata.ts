import type { ModelReasoningContentBlock } from "@zcode/contracts";

type ReasoningTransformOptions = {
  providerKind?: "openai" | "anthropic" | "openai-compatible" | "gateway" | "custom";
};
type ReasoningProviderOptions =
  | { providerOptions: Record<string, unknown> }
  | Record<string, never>;

export function providerOptionsForReasoningBlock(
  block: ModelReasoningContentBlock,
  options: ReasoningTransformOptions,
): ReasoningProviderOptions {
  const providerOptions = objectProviderOptions(block.providerOptions);
  if (options.providerKind !== "anthropic") {
    return block.providerOptions ? { providerOptions: block.providerOptions } : {};
  }

  if (hasAnthropicReasoningMetadata(block.providerOptions)) {
    return { providerOptions: block.providerOptions };
  }

// Directly equating "no signature" with "incompatible" will make it the same as in the model history
// unsigned thinking is silently deleted before provider serialization. Cross-model, orphan, and tail cleanup are performed at the request level
// History normalization decision; here the empty signature supported by the serializer is used to represent the reserved unsigned block.
  return {
    providerOptions: {
      ...providerOptions,
      anthropic: {
        ...anthropicProviderOptions(block.providerOptions),
        signature: "",
      },
    },
  };
}

function hasAnthropicReasoningMetadata(
  providerOptions: unknown,
): providerOptions is Record<string, unknown> {
  const anthropicOptions = anthropicProviderOptions(providerOptions);
  return (
    typeof anthropicOptions.signature === "string" ||
    typeof anthropicOptions.redactedData === "string"
  );
}

function objectProviderOptions(providerOptions: unknown): Record<string, unknown> {
  if (!providerOptions || typeof providerOptions !== "object" || Array.isArray(providerOptions)) {
    return {};
  }

  return providerOptions as Record<string, unknown>;
}

function anthropicProviderOptions(providerOptions: unknown): Record<string, unknown> {
  if (!providerOptions || typeof providerOptions !== "object" || Array.isArray(providerOptions)) {
    return {};
  }

  const anthropicOptions = (providerOptions as Record<string, unknown>).anthropic;
  if (
    !anthropicOptions ||
    typeof anthropicOptions !== "object" ||
    Array.isArray(anthropicOptions)
  ) {
    return {};
  }

  return anthropicOptions as Record<string, unknown>;
}
