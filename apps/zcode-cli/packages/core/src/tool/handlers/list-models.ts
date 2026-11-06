// ============================================================
// ListModels Tool Handler
// ============================================================
// List the configured models of this host.
//
//There is only one reason for existence: the main agent needs to select a subagent model (`subagent_model`) for a workflow run. It is not a model selection
// Switch ** - This tool cannot change the session's own model. This sentence must be clearly stated in the description, otherwise the model will treat it as
// The entrance to "switch myself", and then report a switch that has not occurred to the user.
//
// There are two complementary paths with the parser (model-reference.ts): that one is passive (when the user says a name and cannot figure it out)
// Returned together with candidates), this one is active ("What models do we have available?"). Both reads are from the same **live** directory.
// ——Every time the port calls the current read registry view, it will never return the frozen copy during the construction period.

import {
  LIST_MODELS_TOOL_NAME,
  ListModelsInputJsonSchema,
  ListModelsInputSchema,
  ListModelsOutputJsonSchema,
  ListModelsOutputSchema,
  type ListModelsOutput,
  type ModelMessageContent,
} from "@zcode/contracts";
import type { ToolEntry, ToolHandler, ToolHandlerFailure } from "../types.js";
import { formatModelCatalogId } from "./model-reference.js";

const LIST_MODELS_TIMEOUT_MS = 10_000;
/** According to ListSavedWorkflows: the directory is deliberately light (one memory read can be answered), 24k is enough for dozens of lines and still leaves a margin. */
const LIST_MODELS_MODEL_BYTES = 24_000;

/**
* Business failure code. The values ​​are just log bits (the executor is projected to `code: "N"`), and the discriminant key is in the message prefix.
* Edited from 31 onwards only for workflow introspection table (1/2), ResumeWorkflowRun (11–15), AmendWorkflow (21–23)
* Vision does not crash.
 */
const LIST_MODELS_ERROR_CODE = { CATALOG_UNAVAILABLE: 31 } as const;

const LIST_MODELS_DESCRIPTION = [
  "Lists the models this host has configured, so a dynamic workflow's subagents can be pointed at one.",
  "",
  "- Each row's `id` (`providerId/modelId`) pastes verbatim into the `subagent_model` field of CreateWorkflow or AmendWorkflow. Append `$<level>` to pick a reasoning level from that row's `reasoningLevels`.",
  "- This tool does NOT change the model you are running on. The session model is the user's choice and only the user changes it; `subagent_model` only moves the workflow's subagents.",
  "- The model the session is on right now is marked `[current]` — setting the subagents to that one is the same as omitting the field.",
  "- A row marked `disabled` cannot be used (no API key, disabled by policy). Resolve that with the user rather than picking around it silently.",
].join("\n");

/**
* "There is no model directory for this session". **Never** silently return an empty list: that will cause the model to combine "This machine does not have a model" with
* "This session cannot read the directory" is mixed into the same conclusion (`workflow_introspection_unavailable` same reason),
* Then tell the user that he doesn't have a model - and he is using one.
 */
function modelCatalogUnavailableFailure(): ToolHandlerFailure {
  return {
    result: false,
    errorCode: LIST_MODELS_ERROR_CODE.CATALOG_UNAVAILABLE,
    message:
      "model_catalog_unavailable: this session cannot list models — the host did not provide a model catalog. This is a capability gap, not an empty configuration. Omit `subagent_model` on CreateWorkflow and AmendWorkflow; the workflow's subagents will run on the session model.",
  };
}

const listModelsHandler: ToolHandler = async (input, context) => {
  ListModelsInputSchema.parse(input);

  const port = context.modelCatalogPort;
  if (port === undefined) return modelCatalogUnavailableFailure();

  const entries = port.listModels();
  const current = entries.find((entry) => entry.current);

  return {
// Absent when no entry in the directory is marked with current (the port contract allows: the session selection may point to a deleted
// provider). Creating an empty string will cause the model to read "no current model" as "current model is empty".
    ...(current === undefined ? {} : { current: formatModelCatalogId(current) }),
    models: entries.map((entry) => ({
      id: formatModelCatalogId(entry),
      providerId: entry.providerId,
      modelId: entry.modelId,
      ...(entry.providerLabel === undefined ? {} : { providerLabel: entry.providerLabel }),
// Models without gears give an empty array instead of absence: the reader knows from this that "it's wrong to follow `$`", while absence reads like
// "This line doesn't say anything".
      reasoningLevels: [...entry.reasoningLevels],
      ...(entry.defaultReasoningLevel === undefined
        ? {}
        : { defaultReasoningLevel: entry.defaultReasoningLevel }),
      ...(entry.contextWindow === undefined ? {} : { contextWindow: entry.contextWindow }),
      ...(entry.disabledReason === undefined ? {} : { disabledReason: entry.disabledReason }),
    })),
  } satisfies ListModelsOutput;
};

