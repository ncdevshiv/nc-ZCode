// ============================================================
// SaveWorkflow Tool Handler
// ============================================================
//Save a dwf script together with metadata as a reusable definition in the project.
//
// The shape is intentionally isomorphic to CreateWorkflow because to the model these are two verbs for the same thing:
// 1. First compile the script using **the same** checker. Can't edit → Return to diagnosis directly, no pop-up window, no disk placement. Let users approve
// A piece of code that cannot be programmed will only interrupt the agent's own error correction and retry loop with a decision that has no effect.
// (same comment in create-workflow.ts).
// 2. Clean → Confirmation window. Writing a file into the user's repository is an irrevocable outward action, always ask.
// 3. Allow → Place the order.
//
// The key difference with CreateWorkflow is the question asked in the confirmation window: "Do you want to spend the money to run this code?"
// The question here is "Should I leave this code in the warehouse so that it may be run by others in the future?" Facts needed to answer it - landing point,
// Metadata, and **whether this time is new or overwritten** - are calculated into the normalized input parameters by `resolveInput`, so this tool
// Deliberately **without any display**: the input parameter channel is transparent transmission without schema for each client version, and the display
// The new fields cannot be read by the old client, and may even cause the entire payload to fail to be verified (see tool-result-metadata.ts
// Field collection annotation). The new desktop reads the parameters according to toolName and renders the rich confirmation block. Whether to overwrite or not is distinguished by the `overwrite` field;
// Old desktop and legacy v3 get common permission prompt + complete input JSON - downgraded but complete content.

import {
  SAVE_WORKFLOW_SENTINEL_IN_SCRIPT_ERROR,
  SAVE_WORKFLOW_SOURCE_ERROR,
  SAVE_WORKFLOW_TOOL_NAME,
  SAVED_WORKFLOW_MAX_NAME_CHARS,
  SaveWorkflowInputJsonSchema,
  SaveWorkflowInputSchema,
  SaveWorkflowOutputJsonSchema,
  SaveWorkflowOutputSchema,
  isValidSavedWorkflowName,
  type ModelMessageContent,
  type SaveWorkflowInput,
  type SaveWorkflowOutput,
  type SavedWorkflowMeta,
} from "@zcode/contracts";
import type {
  ToolApprovalGate,
  ToolEntry,
  ToolHandler,
  ToolInputResolutionResult,
  ToolInputValidationResult,
} from "../types.js";
import { SAVE_WORKFLOW_TOOL_DESCRIPTION } from "./save-workflow-description.js";
import {
  SAVED_WORKFLOW_SENTINEL,
  findSavedWorkflowShadowing,
  savedWorkflowExists,
  savedWorkflowPath,
  savedWorkflowRoot,
  saveSavedWorkflow,
} from "./saved-workflows/index.js";
import { readWorkflowScriptFile } from "./workflow-path-source.js";
import { analyzeScript } from "./workflow-script-analysis.js";

const SAVE_WORKFLOW_TIMEOUT_MS = 15_000;
const SAVE_WORKFLOW_MODEL_BYTES = 24_000;
const SAVE_WORKFLOW_FAILURE_CODE = 400;

const DIAGNOSTICS_NOT_SAVED_NOTE =
  "NOTE: Nothing was saved — fix the errors above and call the tool again.";

/** Metadata view: The three input fields are combined into the object to be written by frontmatter. */
function toMeta(parsed: SaveWorkflowInput): SavedWorkflowMeta {
  return {
    description: parsed.description,
    ...(parsed.whenToUse === undefined ? {} : { whenToUse: parsed.whenToUse }),
    ...(parsed.args === undefined ? {} : { args: parsed.args }),
  };
}

/**
* Semantic verification on model input parameters is closed before hook.
 *
* The script with built-in frontmatter is rejected according to business failure, but does not do the smart merge of "replace when detected": after the merge
* The model has no way of distinguishing between the description passed by itself and the one in the file, which one takes effect, and the idempotence of encode (in the file
* There is always only one frontmatter block) and there are no guards.
 */
function validateSaveWorkflowInput(input: unknown): ToolInputValidationResult {
  const parsed = SaveWorkflowInputSchema.safeParse(input);
  if (!parsed.success) return { result: true };

  if (!isValidSavedWorkflowName(parsed.data.name)) {
    return {
      result: false,
      errorCode: SAVE_WORKFLOW_FAILURE_CODE,
      message: `'${parsed.data.name}' is not a usable workflow name: names may only contain letters, digits, '.', '-' and '_', and must be 1-${SAVED_WORKFLOW_MAX_NAME_CHARS} characters.`,
    };
  }

  const hasScript = parsed.data.script !== undefined;
  const hasScriptPath = parsed.data.script_path !== undefined;
  if (hasScript === hasScriptPath) {
    return {
      result: false,
      errorCode: SAVE_WORKFLOW_FAILURE_CODE,
      message: SAVE_WORKFLOW_SOURCE_ERROR,
    };
  }

// The built-in frontmatter check only works for **inline** text: `script_path` usually refers to a copy from the saved definition
// Draft, it originally comes with blocks, and `resolveInput` will throw away the blocks. Reporting an error to it is equivalent to disabling the main usage of this path.
  if (parsed.data.script?.trimStart().startsWith(SAVED_WORKFLOW_SENTINEL) === true) {
    return {
      result: false,
      errorCode: SAVE_WORKFLOW_FAILURE_CODE,
      message: SAVE_WORKFLOW_SENTINEL_IN_SCRIPT_ERROR,
    };
  }

  return { result: true };
}

