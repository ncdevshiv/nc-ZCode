// ============================================================
// CreateWorkflow - verification and normalization of input parameters
// ============================================================
//
// This module is where **two sources are combined into one script**, and it is also the only time the disk is read in the entire process. The confluence point must remain single:
// Put it back in the handler, it will gradually be entangled with the handler's branch, and the next person will naturally open another "saved run special"
// processing", and the first victim of that fork is the confirmation window - it will start showing bytes that are different from what is going to be executed.

import {
  CREATE_WORKFLOW_ARGS_WITHOUT_PATH_ERROR,
  CREATE_WORKFLOW_SOURCE_ERROR,
  CreateWorkflowInputSchema,
  type CreateWorkflowInput,
  type ModelCatalogPort,
  type SavedWorkflowArgsDeclaration,
  type SavedWorkflowScope,
} from "@zcode/contracts";
import type { ToolHandlerFailure, ToolInputResolutionResult } from "../types.js";
import { resolveModelReference } from "./model-reference.js";
import {
  listSavedWorkflows,
  resolveSavedWorkflow,
  validateWorkflowArgs,
} from "./saved-workflows/index.js";
import { writeWorkflowDraft } from "./workflow-drafts.js";
import { readWorkflowScriptFile } from "./workflow-path-source.js";

/** The error code of business failure, the same shape as the existing handler failure convention. */
const CREATE_WORKFLOW_FAILURE_CODE = 400;

function failure(message: string): ToolHandlerFailure {
  return { result: false, errorCode: CREATE_WORKFLOW_FAILURE_CODE, message };
}

/**
* Choose one of three sources (plus "`args` only follows `path`"), which is only true for the ** input parameters sent by the ** model.
 *
* This article cannot be written into zod: after normalization, the presence of `script` and `saved` / `path` at the same time is a legal execution state, and schema will be in
* The handler's parse and hook will be blown up after the rewritten secondary verification - and only on non-inline paths.
 */
export function validateCreateWorkflowSource(
  input: unknown,
): { result: true } | ToolHandlerFailure {
  const parsed = CreateWorkflowInputSchema.safeParse(input);
// The failure of the schema itself has been closed by the executor at an earlier position, and only XOR is used here.
  if (!parsed.success) return { result: true };
  const sources = [parsed.data.script, parsed.data.saved, parsed.data.path].filter(
    (source) => source !== undefined,
  );
  if (sources.length !== 1) return failure(CREATE_WORKFLOW_SOURCE_ERROR);
// `args` and `path` go together: the actual parameters of `saved` go to `saved.args`, and the inline script has no declaration to verify. silence
// Ignoring it will make the caller think that the parameters are valid, and the `args` read by the script is empty.
  if (parsed.data.args !== undefined && parsed.data.path === undefined) {
    return failure(CREATE_WORKFLOW_ARGS_WITHOUT_PATH_ERROR);
  }
  return { result: true };
}

/**
* Concurrency upper bound clamp: `[1, ceiling]`.
* Values ​​that exceed the ceiling are suppressed instead of rejected - when the model says "up to 32", what the user wants is an upper bound, not an error.
 *
* When the ceiling is unknown (the port is absent, or the host's port does not have `concurrencyCeiling`), it is released as is: the port implementation itself will still
* Clamp once. One less clamp here will only make the confirmation window display a larger number, and refusing to execute will cause the entire path to collapse.
 *
* `CreateWorkflow` and `AmendWorkflow` share this function: the same number will produce different results on the two tools.
* Inconsistencies that are only discovered when the user changes the concurrency once.
 */
export function clampWorkflowMaxConcurrency(value: number, ceiling: number | undefined): number {
//The value has passed the schema (positive integer), and the lower bound is still written: this helper is the common entrance of the two tools, and the schema has been changed
// Nor should 0 or negative numbers pass through.
  const atLeastOne = Math.max(1, value);
  return ceiling === undefined ? atLeastOne : Math.min(atLeastOne, Math.max(1, ceiling));
}