/**
* Model surface: one model per line.
 *
* Deliberately different from the **multi-line block** format of ListSavedWorkflows (the other one needs to have a description, usage time and parameter list): directory line
* is a high cardinality entity with low information density - dozens of rows all look the same, and the model only does one thing here, convert the `id` of a certain row
* Copied into `subagent_model`. It fits perfectly into one row and allows a whole catalog to fit within a 24k budget.
 */
function formatListModelsModelContent(output: unknown): ModelMessageContent {
  const parsed = ListModelsOutputSchema.safeParse(output);
  if (!parsed.success) return "ListModels returned an invalid result.";
  const { current, models } = parsed.data;

  if (models.length === 0) {
// "No one is worthy" must be said in one sentence: an empty container is easily read as "the tool is not available".
    return [
      '<models count="0">',
      "No models are configured on this host. Omit `subagent_model`: the workflow's subagents run on the session model.",
      "</models>",
    ].join("\n");
  }

  const lines = models.map((model) => {
    const parts = [model.id];
    if (model.providerLabel !== undefined) parts.push(` — ${model.providerLabel}`);
    if (model.reasoningLevels.length > 0) {
      const levels = model.reasoningLevels.join(",");
      const fallback =
        model.defaultReasoningLevel === undefined ? "" : ` (default ${model.defaultReasoningLevel})`;
      parts.push(`; levels: ${levels}${fallback}`);
    }
// current and disabled are at the end of the line, each enclosed in square brackets: they are the two tokens by which the model **excludes** a line,
// If placed in the middle, it will be pushed out of sight by the longer gear list.
    if (model.id === current) parts.push(" [current]");
    if (model.disabledReason !== undefined) parts.push(` [disabled: ${model.disabledReason}]`);
    return parts.join("");
  });

  return [`<models count="${models.length}">`, ...lines, "</models>"].join("\n");
}

export const listModelsToolEntry: ToolEntry = {
  capability: "List the models this host has configured, for choosing a workflow's subagent model",
  metadata: {
    name: LIST_MODELS_TOOL_NAME,
    description: LIST_MODELS_DESCRIPTION,
    readOnly: true,
    destructive: false,
    concurrentSafe: true,
    timeoutMs: LIST_MODELS_TIMEOUT_MS,
    maxOutputBytes: LIST_MODELS_MODEL_BYTES,
    sideEffectScope: "none",
    riskLevel: "low",
    needsApproval: false,
  },
  handler: listModelsHandler,
  inputSchema: ListModelsInputJsonSchema,
  outputSchema: ListModelsOutputJsonSchema,
  runtimeInputSchema: ListModelsInputSchema,
  runtimeOutputSchema: ListModelsOutputSchema,
  formatModelContent: formatListModelsModelContent,
  permission: {
    permission: "listModels",
    reason: "ListModels reads the host's configured model catalog",
    riskLevel: "low",
    sideEffectScope: "none",
    needsApproval: false,
// The input parameter is an empty object, so the pattern only matches by tool name (same as ListSavedWorkflows).
    patternSources: ["toolName"],
    alwaysAllowPatternSources: ["toolName"],
    denyPriority: "beforeAsk",
// Deliberately **not** inherit CreateWorkflow's alwaysAsk: the reason for that door is to "execute the entire code",
// Reading a table that has a configured model does not belong to it.
  },
  resultBudget: {
    maxInlineBytes: LIST_MODELS_MODEL_BYTES,
    maxModelBytes: LIST_MODELS_MODEL_BYTES,
    strategy: "truncate",
    preview: {
      maxBytes: LIST_MODELS_MODEL_BYTES,
      direction: "head",
    },
  },
  timeout: {
    kind: "timed",
    defaultMs: LIST_MODELS_TIMEOUT_MS,
    maxMs: LIST_MODELS_TIMEOUT_MS,
    allowCallOverride: false,
  },
  cancellation: {
    supported: false,
    cleanup: "none",
    userVisibleMessage: "ListModels reads the in-memory model catalog and cannot be cancelled",
  },
  trace: {
    required: true,
    propagateToAdapters: false,
    recordInput: "summary",
    recordOutput: "summary",
  },
};