/**
* Normalization: Calculate the landing point, coverage determination and shading facts and fill in the input parameters.
 *
* These facts are key to the decision-making process in the confirmation window (approve one overwrite = agree to discard the copy on disk; mask prompt = tell the user
* Will this definition be blocked by another file with the same name in this project?), use **input parameter** instead of display, because the input channel is different for each
* The client versions are transparent transmission without schema - the old desktop and legacy v3 can also get the complete content, while the display
* They cannot read new fields, and may even make the entire payload fail to be verified.
 *
* The scope is no longer backfilled: it is a required field in the model, here only the root is selected to count `path` / `overwrite` / `shadowing`.
 */
async function resolveSaveWorkflowInput(
  input: unknown,
  cwd: string,
): Promise<ToolInputResolutionResult> {
  const parsed = SaveWorkflowInputSchema.safeParse(input);
  if (!parsed.success) return { result: true, input };

// `script_path` is read into the text first: the confirmation window should display the string of bytes that will be written to the disk, and the window is before the handler.
// The chunk is discarded (only the text remains) - the metadata is determined by the fields in this call, and that's what the user approves.
  let script = parsed.data.script;
  if (parsed.data.script_path !== undefined) {
    const read = await readWorkflowScriptFile({ cwd, inputPath: parsed.data.script_path });
    if (!read.ok) {
      return { result: false, errorCode: SAVE_WORKFLOW_FAILURE_CODE, message: read.message };
    }
    script = read.file.script;
  }

  const { scope, name } = parsed.data;
  const shadowing = findSavedWorkflowShadowing({ cwd, name, scope });
  return {
    result: true,
    input: {
      ...parsed.data,
      ...(script === undefined ? {} : { script }),
      path: savedWorkflowPath(savedWorkflowRoot(cwd, scope), name),
      overwrite: savedWorkflowExists({ cwd, name, scope }),
// Omit this key when there is no same name: an undefined will cause a noise field to be attached to each save.
      ...(shadowing === undefined ? {} : { shadowing }),
    } satisfies SaveWorkflowInput,
  };
}

const saveWorkflowHandler: ToolHandler = async (input, context) => {
  const parsed = SaveWorkflowInputSchema.parse(input) as SaveWorkflowInput;
  const cwd = context.workingDirectory ?? ".";
  const scope = parsed.scope;
// The drop point is filled in by normalization; the absence can only be caused by someone bypassing the executor's life cycle. At this time, it will be counted again instead of crashing.
  const path = parsed.path ?? savedWorkflowPath(savedWorkflowRoot(cwd, scope), parsed.name);
  const script = parsed.script;
  if (script === undefined) {
// Unable to reach: validateInput blocks "neither", resolveInput will read `script_path` as `script`.
// If it really happens, it means someone has bypassed the executor's life cycle. It is better to tell it than to silently save an empty file.
    throw new Error("SaveWorkflow handler received input without a resolved script");
  }

  const { diagnostics, ok } = analyzeScript(script);
  if (!ok) {
    return {
      name: parsed.name,
      scope,
      path,
      diagnostics,
      ok,
      response: [
        "The workflow script has errors:",
        ...diagnostics.map(
          (diagnostic) => `L${diagnostic.line}:C${diagnostic.column} ${diagnostic.message}`,
        ),
        "",
        DIAGNOSTICS_NOT_SAVED_NOTE,
      ].join("\n"),
    } satisfies SaveWorkflowOutput;
  }

// Write failures (read-only mounts, permissions) bubble up into tool call failures: never swallowed into a successful output reporting the path,
// That will cause the model to tell the user "saved" accordingly.
  //
// `overwritten` is re-determined from the moment ** is written, and the one in the input parameter is not read. `overwrite`: The one on the input parameter is for
// The confirmation window is the same as the fact that the hook looks at, and the hook can rewrite the parameters - letting it change the readme after placing the order will create a display field
// Become a behavior switch.
  const written = saveSavedWorkflow({
    cwd,
    name: parsed.name,
    meta: toMeta(parsed),
    script,
    scope,
  });

  const isGlobal = written.scope === "global";
  return {
    name: parsed.name,
    scope: written.scope,
    path: written.path,
    diagnostics,
    ok,
    overwritten: written.overwritten,
    response: [
      written.overwritten
        ? isGlobal
          ? `Replaced the saved global workflow '${parsed.name}' at ${written.path}.`
          : `Replaced the saved workflow '${parsed.name}' at ${written.path}.`
        : isGlobal
          ? `Saved global workflow '${parsed.name}' to ${written.path}.`
          : `Saved the workflow '${parsed.name}' to ${written.path}.`,
      `Run it with CreateWorkflow using \`saved: { name: "${parsed.name}" }\`.`,
    ].join(" "),
  } satisfies SaveWorkflowOutput;
};

