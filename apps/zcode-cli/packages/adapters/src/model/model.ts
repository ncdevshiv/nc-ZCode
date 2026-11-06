import {
  ModelErrorCode,
  ModelFailureReason,
  ModelProtocolError,
  type Model,
  type ModelEvent,
  type ModelOptionSpecs,
  type ModelOptions,
  type ModelProperties,
  type ModelPropertiesInput,
  type ModelId,
  type ModelProviderId,
  type ModelRequest,
  type ModelResult,
} from "@zcode/contracts";

export interface ModelExecutionRequest extends Omit<ModelRequest, "options"> {
  options: Required<ModelOptions>;
}

export interface ModelExecutor {
  generateText(request: ModelExecutionRequest): Promise<ModelResult>;
  streamText(request: ModelExecutionRequest): AsyncIterable<ModelEvent>;
}

export interface CreateModelOptions {
  providerId: ModelProviderId;
  modelId: ModelId;
  displayName?: string;
  properties: ModelPropertiesInput;
  optionSpecs: ModelOptionSpecs;
  options?: ModelOptions;
  executor: ModelExecutor;
}

export function createModel(input: CreateModelOptions): Model {
  return new ExecutableModel(input);
}

class ExecutableModel implements Model {
  readonly providerId: ModelProviderId;
  readonly modelId: ModelId;
  readonly displayName?: string;
  readonly properties: ModelProperties;
  readonly optionSpecs: ModelOptionSpecs;
  readonly options: ModelOptions;
  private readonly executor: ModelExecutor;

  constructor(input: CreateModelOptions) {
    this.providerId = input.providerId;
    this.modelId = input.modelId;
    this.displayName = input.displayName;
    this.properties = Object.freeze({ ...input.properties });
    this.optionSpecs = freezeOptionSpecs(input.optionSpecs);
    this.options = Object.freeze(validatePartialOptions(this.optionSpecs, input.options ?? {}));
    this.executor = input.executor;
  }

  bind(options?: ModelOptions): Model {
    if (!options || Object.keys(options).length === 0) return this;
    return new ExecutableModel({
      providerId: this.providerId,
      modelId: this.modelId,
      displayName: this.displayName,
      properties: this.properties,
      optionSpecs: this.optionSpecs,
      options: { ...this.options, ...options },
      executor: this.executor,
    });
  }

  async generateText(request: ModelRequest): Promise<ModelResult> {
    return this.executor.generateText(this.prepareRequest(request));
  }

  streamText(request: ModelRequest): AsyncIterable<ModelEvent> {
    const prepared = this.prepareRequest(request);
    return this.executor.streamText(prepared);
  }

  private prepareRequest(request: ModelRequest): ModelExecutionRequest {
    validateRequestProperties(this.properties, request);
    const requestOptions = request.options ?? {};
    return {
      messages: request.messages,
      tools: request.tools,
      responseJsonSchema: request.responseJsonSchema,
      abortSignal: request.abortSignal,
      options: {
        ...validateOptions(this.optionSpecs, { ...this.options, ...requestOptions }),
      },
    };
  }
}

function freezeOptionSpecs(specs: ModelOptionSpecs): ModelOptionSpecs {
  return Object.freeze({
    maxOutputTokens: Object.freeze({ ...specs.maxOutputTokens }),
    reasoningLevel: Object.freeze({
      ...specs.reasoningLevel,
      values: Object.freeze([...specs.reasoningLevel.values]),
    }),
  });
}

function validateOptions(specs: ModelOptionSpecs, options: ModelOptions): Required<ModelOptions> {
  const maxOutputTokens = options.maxOutputTokens;
  if (
    maxOutputTokens === undefined ||
    !Number.isInteger(maxOutputTokens) ||
    maxOutputTokens <= 0 ||
    maxOutputTokens > specs.maxOutputTokens.max
  ) {
    throw invalidRequest("maxOutputTokens is outside the model option range", {
      maxOutputTokens,
      spec: specs.maxOutputTokens,
    });
  }

  const reasoningLevel = options.reasoningLevel;
  const reasoningSpec = specs.reasoningLevel;
  if (reasoningLevel === undefined || !reasoningSpec.values.includes(reasoningLevel)) {
    throw invalidRequest("reasoningLevel is not supported by the model", {
      reasoningLevel,
      values: reasoningSpec.values,
    });
  }

  return { maxOutputTokens, reasoningLevel };
}

function validatePartialOptions(specs: ModelOptionSpecs, options: ModelOptions): ModelOptions {
  const maxOutputTokens = options.maxOutputTokens;
  if (
    maxOutputTokens !== undefined &&
    (!Number.isInteger(maxOutputTokens) ||
      maxOutputTokens <= 0 ||
      maxOutputTokens > specs.maxOutputTokens.max)
  ) {
    throw invalidRequest("maxOutputTokens is outside the model option range", {
      maxOutputTokens,
      spec: specs.maxOutputTokens,
    });
  }
  const reasoningLevel = options.reasoningLevel;
  if (reasoningLevel !== undefined && !specs.reasoningLevel.values.includes(reasoningLevel)) {
    throw invalidRequest("reasoningLevel is not supported by the model", {
      reasoningLevel,
      values: specs.reasoningLevel.values,
    });
  }
  return { ...options };
}

function validateRequestProperties(properties: ModelProperties, request: ModelRequest): void {
  if (request.tools && request.tools.length > 0 && !properties.supportsToolCall) {
    throw invalidRequest("Model does not support tool calls");
  }
  if (request.responseJsonSchema && !properties.supportsJsonSchemaOutput) {
    throw invalidRequest("Model does not support structured output");
  }

  for (const message of request.messages) {
    if (!Array.isArray(message.content)) continue;
    for (const block of message.content) {
      if (block.type === "image" && !properties.inputFormat.supportsImage) {
        throw invalidRequest("Model does not support image input");
      }
      if (
        block.type === "file" &&
        block.mediaType.toLowerCase() === "application/pdf" &&
        !properties.inputFormat.supportsPdf
      ) {
        throw invalidRequest("Model does not support PDF input");
      }
      if (
        block.type === "file" &&
        block.mediaType.toLowerCase().startsWith("video/") &&
        !properties.inputFormat.supportsVideo
      ) {
        throw invalidRequest("Model does not support video input");
      }
    }
  }
}

function invalidRequest(message: string, context?: Record<string, unknown>): ModelProtocolError {
// Capability / option verification occurs before the executor and will not go through the runner's failure normalization;
// If only code/message is retained, neither TurnError nor cold recovery can know that this is a local verification failure before the request is issued.
  return new ModelProtocolError(ModelErrorCode.InvalidModelRequest, message, {
    ...context,
    reason: ModelFailureReason.InvalidRequest,
    retryable: false,
    source: "runtime",
  });
}
