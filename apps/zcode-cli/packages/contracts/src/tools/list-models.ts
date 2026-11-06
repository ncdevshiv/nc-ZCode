// ============================================================
// ListModels Tool - List the configured models of this host
// ============================================================
//
// A read-only, side-effect-free discovery surface that serves one thing: the main agent needs to select a sub-agent model for a workflow run
// (`subagent_model` of `CreateWorkflow` / `AmendWorkflow`). It is not a mode selector switch - this tool cannot change it
// Session's own model, the main agent always stays on the one selected by the user.
//
// There are **two complementary ways** with the parser: first pass the name directly according to the user's name. If it cannot be solved, the tool will return it together with the candidate;
// This tool is the active one ("What models do we have available?"). Both paths read the same living directory
// （{@link import("../interfaces/model-catalog.port.js").ModelCatalogPort}）。

import { z } from "zod";
import { toToolJsonSchema } from "./json-schema.js";

export const LIST_MODELS_TOOL_NAME = "ListModels";

export const ListModelsInputSchema = z
  .object({})
// The same constraint as ListSavedWorkflows: no input parameters. Filtering, paging, and fuzzy queries are not provided - the directory is
// In a table with dozens of rows, one more knob means one more place that makes the model think "not all columns are listed".
  .strict();

export type ListModelsInput = z.infer<typeof ListModelsInputSchema>;

export const ListModelsInputJsonSchema = toToolJsonSchema(ListModelsInputSchema);

/** A line in the directory. `id` is the canonical form that can be filled **verbatim** into `subagent_model`. */
export const ListModelsEntrySchema = z
  .object({
/** Canonical form `providerId/modelId` (excluding inference level; if you want the level, just connect `$level` yourself). */
    id: z.string(),
    providerId: z.string(),
    modelId: z.string(),
/** The human-readable name of the provider; absent if not given by the registry. */
    providerLabel: z.string().optional(),
/** Legal `$level` value. A model without a gear is an empty array - the reader knows that "it's wrong to follow `$`". */
    reasoningLevels: z.array(z.string()),
/** The level that will be used when `$level` is not connected; absent when `reasoningLevels` is empty. */
    defaultReasoningLevel: z.string().optional(),
    contextWindow: z.number().optional(),
/** The reason why it is not optional; optional means it is absent (not an empty string). */
    disabledReason: z.string().optional(),
  })
  .strict();

export type ListModelsEntry = z.infer<typeof ListModelsEntrySchema>;

export const ListModelsOutputSchema = z
  .object({
    /**
* The model used at the moment of the session (canonical form). **Just coordinates, not recommendations**: Run when the subagent omits `subagent_model`
* That's it, so "set the subagent to this one" does nothing. Absent when none of the items in the catalog match.
     */
    current: z.string().optional(),
    models: z.array(ListModelsEntrySchema),
  })
  .strict();

export type ListModelsOutput = z.infer<typeof ListModelsOutputSchema>;

export const ListModelsOutputJsonSchema = toToolJsonSchema(ListModelsOutputSchema);