/**
* Determine whether "write this script into the warehouse" is worth interrupting the user.
 *
* **Without display**: Everything to be displayed in the confirmation window - scripts, metadata, drop points, whether it is overwritten or not - is already normalized
* Entered the ginseng. Press `toolName === "SaveWorkflow"` on the new desktop to read in the parameters and render the rich confirmation block, the old desktop and legacy v3
* Get a general permission prompt with complete input parameters: downgraded but complete, and valid on **all** version combinations. Past
* Adding a new kind to display can only be read by new clients (see the comments of tool-result-metadata.ts).
 *
* Scripts that cannot be edited are directly released to the handler: it will return the diagnosis and will not be placed on the disk. If there is nothing to judge, there should be no window.
 */
function prepareSaveWorkflowApproval(input: unknown): ToolApprovalGate {
  const parsed = SaveWorkflowInputSchema.safeParse(input);
// After normalization, `script` must be present; if it is absent, someone has bypassed the life cycle. At this time, it will be released and an error will be reported to the handler.
  if (!parsed.success || parsed.data.script === undefined) return { gate: "proceed" };
  return analyzeScript(parsed.data.script).ok ? { gate: "ask" } : { gate: "proceed" };
}

function formatSaveWorkflowModelContent(output: unknown): ModelMessageContent {
  const parsed = SaveWorkflowOutputSchema.safeParse(output);
  if (!parsed.success) return "SaveWorkflow returned an invalid result.";
  return parsed.data.response;
}

export const saveWorkflowToolEntry: ToolEntry = {
  capability:
    "Typecheck a dynamic-workflow script and, once confirmed, save it into the project as a reusable definition",
  metadata: {
    name: SAVE_WORKFLOW_TOOL_NAME,
    description: SAVE_WORKFLOW_TOOL_DESCRIPTION,
    readOnly: false,
// Overwriting an existing definition will lose the copy on disk, but the confirmation window will make this clear first; same file as Write.
    destructive: false,
    concurrentSafe: false,
    timeoutMs: SAVE_WORKFLOW_TIMEOUT_MS,
    maxOutputBytes: SAVE_WORKFLOW_MODEL_BYTES,
// What is written is a file in the project, which is in the same scope as Write/Edit.
    sideEffectScope: "workspace",
    riskLevel: "medium",
    needsApproval: true,
  },
  handler: saveWorkflowHandler,
  validateInput: (input) => validateSaveWorkflowInput(input),
// Enter the drop point and coverage judgment calculation into parameters: the confirmation window and hook read the same fact, and are visible to all client versions.
  resolveInput: (input, context) =>
    resolveSaveWorkflowInput(input, context.workingDirectory ?? "."),
  prepareApproval: prepareSaveWorkflowApproval,
  inputSchema: SaveWorkflowInputJsonSchema,
  outputSchema: SaveWorkflowOutputJsonSchema,
  runtimeInputSchema: SaveWorkflowInputSchema,
  runtimeOutputSchema: SaveWorkflowOutputSchema,
  formatModelContent: formatSaveWorkflowModelContent,
  permission: {
    permission: "saveWorkflow",
// For diagnostic purposes, not for users: the confirmation window renders the localized title by itself.
    reason: "saveWorkflow.confirmation: user must confirm writing the workflow into the project",
    riskLevel: "medium",
    sideEffectScope: "workspace",
    needsApproval: true,
    patternSources: ["toolName"],
    alwaysAllowPatternSources: ["toolName"],
    denyPriority: "beforeAsk",
// Same file as CreateWorkflow: things written into the warehouse will be submitted, seen by others, and run again in the future.
// Any permission mode (including yolo / plan) must be asked first.
    alwaysAsk: true,
// Each call writes a different file and different content. The persistent project rules cannot remember "this decision".
// This confirmation will only be turned off permanently.
    askOptions: { allowAlways: false },
  },
  resultBudget: {
    maxInlineBytes: SAVE_WORKFLOW_MODEL_BYTES,
    maxModelBytes: SAVE_WORKFLOW_MODEL_BYTES,
    strategy: "truncate",
    preview: {
      maxBytes: SAVE_WORKFLOW_MODEL_BYTES,
      direction: "head",
    },
  },
  timeout: {
    defaultMs: SAVE_WORKFLOW_TIMEOUT_MS,
    maxMs: SAVE_WORKFLOW_TIMEOUT_MS,
    allowCallOverride: false,
  },
  cancellation: {
    supported: false,
    cleanup: "none",
    userVisibleMessage: "SaveWorkflow typechecks and writes synchronously and cannot be cancelled",
  },
  trace: {
    required: true,
    propagateToAdapters: true,
    recordInput: "summary",
    recordOutput: "summary",
  },
};
