// ============================================================
// Model Catalog Port - A read-only snapshot boundary that hosts a configured model
// ============================================================
// There is only one reason for existence:
// The tool layer should parse the model name ("GLM-5.3-Flash") mentioned by the user into a sub-agent selection for a workflow run, and core
// The provider registry is not visible. The port passes in the host fact "what models are there", and the parsing itself remains in the core
// (Pure function, no I/O).
//
// Deliberate **synchronous, no I/O**: the call points are in the handlers of `resolveInput` and `ListModels`, and should not be placed in either place
// In order to make a list and wait for the network; the host side only reads a registry view in memory.

/** A model entry in the directory (a provider × model combination). */
export interface ModelCatalogEntry {
  providerId: string;
  modelId: string;
/** The human-readable name of the provider; it will be absent if the registry does not provide it (the read side returns `providerId`). */
  providerLabel?: string;
/** The inference level supported by this model (legal value of `$level`). A model without a gear is an empty array, not an absence. */
  reasoningLevels: string[];
/** The default level of the registry; absent when `reasoningLevels` is empty. */
  defaultReasoningLevel?: string;
  contextWindow?: number;
/** Whether this entry is the current selection of the session (provider + model are the same). At most one item in the entire table is true. */
  current: boolean;
/** The reason why it cannot be selected (key is not configured, disabled by policy, etc.); if it can be selected, it will be absent. */
  disabledReason?: string;
}

export interface ModelCatalogPort {
  /**
* Lists the models available at this time.
   *
* **Each call must now read the live view of the registry** and never return a copy frozen during construction: the provider can be used in the session
* It is added, deleted or modified in the middle (the lesson of the stale provider registry is that when the sub-agent holds the parent session constructor
* Do not leave that adapter in place). An out-of-date directory will cause parsing to pick up a model that no longer exists, and failure will have to go to the subagent.
* Explodes when first spoken - long before the user presses confirmation.
   */
  listModels(): ModelCatalogEntry[];
}
