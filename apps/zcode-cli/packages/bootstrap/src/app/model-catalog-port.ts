// ============================================================
// Host implementation of ModelCatalogPort: Provider Registry → model directory visible to the tool layer
// ============================================================
// For the port contract, see contracts/src/interfaces/model-catalog.port.ts. There is only one reason for existence:
// `Subagent_model` of `CreateWorkflow` / `AmendWorkflow` needs to parse the model name mentioned by the user once
//Sub-agent selection for workflow run, and core cannot see the provider registry. This module hosts the "what models are there"
// The facts are passed over, and the parsing itself remains in the pure function of core.
//
// `listRegistryBackedModels` of provider-registry-selection.ts are the **two faces** of the same registry:
// The GUI picker's `ZCodeModelOption` (with label / maxOutputTokens / format attributes) is output there, and it is output here
// Narrow items for tool layer. Deliberately not sharing the same projection function - the field sets and presence rules of the two faces are independent, forcing them to merge will only
// Let a change for picker quietly change the criteria for model parsing. What is shared is the **default gear rule** (see below).

import type { ModelCatalogEntry, ModelCatalogPort } from "@zcode/contracts";
import type { ModelSelection } from "@zcode/shared/model-selection";
import type { ProviderRegistryModelSource } from "./provider-registry-model-runtime.js";

interface ModelCatalogPortDeps {
/** Provider Registry for the process. **The entire object** is saved, never a snapshot here `getView()`. */
  registry: ProviderRegistryModelSource;
  /**
* The session's **current** model selection (`runtime.getSessionModelSelection()`). Similar to registry, it is a function
* Not a value: `current` is a fact at the moment of each enumeration, the user can change the main model between tool calls.
   */
  currentSelection: () => ModelSelection | undefined;
}

/**
* Create {@link ModelCatalogPort}.
 *
* **`listModels()` reads `registry.getView()`** now on every call and never caches - this is not a performance trade-off;
* Direct lessons from stale provider registry: the moment when the child agent holds the parent session constructor
* The provider adapter is not released. After the user changes the provider midway, the sub-agent is still facing a configuration that no longer exists.
* Send a request. A directory frozen during construction will cause `subagent_model` to parse and pick a model that has now been deleted, and
* Failure will not occur until the subagent speaks for the first time - long before the user presses confirmation. The view itself is a memory object,
* Reread it is not I/O.
 */
export function createModelCatalogPort(deps: ModelCatalogPortDeps): ModelCatalogPort {
  return {
    listModels(): ModelCatalogEntry[] {
// This line is the entire implementation of the above discipline. Any "optimization" that takes it outside of closures is reproducing the same problem.
      const view = deps.registry.getView();
      const current = deps.currentSelection();
      return view.providers.flatMap((provider) =>
        provider.models.map((model): ModelCatalogEntry => {
          const reasoning = model.config.optionSpecs.reasoningLevel;
// The gear table is **copied** rather than handed out as is: the values ​​of the registry are part of the readonly view,
// The port contract gives a common readable array, allowing the caller to get a copy that will not change with the registry.
          const reasoningLevels = [...reasoning.values];
//Default gear = the last gear, and `toModelOption` of provider-registry-selection.ts
// (GUI picker's `reasoning.defaultLevel`) **Same rule**. Different defaults are given in two places,
// There will be a "default high in picker, `subagent_model` defaults to low when no gear is written" type that only users can
// Deviations that will be found. Models without gears have the entire field absent (empty array + no default).
          const defaultReasoningLevel = reasoning.values.at(-1);
          const contextWindow = model.config.properties.contextWindow;
// `providerName` is `string | null | undefined` in the registry (config-service.ts replaces the empty string
// Normalized to `null`), while the port contract is `string | undefined`. Three kinds of "unnamed" are synthesized here
// **A** answer: key is absent. Never pass a `null` or empty string - it will be printed into ListModels unchanged
// That line, what the reader wants is "Return providerId without taking the name".
          const providerLabel = provider.providerName?.trim();
          return {
            providerId: provider.providerId,
            modelId: model.modelId,
// The human-readable name of the provider; if it is not retrieved, it will be absent (the providerId will be returned on the reading side), so it is not covered here.
// providerId - that will make the "has it been named" thing disappear on the port.
            ...(providerLabel ? { providerLabel } : {}),
            reasoningLevels,
            ...(defaultReasoningLevel === undefined ? {} : { defaultReasoningLevel }),
            ...(contextWindow === undefined ? {} : { contextWindow }),
// The two parts of the identity are equal, which is the current selection; options are not part of the identity (similar to workflow-actor-model.ts
// The pins are compared to the same criterion). At most one item in the entire table is true.
            current:
              current !== undefined &&
              current.providerId === provider.providerId &&
              current.modelId === model.modelId,
// `disabledReason` Deliberate **constant absence**: This host has no source of this fact today. Model list for GUI
// Take the same registry (listRegistryBackedModels → toModelOption), and never write this in that path
// Fields; `registry.validateSelection()` is also not a source - every item we enumerate comes from the registry
// The view itself must pass the verification according to the construction. The only place in the warehouse that produces disabledReason is
// This registry cannot be reached in the desktop legacy configuration migration of packages/services. There really is a future
//Criteria for "configured but unavailable" (missing key, disabled by policy), just fill in here, and the port contract does not need to be touched.
          };
        }),
      );
    },
  };
}