/**
* The normalized result of the sub-agent model: the solved canonical form, or a business failure.
 *
* Use `result` instead of creating a self-made `ok` for the judgment bit: the call point must be able to `return` the failure as it is, and
* The discriminant bit of `ToolHandlerFailure` is `result: false`.
 */
type SubagentModelResolution = { result: true; canonical?: string } | ToolHandlerFailure;

/**
* Parse `subagent_model` into canonical form (the half of `CreateWorkflow`; the three states of `AmendWorkflow` in
* amend-workflow.ts, shared by both sides `resolveModelReference`).
 *
* When the port is absent but the field is present, it is **explicitly rejected** and not silently allowed: a string that the host cannot understand is passed along and will eventually
* Explodes when the subagent first opens its mouth - well before the user presses confirm, and looks like a problem with the model at that point.
 */
function resolveCreateSubagentModel(
  requested: string | undefined,
  catalog: ModelCatalogPort | undefined,
): SubagentModelResolution {
  if (requested === undefined) return { result: true };
  if (catalog === undefined) return failure(SUBAGENT_MODEL_UNAVAILABLE);
  const resolution = resolveModelReference(requested, catalog.listModels());
// If it cannot be solved, the entire call fails: nothing is started, and the confirmation window is not opened (the same path as "the saved definition does not exist").
  if (!resolution.ok) return failure(resolution.message);
  return { result: true, canonical: resolution.canonical };
}

/** Rejection text when the host has no model directory (common to `AmendWorkflow`, so constant rather than inline string). */
export const SUBAGENT_MODEL_UNAVAILABLE =
  "This host cannot choose a subagent model; omit subagent_model.";

/**
* Normalize input parameters into execution facts. The inline source is the **identity function** (no disk operation is performed); the saved source parsing file,
* Verify actual parameters, backfill default values, write a working copy, and output `{name, script, saved: {name, args, path, scope,
* draft}}`; `path` source reads that file (with metadata blocks, strip off the blocks and verify `args`), output
 * `{name, script, path, args?, script_line_offset?}`。
 *
* After normalization, `script` must be present, so the downstream (hook, permission rules, prepareApproval, handler) has three
*The source is the same code. `saved` / `path` is just the ins and outs from now on: the details of the run tag, the persistence of the actual parameters and
* The logging of the script file reads them, and the execution does not read a single byte of them.
 *
* `ceiling` is the concurrency ceiling of the local machine (`port.concurrencyCeiling?.()`, if it is absent, it will not be clamped); `catalog` is the local concurrency ceiling
* The model catalog (`context.modelCatalogPort`, if it is absent, the model cannot be selected). `max_concurrency` and
* `subagent_model` are all top-level fields, and the three sources are processed in the same way.
 */
