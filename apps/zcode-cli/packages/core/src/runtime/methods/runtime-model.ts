import {
  CoreErrorType,
  createCoreError,
  getCurrentModelInvocationContext,
  runWithModelInvocationContext,
  type Model,
  type ModelInvocationContext,
  type ModelRequest,
} from "../deps.js";
import type { AgentRuntimeInternal } from "../internal.js";
import type { RuntimeModelFactoryInput } from "../types.js";
import { resolveModelRetryBudgetFromTaskType } from "./model-request-session-type.js";

export function createRuntimeModel(
  runtime: AgentRuntimeInternal,
  input: Omit<RuntimeModelFactoryInput, "selection"> & {
    selection: RuntimeModelFactoryInput["selection"] | undefined;
  },
): Model {
// Unbound Session can restore history; missing selections are only rejected at the execution entry, and the Factory contract is still strict.
  if (!input.selection) {
    throw createCoreError(CoreErrorType.ConfigurationError, "Select a model before continuing", {
      recoverable: true,
    });
  }
  return withRuntimeInvocationLayer(
    runtime,
    runtime.modelFactory({ ...input, selection: input.selection }),
  );
}

/**
* runtime layer calling context.
 *
* The admission port and retry budget answer "who is adjusting" (which manager manages this runtime and how many retries are allowed), not
* "Why adjust" (agent step / web_search / compact / title). Design gaps: if they are only in the turn step
* Injection into the calling context, WebSearch / WebFetch processing / compression / title sidecar and other nine places only set "why to adjust"
* All call points bypassed the gate - in the actual measurement scenario, the manager could not see two-thirds of 429. Now these two fields are in the handle
* The factory is bound once; the merging order of `withModelInvocationContext` allows this layer to overwhelm the calling layer, and there is no call-by-call exit:
* A runtime that is not subject to gate constraints does not have access ports.
 */
function withRuntimeInvocationLayer(runtime: AgentRuntimeInternal, model: Model): Model {
  const layer: ModelInvocationContext = {
    modelRetryBudget: resolveModelRetryBudgetFromTaskType(runtime.config.taskType),
    ...(runtime.modelRequestAdmission === undefined
      ? {}
      : { modelRequestAdmission: runtime.modelRequestAdmission }),
  };
  return withModelInvocationContext(model, () => layer);
}

export function withModelInvocationContext(
  model: Model,
  createContext: (request: ModelRequest) => ModelInvocationContext,
): Model {
  const wrapped: Model = {
    providerId: model.providerId,
    modelId: model.modelId,
    displayName: model.displayName,
    properties: model.properties,
    optionSpecs: model.optionSpecs,
    options: model.options,
    bind(options) {
      return withModelInvocationContext(model.bind(options), createContext);
    },
    generateText(request) {
      return runWithModelInvocationContext(
        { ...getCurrentModelInvocationContext(), ...createContext(request) },
        () => model.generateText(request),
      );
    },
    streamText(request) {
      return runWithModelInvocationContext(
        { ...getCurrentModelInvocationContext(), ...createContext(request) },
        () => model.streamText(request),
      );
    },
  };
  return Object.freeze(wrapped);
}
