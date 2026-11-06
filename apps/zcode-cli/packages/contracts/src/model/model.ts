import type {
  JsonSchema,
  ModelInputMessage,
  ModelId,
  ModelProviderId,
  ModelStreamEvent,
  ModelTextResult,
  ModelToolContract,
} from "./index.js";
import { modelSelectionSchema, type ModelSelection } from "@zcode/shared/model-selection";
import type { ModelPropertiesData, ModelOptionSpecsData } from "@zcode/shared/model-config";

export type { ModelSelection } from "@zcode/shared/model-selection";

// Only keep the CLI public type name, the fields come from the same data schema, and do not copy the Provider configuration definition.
export type {
  ModelInputFormatData as ModelInputFormat,
  ModelOutputFormatData as ModelOutputFormat,
  EnumOptionSpecData as EnumOptionSpec,
  LimitOptionSpecData as LimitOptionSpec,
} from "@zcode/shared/model-config";
export type ModelOptionSpecs = ModelOptionSpecsData;
export type ModelProperties = ModelPropertiesData;
export type ModelPropertiesInput = ModelProperties;

export interface ModelOptions {
  reasoningLevel?: string;
  maxOutputTokens?: number;
}

export interface ModelRequest {
  messages: ModelInputMessage[];
  tools?: ModelToolContract[];
  responseJsonSchema?: JsonSchema;
  options?: ModelOptions;
  abortSignal?: AbortSignal;
}

// The first stage uses the results and stream event fields that have been normalized by the Provider SDK;
// The old names are only retained within the Adapter compatibility boundary, and business calls use the following two names uniformly.
export type ModelResult = ModelTextResult;
export type ModelEvent = ModelStreamEvent;

export interface Model {
  readonly providerId: ModelProviderId;
  readonly modelId: ModelId;
  readonly displayName?: string;
  readonly properties: ModelProperties;
  readonly optionSpecs: ModelOptionSpecs;
  readonly options: ModelOptions;

  bind(options?: ModelOptions): Model;
  generateText(request: ModelRequest): Promise<ModelResult>;
  streamText(request: ModelRequest): AsyncIterable<ModelEvent>;
}

/** Verify the current ModelSelection value; the old database format is only converted during versioned migration, not during normal reading. */
export function parseModelSelectionValue(value: unknown): ModelSelection | undefined {
  const parsed = modelSelectionSchema.safeParse(value);
  return parsed.success ? parsed.data : undefined;
}