export async function resolveCreateWorkflowInput(
  input: unknown,
  cwd: string,
  ceiling?: number,
  catalog?: ModelCatalogPort,
): Promise<ToolInputResolutionResult> {
  const parsed = CreateWorkflowInputSchema.safeParse(input);
  if (!parsed.success) return { result: true, input };
  const model: CreateWorkflowInput = parsed.data;
  const requested = model.max_concurrency;

// Model parsing comes before disk reading: three sources come from the same code, and a call that cannot be solved should not scan the disk first.
  const subagentModel = resolveCreateSubagentModel(model.subagent_model, catalog);
  if (!subagentModel.result) return subagentModel;
  const subagentModelField =
    subagentModel.canonical === undefined ? {} : { subagent_model: subagentModel.canonical };
  const clampedField =
    requested === undefined
      ? {}
      : { max_concurrency: clampWorkflowMaxConcurrency(requested, ceiling) };

  if (model.path !== undefined) {
    return resolvePathSource(model, model.path, cwd, {
      ...clampedField,
      ...subagentModelField,
    });
  }

// Inline: Identity. The inline path must not be touched once - that is a measurable form of "zero regression".
  if (model.saved === undefined) {
// Both exceptions are "If you rewrite it, it will not read the disk. If you don't rewrite it, the confirmation window will not display what will take effect.": Overcrowded concurrency
// Upper bound, and model name that has not been normalized to canonical form. When neither is touched, it remains the same byte by byte.
    const clamped =
      requested === undefined ? undefined : clampWorkflowMaxConcurrency(requested, ceiling);
// The model name compares to the **original** input parameters instead of `model.subagent_model`: the schema has `.trim()`, so there are
// The blank string is parsed to be equal to the canonical form, and the identity release will cause the confirmation window to display the string of blanks.
    const rawSubagentModel = (input as { subagent_model?: unknown } | null)?.subagent_model;
    if (clamped === requested && subagentModel.canonical === rawSubagentModel) {
      return { result: true, input };
    }
    return {
      result: true,
      input: {
        ...model,
        ...(clamped === undefined ? {} : { max_concurrency: clamped }),
        ...subagentModelField,
      } satisfies CreateWorkflowInput,
    };
  }

  const found = resolveSavedWorkflow({ cwd, name: model.saved.name, scope: model.saved.scope });
  if (!found.ok) return describeResolveFailure(model.saved.name, found, cwd, model.saved.scope);

  const validated = validateWorkflowArgs(found.meta.args, model.saved.args);
  if (!validated.ok) {
    return failure(
      [
        `The arguments for saved workflow '${model.saved.name}' are not valid:`,
        ...validated.errors.map((error) => `- ${error}`),
        "",
        describeArgsDeclaration(`'${found.name}'`, found.meta.args),
      ].join("\n"),
    );
  }

// The working copy is written after this read. What is written is the string of bytes just read (together with the metadata block), so there is no possibility
// The second read forks with it. The definition itself is never changed from one run: the model is modified by this copy.
  const draft = await writeWorkflowDraft({
    cwd,
    name: model.name ?? found.name,
    source: found.source,
  });

  return {
    result: true,
    input: {
// When the display name is not specified, the saved name is taken: run enumerated across sessions, so you can still recognize which workflow it is, instead of
// A string of bare runIds. The existing read rule for `readWorkflowName` (reading input.name) therefore hits unchanged.
      name: model.name ?? found.name,
// Verbatim script body. Old desktop's `readWorkflowScript(raw.script)` so **structural** hits -
// Not compatible.
      script: found.script,
      saved: {
        name: found.name,
        args: validated.args,
        path: found.path,
        scope: found.scope,
// When the draft cannot be written, the entire field will be absent (do your best), and the model will return to the old copy.
        ...(draft === undefined ? {} : { draft: draft.path }),
      },
//Copy byte-by-byte with metadata blocks, so the diagnostic file lines should skip the lines of the block.
      ...(found.bodyLineOffset === 0 ? {} : { script_line_offset: found.bodyLineOffset }),
      ...(requested === undefined
        ? {}
        : { max_concurrency: clampWorkflowMaxConcurrency(requested, ceiling) }),
// The saved branch is composed of new parameters from scratch, so each top-level field must be named here once, otherwise it will be
// Silently discarded - and "only discarded on the saved path" is the most difficult to detect failure.
      ...subagentModelField,
    } satisfies CreateWorkflowInput,
  };
}

/**
* Normalization of `path` sources.
 *
* Don’t write a draft: The file is already a working copy, and copying it again will only make the model not know which one to change next time.
 *
* Files with metadata blocks are parsed according to the saving definition, and `args` is verified according to the declaration in the block (the default value is also filled in); files without blocks are completely
* is a script. It is an error to give `args` at this time - no statement can verify them, and silently discarding them will make the caller think that the parameters are valid.
 */
