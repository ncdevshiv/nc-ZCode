import { z } from "zod";

const stringRecordSchema = z.record(z.string(), z.string());
const unknownRecordSchema = z.record(z.string(), z.unknown());
const positiveNumberSchema = z.number().finite().positive();

const reasoningCapabilitySchema = z.object({
  enabled: z.boolean().optional(),
  levels: z.array(z.string().min(1)).optional(),
  defaultLevel: z.string().min(1).optional(),
  providerOptionsByLevel: z.record(z.string(), unknownRecordSchema).optional(),
});

const providerModelSchema = z
  .object({
    id: z.string().min(1).optional(),
    name: z.string().min(1).optional(),
    attachment: z.boolean().optional(),
    reasoning: z.union([z.boolean(), reasoningCapabilitySchema]).optional(),
    tool_call: z.boolean().optional(),
    structured_output: z.boolean().optional(),
    supportsImages: z.boolean().optional(),
    supportsPdf: z.boolean().optional(),
    supportsVideo: z.boolean().optional(),
    supportsToolCall: z.boolean().optional(),
    supportsJsonSchemaOutput: z.boolean().optional(),
    contextWindow: positiveNumberSchema.optional(),
    maxOutputTokens: positiveNumberSchema.optional(),
    limit: z
      .object({
        context: positiveNumberSchema.optional(),
        output: positiveNumberSchema.optional(),
      })
      .optional(),
    modalities: z
      .object({
        input: z.array(z.enum(["text", "audio", "image", "video", "pdf"])).optional(),
        output: z.array(z.enum(["text", "audio", "image", "video", "pdf"])).optional(),
      })
      .optional(),
    options: unknownRecordSchema.optional(),
    headers: stringRecordSchema.optional(),
  })
  .passthrough();

const providerSchema = z
  .object({
    kind: z.enum(["anthropic", "openai", "openai-compatible"]),
    name: z.string().min(1).optional(),
    npm: z.never().optional(),
    options: z
      .object({
// The published configuration uses an empty string to indicate that the Key has not been filled in, and the entire upgrade input cannot be rejected for this reason.
        apiKey: z.string().optional(),
        baseURL: z.string().min(1).optional(),
        apiKeyRequired: z.boolean().optional(),
        headers: stringRecordSchema.optional(),
      })
      .passthrough()
      .optional(),
    headers: stringRecordSchema.optional(),
    models: z.record(z.string(), providerModelSchema).optional(),
  })
  .passthrough();

const modelSelectionSchema = z.string().refine((value) => parseModelTarget(value) !== undefined, {
  message: "Model references must use provider/model format",
});
const legacyRootSchema = z
  .object({
    provider: z.record(z.string(), providerSchema).optional(),
    model: z
      .union([
        modelSelectionSchema,
        z
          .object({
            main: modelSelectionSchema.optional(),
            lite: modelSelectionSchema.optional(),
          })
          .strict()
          .refine((value) => value.main !== undefined || value.lite !== undefined),
      ])
      .optional(),
    small_model: z.never().optional(),
  })
  .passthrough();

type LegacyRoot = z.infer<typeof legacyRootSchema>;
export type LegacyCliProvider = NonNullable<LegacyRoot["provider"]>[string];

export interface LegacyCliModelConfigProjection {
  readonly model?: {
    readonly main?: { readonly provider: string; readonly model: string };
    readonly lite?: { readonly provider: string; readonly model: string };
  };
  readonly provider?: Readonly<Record<string, LegacyCliProvider>>;
}

/** Published minimum private parsing bounds for legacy CLI JSON; results can only be imported into the current Config immediately. */
export function parseLegacyCliModelConfig(value: unknown): LegacyCliModelConfigProjection {
  const parsed = legacyRootSchema.parse(value);
  const rawModel = parsed.model;
  const main =
    typeof rawModel === "string" ? parseModelTarget(rawModel) : parseModelTarget(rawModel?.main);
  const lite = typeof rawModel === "object" ? parseModelTarget(rawModel?.lite) : undefined;
  const model = main || lite ? { ...(main ? { main } : {}), ...(lite ? { lite } : {}) } : undefined;
  return {
    ...(model ? { model } : {}),
    ...(parsed.provider ? { provider: parsed.provider } : {}),
  };
}

function parseModelTarget(value: string | undefined) {
  if (!value) return undefined;
  const separator = value.indexOf("/");
  if (separator <= 0 || separator === value.length - 1) return undefined;
  const provider = value.slice(0, separator).trim();
  const model = value.slice(separator + 1).trim();
  if (!provider || !model) return undefined;
  return {
    provider,
    model,
  };
}