async function resolvePathSource(
  model: CreateWorkflowInput,
  inputPath: string,
  cwd: string,
  extraFields: { max_concurrency?: number; subagent_model?: string },
): Promise<ToolInputResolutionResult> {
  const read = await readWorkflowScriptFile({ cwd, inputPath });
  if (!read.ok) return failure(read.message);
  const file = read.file;

  if (file.meta === undefined && model.args !== undefined) {
    return failure(
      `The workflow script file ${file.described} declares no arguments (it has no \`/* zcode-workflow\` metadata block), so it takes none. Drop \`args\`, or add a block declaring them.`,
    );
  }

  const validated = validateWorkflowArgs(file.meta?.args, model.args);
  if (!validated.ok) {
    return failure(
      [
        `The arguments for the workflow script file ${file.described} are not valid:`,
        ...validated.errors.map((error) => `- ${error}`),
        "",
        describeArgsDeclaration(file.described, file.meta?.args),
      ].join("\n"),
    );
  }

  return {
    result: true,
    input: {
// The same discipline as the saved branch: here is a new input parameter from scratch, and each top-level field must be named once.
      ...(model.name === undefined ? {} : { name: model.name }),
      script: file.script,
      path: file.path,
// When declared empty, the actual parameter is always `{}`; no empty shell key is created, and it maintains the same shape as the "no actual parameter" of inline run.
      ...(Object.keys(validated.args).length === 0 ? {} : { args: validated.args }),
      ...(file.bodyLineOffset === 0 ? {} : { script_line_offset: file.bodyLineOffset }),
      ...extraFields,
    } satisfies CreateWorkflowInput,
  };
}

/**
* Description of parsing failure. **List actual available names** (with scope labels) when not found: most useful if the model guesses a wrong name
* The next information is the correct batch, otherwise it will just guess again.
 *
* The copy that cannot be found is divided into two categories: if it is given `scope`, it will say "it is not in this scope", and if it is not given, it will say "it is not found anywhere" - in both cases, put two
* List the names in the files so that the model can see if the one it wants is in another file.
 */
function describeResolveFailure(
  name: string,
  found: Exclude<ReturnType<typeof resolveSavedWorkflow>, { ok: true }>,
  cwd: string,
  scope: SavedWorkflowScope | undefined,
): ToolHandlerFailure {
  if (found.reason === "invalid_name") {
    return failure(`'${name}' is not a usable workflow name: ${found.detail}`);
  }
  if (found.reason === "parse_error") {
    return failure(
      [
        `The saved workflow '${name}' at ${found.path} could not be read: ${found.detail}`,
        "",
        "Its metadata block is malformed — most likely hand-edited. Fix the file, or save the workflow again.",
      ].join("\n"),
    );
  }
  if (found.reason === "read_error") {
    return failure(
      `The saved workflow '${name}' at ${found.path} could not be read: ${found.detail}`,
    );
  }

  const headline =
    scope === undefined
      ? `No saved workflow named '${name}' in this project or globally.`
      : `No ${scope} workflow named '${name}'.`;
  return failure(`${headline}\n\n${describeAvailableWorkflows(cwd)}`);
}

/**
* The names actually available in the two files, each with a scope label. Directionally scan each root (without masking) so that those covered by the project file
* The global definition also appears in the manifest - this is how the model knows to specify `scope: "global"` to access it.
 */
function describeAvailableWorkflows(cwd: string): string {
  const project = listSavedWorkflows({ cwd, scope: "project" }).entries;
  const global = listSavedWorkflows({ cwd, scope: "global" }).entries;
  const tagged = [
    ...project.map((entry) => `${entry.name} (project)`),
    ...global.map((entry) => `${entry.name} (global)`),
  ];
  if (tagged.length === 0) {
    return "There are no saved workflows yet, in this project or globally. Use SaveWorkflow to create one, or pass an inline `script` instead.";
  }
  return `Available saved workflows: ${tagged.join(", ")}. Use ListSavedWorkflows for their descriptions.`;
}

/** A compact restatement of the parameter declaration, appended after parameter validation failure, so that the model can be corrected in one go rather than having to guess again. */
function describeArgsDeclaration(
  label: string,
  args: SavedWorkflowArgsDeclaration | undefined,
): string {
  const declared = Object.entries(args ?? {});
  if (declared.length === 0) return `${label} declares no arguments.`;
  return [
    `${label} declares:`,
    ...declared.map(([key, spec]) => {
      const notes = [
        spec.type,
        spec.required === true ? "required" : "optional",
        spec.default === undefined ? undefined : `default ${JSON.stringify(spec.default)}`,
      ].filter((note) => note !== undefined);
      return `- ${key} (${notes.join(", ")})${spec.description === undefined ? "" : `: ${spec.description}`}`;
    }),
  ].join("\n");
}
